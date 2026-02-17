use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use strata_sim::impairment::{apply_impairment, ImpairmentConfig};
use strata_sim::scenario::{LinkScenarioConfig, Scenario, ScenarioConfig};
use strata_sim::topology::Namespace;

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
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_strata-node"));

    let receiver_ns_clone = receiver_ns.clone();
    let exec_recv = executable.clone();

    let _receiver_handle = thread::spawn(move || {
        let output = receiver_ns_clone
            .exec(
                exec_recv.to_str().unwrap(),
                &[
                    "receiver",
                    "--bind",
                    "10.0.30.2:1234,10.0.40.2:1235",
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
                    "10.0.30.2:1234,10.0.40.2:1235",
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

/// Long-running stability test: 60s at moderate utilization.
/// Verifies the coefficient of variation (CV) of throughput stays below 25%,
/// confirming no drift, oscillation, or resource leak.
///
/// Uses a single 5 Mbps link with 30 ms delay, encoder at 4 Mbps (~80% utilization).
#[test]
fn test_long_running_stability() {
    if !check_privileges() {
        eprintln!("Skipping test_long_running_stability (needs sudo)");
        return;
    }

    let sender_ns = Arc::new(Namespace::new("rst_stab_snd").expect("Failed to create sender ns"));
    let receiver_ns =
        Arc::new(Namespace::new("rst_stab_rcv").expect("Failed to create receiver ns"));

    sender_ns
        .add_veth_link(
            &receiver_ns,
            "stab_veth_a",
            "stab_veth_b",
            "10.0.50.1/24",
            "10.0.50.2/24",
        )
        .expect("Failed to create link");

    let cfg = ImpairmentConfig {
        rate_kbit: Some(5000),
        delay_ms: Some(30),
        loss_percent: Some(0.05),
        ..Default::default()
    };
    apply_impairment(&sender_ns, "stab_veth_a", cfg).unwrap();

    // Management link for stats
    setup_stability_mgmt(
        "veth_mgmt_st",
        "veth_mgmt_su",
        "rst_stab_snd",
        "192.168.115.1/24",
        "192.168.115.2/24",
    );

    let executable = PathBuf::from(env!("CARGO_BIN_EXE_strata-node"));

    let recv_ns_clone = receiver_ns.clone();
    let exec_recv = executable.clone();
    let _receiver_handle = thread::spawn(move || {
        let output = recv_ns_clone
            .exec(
                exec_recv.to_str().unwrap(),
                &["receiver", "--bind", "10.0.50.2:1240"],
            )
            .expect("Failed to run receiver");
        eprintln!(
            "Receiver Output: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    let stats_port = 9900;
    let stats_sock =
        std::net::UdpSocket::bind(format!("192.168.115.1:{}", stats_port)).expect("Bind failed");
    stats_sock
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    let stats_data: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let stats_running = Arc::new(std::sync::atomic::AtomicBool::new(true));

    let sd = stats_data.clone();
    let sr = stats_running.clone();
    let stats_handle = thread::spawn(move || {
        let mut buf = [0u8; 65535];
        while sr.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok((amt, _)) = stats_sock.recv_from(&mut buf) {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&buf[..amt]) {
                    // Sum observed_bps across all links
                    if let Some(links) = val.get("links") {
                        let mut total_obs = 0.0;
                        if let Some(obj) = links.as_object() {
                            for (_k, l) in obj {
                                total_obs += l["observed_bps"].as_f64().unwrap_or(0.0);
                            }
                        }
                        if total_obs > 0.0 {
                            sd.lock().unwrap().push(total_obs);
                        }
                    }
                }
            }
        }
    });

    // Sender at 4 Mbps (~80% of 5 Mbps cap) — 3600 frames @ 60fps = 60s
    let sender_ns_clone = sender_ns.clone();
    let exec_send = executable.clone();
    let sender_handle = thread::spawn(move || {
        thread::sleep(Duration::from_secs(1));
        sender_ns_clone
            .exec(
                exec_send.to_str().unwrap(),
                &[
                    "sender",
                    "--dest",
                    "10.0.50.2:1240?rtt-min=60&buffer=2000",
                    "--stats-dest",
                    &format!("192.168.115.1:{}", stats_port),
                    "--bitrate",
                    "4000",
                ],
            )
            .expect("Failed to run sender")
    });

    // Wait for sender to finish (60s of video + margin)
    let sender_output = sender_handle
        .join()
        .expect("Sender thread panicked during long-running test");

    // Stop stats collection
    stats_running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = stats_handle.join();
    cleanup_stability_mgmt("veth_mgmt_st");

    assert!(
        sender_output.status.success(),
        "Sender process failed during long-running test"
    );

    let samples = stats_data.lock().unwrap();
    println!(
        "Long-running stability: collected {} throughput samples",
        samples.len()
    );
    assert!(
        samples.len() >= 10,
        "Too few samples ({}) for stability analysis",
        samples.len()
    );

    // Skip first 20% of samples (warm-up)
    let skip = samples.len() / 5;
    let stable: Vec<f64> = samples[skip..].to_vec();

    let n = stable.len() as f64;
    let mean = stable.iter().sum::<f64>() / n;
    let variance = stable.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let stddev = variance.sqrt();
    let cv = if mean > 0.0 { stddev / mean } else { 1.0 };

    println!(
        "Stability: mean={:.0} bps, stddev={:.0} bps, CV={:.3} ({} samples after warm-up)",
        mean,
        stddev,
        cv,
        stable.len()
    );

    assert!(
        cv < 0.25,
        "Throughput CV ({:.3}) exceeds 25% — system may be oscillating or drifting",
        cv
    );
}

fn setup_stability_mgmt(h_name: &str, c_name: &str, netns: &str, h_ip: &str, c_ip: &str) {
    let _ = Command::new("sudo")
        .args(["ip", "link", "del", h_name])
        .output();
    let _ = Command::new("sudo")
        .args([
            "ip", "link", "add", h_name, "type", "veth", "peer", "name", c_name,
        ])
        .output();
    let _ = Command::new("sudo")
        .args(["ip", "link", "set", c_name, "netns", netns])
        .output();
    let _ = Command::new("sudo")
        .args(["ip", "addr", "add", h_ip, "dev", h_name])
        .output();
    let _ = Command::new("sudo")
        .args(["ip", "link", "set", h_name, "up"])
        .output();
    let _ = Command::new("sudo")
        .args([
            "ip", "netns", "exec", netns, "ip", "addr", "add", c_ip, "dev", c_name,
        ])
        .output();
    let _ = Command::new("sudo")
        .args([
            "ip", "netns", "exec", netns, "ip", "link", "set", c_name, "up",
        ])
        .output();
}

fn cleanup_stability_mgmt(h_name: &str) {
    let _ = Command::new("sudo")
        .args(["ip", "link", "del", h_name])
        .output();
}
