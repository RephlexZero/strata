use rist_network_sim::impairment::{apply_impairment, ImpairmentConfig};
use rist_network_sim::scenario::{LinkScenarioConfig, Scenario, ScenarioConfig};
use rist_network_sim::topology::Namespace;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn check_privileges() -> bool {
    // Check if we can run sudo without password
    match Command::new("sudo").arg("-n").arg("true").status() {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

#[test]
fn test_race_car_scenarios() {
    if !check_privileges() {
        eprintln!("Skipping test_race_car_scenarios (needs sudo)");
        return;
    }

    // 1. Setup Namespaces
    let sender_ns = Arc::new(Namespace::new("rst_race_snd").expect("Failed to create sender ns"));
    let receiver_ns =
        Arc::new(Namespace::new("rst_race_rcv").expect("Failed to create receiver ns"));

    // Link 1: 10.0.30.x (Main High Speed Link)
    sender_ns
        .add_veth_link(
            &receiver_ns,
            "race_veth1_a",
            "race_veth1_b",
            "10.0.30.1/24",
            "10.0.30.2/24",
        )
        .expect("Failed to create link 1");

    // Link 2: 10.0.40.x (Backup / Control Link)
    sender_ns
        .add_veth_link(
            &receiver_ns,
            "race_veth2_a",
            "race_veth2_b",
            "10.0.40.1/24",
            "10.0.40.2/24",
        )
        .expect("Failed to create link 2");

    // Initial State: Clean
    let clean_config_1 = ImpairmentConfig {
        rate_kbit: Some(5000), // 5Mbps
        delay_ms: Some(20),
        ..Default::default()
    };
    let clean_config_2 = ImpairmentConfig {
        rate_kbit: Some(1000), // 1Mbps
        delay_ms: Some(50),
        ..Default::default()
    };

    apply_impairment(&sender_ns, "race_veth1_a", clean_config_1.clone()).unwrap();
    apply_impairment(&sender_ns, "race_veth2_a", clean_config_2.clone()).unwrap();

    // 2. Start Receiver
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_integration_node"));

    let receiver_ns_clone = receiver_ns.clone();
    let exec_recv = executable.clone();

    let _receiver_handle = thread::spawn(move || {
        let output = receiver_ns_clone
            .exec(
                exec_recv.to_str().unwrap(),
                &[
                    "receiver",
                    "--bind",
                    "rist://10.0.30.2:1234,rist://10.0.40.2:1235",
                ],
            )
            .expect("Failed to run receiver");
        println!(
            "Receiver Output: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    // 3. Start Sender
    let sender_ns_clone = sender_ns.clone();
    let exec_send = executable.clone();

    let sender_handle = thread::spawn(move || {
        thread::sleep(Duration::from_secs(1));
        let output = sender_ns_clone
            .exec(
                exec_send.to_str().unwrap(),
                &[
                    "sender",
                    "--dest",
                    "rist://10.0.30.2:1234,rist://10.0.40.2:1235",
                    "--bitrate",
                    "4000", // 4Mbps target.
                ],
            )
            .expect("Failed to run sender");
        // We print stdout/stderr to inspect later
        println!(
            "Sender Output Log:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    });

    // 4. Simulation Logic (Main Thread)
    println!(">>> SIMULATION: Started. Seeded dynamic scenario.");
    let scenario_start = Instant::now();
    let mut flapped = false;
    let mut scenario = Scenario::new(ScenarioConfig {
        seed: 99,
        duration: Duration::from_secs(35),
        step: Duration::from_secs(2),
        links: vec![
            LinkScenarioConfig {
                min_rate_kbit: 2500,
                max_rate_kbit: 6000,
                rate_step_kbit: 400,
                base_delay_ms: 20,
                delay_jitter_ms: 40,
                delay_step_ms: 6,
                max_loss_percent: 15.0,
                loss_step_percent: 2.0,
            },
            LinkScenarioConfig {
                min_rate_kbit: 500,
                max_rate_kbit: 2000,
                rate_step_kbit: 200,
                base_delay_ms: 40,
                delay_jitter_ms: 40,
                delay_step_ms: 8,
                max_loss_percent: 10.0,
                loss_step_percent: 1.5,
            },
        ],
    });

    for frame in scenario.frames() {
        let elapsed = scenario_start.elapsed();
        if elapsed < frame.t {
            thread::sleep(frame.t - elapsed);
        }

        let _ = apply_impairment(&sender_ns, "race_veth1_a", frame.configs[0].clone());
        let _ = apply_impairment(&sender_ns, "race_veth2_a", frame.configs[1].clone());

        if !flapped && frame.t.as_secs_f64() >= 15.0 {
            flapped = true;
            println!(">>> CHAOS: Flapping race_veth2_a down/up");
            let _ = sender_ns.exec("ip", &["link", "set", "race_veth2_a", "down"]);
            thread::sleep(Duration::from_secs(2));
            let _ = sender_ns.exec("ip", &["link", "set", "race_veth2_a", "up"]);
        }

        println!(
            ">>> IMPAIRMENT t={:.1}s link1 rate={}kbps loss={:.1}% link2 rate={}kbps loss={:.1}%",
            frame.t.as_secs_f64(),
            frame.configs[0].rate_kbit.unwrap_or(0),
            frame.configs[0].loss_percent.unwrap_or(0.0),
            frame.configs[1].rate_kbit.unwrap_or(0),
            frame.configs[1].loss_percent.unwrap_or(0.0)
        );
    }

    // Wait for Sender to finish (Now using 1200 buffers @ 60fps = 20s of video)
    // The test runs for 35s, so sender will finish before the test ends
    println!(">>> SIMULATION: End. Cleaning up.");

    // --- Assertions ---

    // 1. The scenario reached the link-flap phase (t >= 15s)
    assert!(
        flapped,
        "Simulation should have reached the link-flap phase at t=15s"
    );

    // 2. The sender thread completed without panicking
    let sender_output = sender_handle
        .join()
        .expect("Sender thread panicked during impaired scenario");

    // 3. The sender process exited successfully
    assert!(
        sender_output.status.success(),
        "Sender process should exit cleanly. Exit code: {:?}, stderr: {}",
        sender_output.status.code(),
        String::from_utf8_lossy(&sender_output.stderr)
    );

    // 4. The sender produced output (wrote data to links)
    let stderr_str = String::from_utf8_lossy(&sender_output.stderr);
    assert!(
        !stderr_str.is_empty(),
        "Sender should produce log output on stderr"
    );

    // Dropping namespaces will kill remaining processes (receiver).
}
