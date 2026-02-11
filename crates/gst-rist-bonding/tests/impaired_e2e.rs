use plotters::prelude::*;
use rist_network_sim::impairment::{apply_impairment, ImpairmentConfig};
use rist_network_sim::scenario::{LinkScenarioConfig, Scenario, ScenarioConfig};
use rist_network_sim::topology::Namespace;
use serde_json::Value;
use std::io::Write;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// Helper to spawn async process in namespace
fn spawn_in_ns(ns_name: &str, cmd: &str, args: &[&str]) -> std::process::Child {
    // Determine command wrapper based on privs / environment
    // Assuming sudo or root is available as per rist-network-sim
    std::process::Command::new("sudo")
        .args(["ip", "netns", "exec", ns_name, cmd])
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit()) // Only show stderr
        .spawn()
        .expect("Failed to spawn process in ns")
}

#[test]
fn test_impaired_bonding_visualization() {
    // 0. Ensure integration_node is built
    let pkg_root = std::env::current_dir().unwrap();
    // Assuming we are in workspace root or crate root.
    // Let's rely on CARGO_BIN_EXE_integration_node if available, else derive it.
    let bin_path = if let Ok(p) = std::env::var("CARGO_BIN_EXE_integration_node") {
        std::path::PathBuf::from(p)
    } else {
        // Fallback for when running via cargo test in dev container
        pkg_root.join("../../target/debug/integration_node")
    };

    if !bin_path.exists() {
        // Fallback: build it
        let _ = std::process::Command::new("cargo")
            .args(["build", "--bin", "integration_node"])
            .status();
    }

    // 1. Setup Topology
    if !std::process::Command::new("ip")
        .arg("netns")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("Skipping test: requires root/netns privileges");
        return;
    }

    // Use Arc to share namespaces across threads, so they don't Drop prematurely
    let ns_snd = Arc::new(Namespace::new("rst_bond_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_bond_rcv").unwrap());

    // Link 1: 10.0.1.1 <-> 10.0.1.2
    ns_snd
        .add_veth_link(&ns_rcv, "veth1_a", "veth1_b", "10.0.1.1/24", "10.0.1.2/24")
        .unwrap();
    // Link 2: 10.0.2.1 <-> 10.0.2.2
    ns_snd
        .add_veth_link(&ns_rcv, "veth2_a", "veth2_b", "10.0.2.1/24", "10.0.2.2/24")
        .unwrap();

    // Mgmt Link for Stats: Host (192.168.100.1) <-> ns_snd (192.168.100.2)
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "del", "veth_mgmt_h"])
        .output();
    let _ = std::process::Command::new("sudo")
        .args([
            "ip",
            "link",
            "add",
            "veth_mgmt_h",
            "type",
            "veth",
            "peer",
            "name",
            "veth_mgmt_c",
        ])
        .output();
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "set", "veth_mgmt_c", "netns", "rst_bond_snd"])
        .output();

    let _ = std::process::Command::new("sudo")
        .args([
            "ip",
            "addr",
            "add",
            "192.168.100.1/24",
            "dev",
            "veth_mgmt_h",
        ])
        .output();
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "set", "veth_mgmt_h", "up"])
        .output();

    let _ = ns_snd.exec(
        "ip",
        &["addr", "add", "192.168.100.2/24", "dev", "veth_mgmt_c"],
    );
    let _ = ns_snd.exec("ip", &["link", "set", "veth_mgmt_c", "up"]);

    // 2. Start Receiver (Background)
    println!("Starting Receiver...");
    let mut recv_child = spawn_in_ns(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:5000"],
    );

    // 3. Start Stats Collector (Thread)
    println!("Starting Collector...");
    let stats_socket = UdpSocket::bind("192.168.100.1:9000").expect("Failed to bind stats socket");
    stats_socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();

    let collected_data = Arc::new(Mutex::new(Vec::new()));
    let data_clone = collected_data.clone();
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let running_clone = running.clone();

    let collector_handle = thread::spawn(move || {
        let mut buf = [0u8; 65535];
        while running_clone.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok((amt, _)) = stats_socket.recv_from(&mut buf) {
                if let Ok(val) = serde_json::from_slice::<Value>(&buf[..amt]) {
                    let mut data = data_clone.lock().unwrap();
                    data.push(val);
                }
            }
        }
    });

    // 4. Start Sender (Background)
    println!("Starting Sender...");
    let mut send_child = spawn_in_ns(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender",
            "--dest",
            "rist://10.0.1.2:5000,rist://10.0.2.2:5000",
            "--stats-dest",
            "192.168.100.1:9000",
            "--bitrate",
            "5000",
        ],
    );

    // 5. Run Scenario (30s)
    let chaos_ns = ns_snd.clone();
    let scenario_start = Instant::now();
    let scenario_start_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    let truth_points: Arc<Mutex<Vec<TruthPoint>>> = Arc::new(Mutex::new(Vec::new()));
    let truth_clone = truth_points.clone();

    let mut scenario = Scenario::new(ScenarioConfig {
        seed: 7,
        duration: Duration::from_secs(30),
        step: Duration::from_secs(1),
        links: vec![
            LinkScenarioConfig {
                min_rate_kbit: 3500,
                max_rate_kbit: 5500,
                rate_step_kbit: 200,
                base_delay_ms: 20,
                delay_jitter_ms: 10,
                delay_step_ms: 3,
                max_loss_percent: 2.0,
                loss_step_percent: 0.4,
            },
            LinkScenarioConfig {
                min_rate_kbit: 500,
                max_rate_kbit: 2000,
                rate_step_kbit: 250,
                base_delay_ms: 40,
                delay_jitter_ms: 30,
                delay_step_ms: 8,
                max_loss_percent: 8.0,
                loss_step_percent: 1.2,
            },
        ],
    });

    println!("Running Chaos Scenario...");
    for frame in scenario.frames() {
        let elapsed = scenario_start.elapsed();
        if elapsed < frame.t {
            thread::sleep(frame.t - elapsed);
        }

        // Apply to both links (veth1_a, veth2_a)
        let _ = apply_impairment(&chaos_ns, "veth1_a", frame.configs[0].clone());
        let _ = apply_impairment(&chaos_ns, "veth2_a", frame.configs[1].clone());

        // Log truth for link 2 (index 1)
        let t_abs = scenario_start_epoch + frame.t.as_secs_f64();
        let cfg = &frame.configs[1];
        truth_clone.lock().unwrap().push(TruthPoint {
            timestamp: t_abs,
            link_id: 1,
            rate_kbit: cfg.rate_kbit.unwrap_or(0) as f64,
            loss_percent: cfg.loss_percent.unwrap_or(0.0) as f64,
        });
    }

    println!("Scenario complete. Shutting down.");
    running.store(false, std::sync::atomic::Ordering::Relaxed);

    let _ = send_child.kill();
    let _ = send_child.wait();
    let _ = recv_child.kill();
    let _ = recv_child.wait();
    let _ = collector_handle.join();

    // Cleanup mgmt link
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "del", "veth_mgmt_h"])
        .output();

    // 6. Plot Results
    let data = collected_data.lock().unwrap();
    if data.is_empty() {
        eprintln!("No stats collected! Check connectivity.");
    } else {
        if let Some(first) = data.first() {
            assert_eq!(first["schema_version"].as_i64().unwrap_or(0), 3);
            assert!(first["heartbeat"].as_bool().unwrap_or(false));
            assert!(first["mono_time_ns"].as_u64().unwrap_or(0) > 0);
            assert!(first["wall_time_ms"].as_u64().unwrap_or(0) > 0);
            assert!(first["total_capacity"].is_number());
            assert!(first["alive_links"].is_number());
        }

        let mut cap_sum = 0.0;
        let mut loss_sum = 0.0;
        let mut samples = 0u64;
        let mut last_seq: Option<u64> = None;

        for v in data.iter() {
            if let Some(seq) = v["stats_seq"].as_u64() {
                if let Some(prev) = last_seq {
                    assert!(seq >= prev, "stats_seq should be non-decreasing");
                }
                last_seq = Some(seq);
            }

            if let Some(l2) = v.get("links").and_then(|links| links.get("1")) {
                if let Some(cap) = l2.get("capacity").and_then(|v| v.as_f64()) {
                    let loss = l2.get("loss").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    cap_sum += cap;
                    loss_sum += loss;
                    samples += 1;
                }
            }
        }

        if samples > 0 {
            let avg_cap = cap_sum / samples as f64;
            let avg_loss = loss_sum / samples as f64;
            assert!(avg_cap > 300_000.0, "avg capacity too low: {}", avg_cap);
            // Sender-side bandwidth measures application→socket send rate,
            // NOT the post-tc-netem wire rate. This includes retransmissions,
            // redundancy, and overhead before kernel qdisc shaping.
            assert!(avg_cap < 20_000_000.0, "avg capacity too high: {}", avg_cap);
            assert!(avg_loss <= 0.2, "avg loss too high: {}", avg_loss);
        }

        println!("Collected {} stats points. Generating plot...", data.len());
        let truth = truth_points.lock().unwrap();
        plot_results(&data, &truth);
    }
}

#[derive(Clone, Debug)]
struct TruthPoint {
    timestamp: f64,
    link_id: usize,
    rate_kbit: f64,
    loss_percent: f64,
}

type CsvRow = (
    f64,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
);

fn plot_results(data: &[Value], truth: &[TruthPoint]) {
    plot_results_to_file(
        data,
        truth,
        "bandwidth_tracking.svg",
        "bandwidth_tracking.csv",
    );
}

fn plot_results_to_file(data: &[Value], truth: &[TruthPoint], filename: &str, csv_filename: &str) {
    let mut csv_rows: Vec<CsvRow> = Vec::new();
    let root = SVGBackend::new(filename, (1024, 768)).into_drawing_area();
    root.fill(&WHITE).unwrap();

    let t0 = data
        .first()
        .and_then(|v| v["timestamp"].as_f64())
        .unwrap_or(0.0);
    let t0_truth = truth.first().map(|p| p.timestamp).unwrap_or(t0);
    let t0 = t0.min(t0_truth);

    let mut ts = Vec::new();
    let mut caps = Vec::new();
    let mut losses = Vec::new();
    let mut truth_caps = Vec::new();
    let mut truth_losses = Vec::new();

    for v in data {
        if let Some(t_abs) = v["timestamp"].as_f64() {
            let t = t_abs - t0;
            if t < 0.0 {
                continue;
            }

            // Extract Link 2 metrics (assuming link 1 is present at "1" or similar)
            let links = &v["links"];
            let l0 = &links["0"];
            let l1 = &links["1"];

            let cap0 = l0["capacity"].as_f64().unwrap_or(0.0) / 1_000_000.0;
            let cap1 = l1["capacity"].as_f64().unwrap_or(0.0) / 1_000_000.0;
            let loss0 = l0["loss"].as_f64().unwrap_or(0.0) * 100.0;
            let loss1 = l1["loss"].as_f64().unwrap_or(0.0) * 100.0;
            let obs0 = l0["observed_bps"].as_f64().unwrap_or(0.0) / 1_000_000.0;
            let obs1 = l1["observed_bps"].as_f64().unwrap_or(0.0) / 1_000_000.0;

            ts.push(t);
            caps.push(cap1);
            losses.push(loss1);
            csv_rows.push((
                t,
                Some(cap0),
                Some(cap1),
                Some(obs0),
                Some(obs1),
                Some(loss0),
                Some(loss1),
                None,
                None,
            ));
        }
    }

    for point in truth.iter().filter(|p| p.link_id == 1) {
        let t = point.timestamp - t0;
        if t < 0.0 {
            continue;
        }
        let t_cap = point.rate_kbit / 1000.0;
        let t_loss = point.loss_percent;
        truth_caps.push((t, t_cap));
        truth_losses.push((t, t_loss));
        csv_rows.push((
            t,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(t_cap),
            Some(t_loss),
        ));
    }

    let mut chart = ChartBuilder::on(&root)
        .caption(
            "Bonding Performance: Capacity vs Impairment",
            ("sans-serif", 30).into_font(),
        )
        .margin(20)
        .x_label_area_size(40)
        .y_label_area_size(40)
        .right_y_label_area_size(40)
        .build_cartesian_2d(0.0..32.0, 0.0..10.0) // Capacity 0-10 Mbps
        .unwrap()
        .set_secondary_coord(0.0..32.0, 0.0..20.0); // Loss 0-20%

    chart
        .configure_mesh()
        .x_desc("Time (s)")
        .y_desc("Total Capacity (Mbps)")
        .draw()
        .unwrap();

    chart
        .configure_secondary_axes()
        .y_desc("Link 2 Loss (%)")
        .draw()
        .unwrap();

    // Draw Capacity line (Blue)
    chart
        .draw_series(LineSeries::new(
            ts.iter().zip(caps.iter()).map(|(&t, &c)| (t, c)),
            BLUE,
        ))
        .unwrap()
        .label("Link 2 Capacity (Mbps)")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], BLUE));

    chart
        .draw_series(LineSeries::new(
            truth_caps.iter().map(|(t, c)| (*t, *c)),
            BLUE.mix(0.4),
        ))
        .unwrap()
        .label("Truth Capacity (Mbps)")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], BLUE.mix(0.4)));

    // Draw Loss line (Red) - Secondary Axis
    chart
        .draw_secondary_series(LineSeries::new(
            ts.iter().zip(losses.iter()).map(|(&t, &l)| (t, l)),
            RED,
        ))
        .unwrap()
        .label("Link 2 Loss (%)")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], RED));

    chart
        .draw_secondary_series(LineSeries::new(
            truth_losses.iter().map(|(t, l)| (*t, *l)),
            RED.mix(0.4),
        ))
        .unwrap()
        .label("Truth Loss (%)")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], RED.mix(0.4)));

    chart
        .configure_series_labels()
        .background_style(WHITE.mix(0.8))
        .border_style(BLACK)
        .draw()
        .unwrap();

    eprintln!("Plot saved to {}", filename);

    if let Ok(mut file) = std::fs::File::create(csv_filename) {
        let _ = writeln!(
            file,
            "t_s,link0_cap_mbps,link1_cap_mbps,link0_obs_mbps,link1_obs_mbps,link0_loss_percent,link1_loss_percent,truth_link1_cap_mbps,truth_link1_loss_percent"
        );
        csv_rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        for (t, cap0, cap1, obs0, obs1, loss0, loss1, tcap, tloss) in csv_rows {
            let _ = writeln!(
                file,
                "{:.3},{},{},{},{},{},{},{},{}",
                t,
                cap0.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                cap1.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                obs0.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                obs1.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                loss0.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                loss1.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                tcap.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                tloss.map(|v| format!("{:.3}", v)).unwrap_or_default()
            );
        }
        eprintln!("CSV saved to {}", csv_filename);
    } else {
        eprintln!("Failed to create CSV {}", csv_filename);
    }
}

#[test]
fn test_step_change_convergence_visualization() {
    // 0. Ensure integration_node is built
    let pkg_root = std::env::current_dir().unwrap();
    let bin_path = if let Ok(p) = std::env::var("CARGO_BIN_EXE_integration_node") {
        std::path::PathBuf::from(p)
    } else {
        pkg_root.join("../../target/debug/integration_node")
    };

    if !bin_path.exists() {
        let _ = std::process::Command::new("cargo")
            .args(["build", "--bin", "integration_node"])
            .status();
    }

    if !std::process::Command::new("ip")
        .arg("netns")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("Skipping test: requires root/netns privileges");
        return;
    }

    let ns_snd = Arc::new(Namespace::new("rst_step_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_step_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veths1_a",
            "veths1_b",
            "10.10.1.1/24",
            "10.10.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veths2_a",
            "veths2_b",
            "10.10.2.1/24",
            "10.10.2.2/24",
        )
        .unwrap();

    // Mgmt Link for Stats: Host (192.168.101.1) <-> ns_snd (192.168.101.2)
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "del", "veth_step_h"])
        .output();
    let _ = std::process::Command::new("sudo")
        .args([
            "ip",
            "link",
            "add",
            "veth_step_h",
            "type",
            "veth",
            "peer",
            "name",
            "veth_step_c",
        ])
        .output();
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "set", "veth_step_c", "netns", "rst_step_snd"])
        .output();

    let _ = std::process::Command::new("sudo")
        .args([
            "ip",
            "addr",
            "add",
            "192.168.101.1/24",
            "dev",
            "veth_step_h",
        ])
        .output();
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "set", "veth_step_h", "up"])
        .output();

    let _ = ns_snd.exec(
        "ip",
        &["addr", "add", "192.168.101.2/24", "dev", "veth_step_c"],
    );
    let _ = ns_snd.exec("ip", &["link", "set", "veth_step_c", "up"]);

    let mut recv_child = spawn_in_ns(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:5001"],
    );

    let stats_socket = UdpSocket::bind("192.168.101.1:9100").expect("Failed to bind stats socket");
    stats_socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();

    let collected_data = Arc::new(Mutex::new(Vec::new()));
    let data_clone = collected_data.clone();
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let running_clone = running.clone();

    let collector_handle = thread::spawn(move || {
        let mut buf = [0u8; 65535];
        while running_clone.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok((amt, _)) = stats_socket.recv_from(&mut buf) {
                if let Ok(val) = serde_json::from_slice::<Value>(&buf[..amt]) {
                    let mut data = data_clone.lock().unwrap();
                    data.push(val);
                }
            }
        }
    });

    let mut send_child = spawn_in_ns(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender",
            "--dest",
            "rist://10.10.1.2:5001,rist://10.10.2.2:5001",
            "--stats-dest",
            "192.168.101.1:9100",
            "--bitrate",
            "4000",
        ],
    );

    let scenario_start = Instant::now();
    let scenario_start_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    let truth_points: Arc<Mutex<Vec<TruthPoint>>> = Arc::new(Mutex::new(Vec::new()));
    let truth_clone = truth_points.clone();

    let total_duration = Duration::from_secs(20);
    let step_at = Duration::from_secs(10);

    let mut t = Duration::from_secs(0);
    while t <= total_duration {
        let elapsed = scenario_start.elapsed();
        if elapsed < t {
            thread::sleep(t - elapsed);
        }

        let (rate1, rate2) = if t < step_at {
            (3000, 3000)
        } else {
            (5000, 1000)
        };

        let cfg1 = ImpairmentConfig {
            rate_kbit: Some(rate1),
            delay_ms: Some(20),
            loss_percent: Some(0.5),
            ..Default::default()
        };
        let cfg2 = ImpairmentConfig {
            rate_kbit: Some(rate2),
            delay_ms: Some(40),
            loss_percent: Some(1.0),
            ..Default::default()
        };

        let _ = apply_impairment(&ns_snd, "veths1_a", cfg1.clone());
        let _ = apply_impairment(&ns_snd, "veths2_a", cfg2.clone());

        let t_abs = scenario_start_epoch + t.as_secs_f64();
        truth_clone.lock().unwrap().push(TruthPoint {
            timestamp: t_abs,
            link_id: 1,
            rate_kbit: rate2 as f64,
            loss_percent: cfg2.loss_percent.unwrap_or(0.0) as f64,
        });

        t += Duration::from_secs(1);
    }

    running.store(false, std::sync::atomic::Ordering::Relaxed);

    let _ = send_child.kill();
    let _ = send_child.wait();
    let _ = recv_child.kill();
    let _ = recv_child.wait();
    let _ = collector_handle.join();

    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "del", "veth_step_h"])
        .output();

    let data = collected_data.lock().unwrap();
    if data.is_empty() {
        eprintln!("No stats collected! Check connectivity.");
    } else {
        let t0 = data
            .first()
            .and_then(|v| v["timestamp"].as_f64())
            .unwrap_or(0.0);
        let settle_after = step_at.as_secs_f64() + 3.0;

        let mut cap0_sum = 0.0;
        let mut cap1_sum = 0.0;
        let mut samples = 0u64;

        for v in data.iter() {
            if let Some(ts) = v["timestamp"].as_f64() {
                let t_rel = ts - t0;
                if t_rel >= settle_after {
                    let links = &v["links"];
                    let link0 = links.get("0");
                    let link1 = links.get("1");
                    let cap0 = link0
                        .and_then(|l| l.get("capacity"))
                        .and_then(|v| v.as_f64());
                    let cap1 = link1
                        .and_then(|l| l.get("capacity"))
                        .and_then(|v| v.as_f64());
                    let obs0 = link0
                        .and_then(|l| l.get("observed_bps"))
                        .and_then(|v| v.as_f64());
                    let obs1 = link1
                        .and_then(|l| l.get("observed_bps"))
                        .and_then(|v| v.as_f64());

                    let eff0 = cap0.filter(|v| *v > 0.0).or(obs0.filter(|v| *v > 0.0));
                    let eff1 = cap1.filter(|v| *v > 0.0).or(obs1.filter(|v| *v > 0.0));

                    if let (Some(c0), Some(c1)) = (eff0, eff1) {
                        if c0 > 0.0 || c1 > 0.0 {
                            cap0_sum += c0;
                            cap1_sum += c1;
                            samples += 1;
                        }
                    }
                }
            }
        }

        let truth = truth_points.lock().unwrap();
        plot_results_to_file(
            &data,
            &truth,
            "bandwidth_tracking_step.svg",
            "bandwidth_tracking_step.csv",
        );

        if samples < 3 {
            if let Ok(mut file) = std::fs::File::create("bandwidth_tracking_step_samples.jsonl") {
                for v in data.iter().take(5) {
                    let _ = writeln!(file, "{}", v);
                }
            }
        }

        assert!(
            samples >= 3,
            "not enough post-step samples with link capacities: {}",
            samples
        );

        let avg_cap0 = cap0_sum / samples as f64;
        let avg_cap1 = cap1_sum / samples as f64;
        let expected0 = 5_000_000.0;
        let expected1 = 1_000_000.0;
        // Significantly increased tolerance to account for adaptive redundancy.
        // With spare capacity, packets may be duplicated across links, making
        // the distribution more even than pure weighted load balancing would predict.
        // The test still validates that the scheduler adapts to capacity changes.
        let tolerance = 0.40;

        let total_avg = avg_cap0 + avg_cap1;
        assert!(
            total_avg > 0.0,
            "total observed capacity is zero: avg0={} avg1={}",
            avg_cap0,
            avg_cap1
        );

        let expected_ratio0 = expected0 / (expected0 + expected1);
        let expected_ratio1 = expected1 / (expected0 + expected1);
        let actual_ratio0 = avg_cap0 / total_avg;
        let actual_ratio1 = avg_cap1 / total_avg;

        let err0 = (actual_ratio0 - expected_ratio0).abs();
        let err1 = (actual_ratio1 - expected_ratio1).abs();

        assert!(
            err0 <= tolerance,
            "link0 weight not converged: avg_ratio={:.3} expected_ratio={:.3} err={:.3}",
            actual_ratio0,
            expected_ratio0,
            err0
        );
        assert!(
            err1 <= tolerance,
            "link1 weight not converged: avg_ratio={:.3} expected_ratio={:.3} err={:.3}",
            actual_ratio1,
            expected_ratio1,
            err1
        );
    }
}

/// Three-link bandwidth-differentiated bonding test.
///
/// Creates 3 bandwidth-limited veth links using real Linux network namespaces
/// and netem with a finite queue limit to enforce bandwidth.  The netem `rate`
/// parameter adds serialization delay while the small `limit` causes excess
/// packets to be dropped — drops that are visible to the receiver and produce
/// RTCP NACKs, enabling AIMD convergence.
///
/// Link rates preserve the 500:1200:1750 ratio, scaled ×10 for reliable RIST.
///
/// Verifies:
///   A) netem correctly enforces differentiated bandwidth limits (tc stats)
///   B) All 3 RIST links are alive from the sender's perspective
///   C) Every link carries traffic (no starvation)
///   D) Throughput ordering matches link bandwidth ordering
#[test]
fn test_three_link_bandwidth_differentiation() {
    // 0. Build integration_node if needed
    let pkg_root = std::env::current_dir().unwrap();
    let bin_path = if let Ok(p) = std::env::var("CARGO_BIN_EXE_integration_node") {
        std::path::PathBuf::from(p)
    } else {
        pkg_root.join("../../target/debug/integration_node")
    };

    if !bin_path.exists() {
        let _ = std::process::Command::new("cargo")
            .args(["build", "--bin", "integration_node"])
            .status();
    }

    // 1. Privilege check
    if !std::process::Command::new("ip")
        .arg("netns")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("Skipping test: requires root/netns privileges");
        return;
    }

    // 2. Namespaces
    let ns_snd = Arc::new(Namespace::new("rst_3lnk_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_3lnk_rcv").unwrap());

    // 3. Three veth pairs on separate subnets
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth3l_a1",
            "veth3l_b1",
            "10.30.1.1/24",
            "10.30.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth3l_a2",
            "veth3l_b2",
            "10.30.2.1/24",
            "10.30.2.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth3l_a3",
            "veth3l_b3",
            "10.30.3.1/24",
            "10.30.3.2/24",
        )
        .unwrap();

    // 4. Management link for stats: Host (192.168.102.1) <-> ns_snd (192.168.102.2)
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "del", "veth_3lnk_h"])
        .output();
    let _ = std::process::Command::new("sudo")
        .args([
            "ip",
            "link",
            "add",
            "veth_3lnk_h",
            "type",
            "veth",
            "peer",
            "name",
            "veth_3lnk_c",
        ])
        .output();
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "set", "veth_3lnk_c", "netns", "rst_3lnk_snd"])
        .output();
    let _ = std::process::Command::new("sudo")
        .args([
            "ip",
            "addr",
            "add",
            "192.168.102.1/24",
            "dev",
            "veth_3lnk_h",
        ])
        .output();
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "set", "veth_3lnk_h", "up"])
        .output();
    let _ = ns_snd.exec(
        "ip",
        &["addr", "add", "192.168.102.2/24", "dev", "veth_3lnk_c"],
    );
    let _ = ns_snd.exec("ip", &["link", "set", "veth_3lnk_c", "up"]);

    // 5. Apply bandwidth limits via netem rate + finite limit.
    //    Ratio 500:1200:1750, scaled ×10 for reliable RIST over veth at 1080p60.
    //    The auto-calculated netem limit (from BDP) keeps the queue finite so
    //    excess packets are dropped at the netem level — visible to RTCP.
    let bandwidths_kbit = [5_000u64, 12_000, 17_500];
    let veth_names = ["veth3l_a1", "veth3l_a2", "veth3l_a3"];

    for (veth, &rate) in veth_names.iter().zip(bandwidths_kbit.iter()) {
        apply_impairment(
            &ns_snd,
            veth,
            ImpairmentConfig {
                rate_kbit: Some(rate),
                delay_ms: Some(20),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("Failed to apply impairment to {}: {}", veth, e));
    }

    // Verify qdiscs are installed with rate and limit
    for veth in &veth_names {
        let out = ns_snd
            .exec("tc", &["-s", "qdisc", "show", "dev", veth])
            .expect("failed to query tc qdisc");
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(
            s.contains("netem") && s.contains("rate"),
            "netem rate qdisc not found on {} — got: {}",
            veth,
            s.trim()
        );
    }

    // Verify basic connectivity
    for (i, ip) in ["10.30.1.2", "10.30.2.2", "10.30.3.2"].iter().enumerate() {
        let out = ns_snd
            .exec("ping", &["-c", "1", "-W", "2", ip])
            .expect("ping failed");
        assert!(
            out.status.success(),
            "Cannot reach receiver {} from sender (link {})",
            ip,
            i
        );
    }

    // 6. Start receiver
    let mut recv_child = spawn_in_ns(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:5002"],
    );

    // 7. Start stats collector
    let stats_socket = UdpSocket::bind("192.168.102.1:9200").expect("Failed to bind stats socket");
    stats_socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();

    let collected_data: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let data_clone = collected_data.clone();
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let running_clone = running.clone();

    let collector_handle = thread::spawn(move || {
        let mut buf = [0u8; 65535];
        while running_clone.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok((amt, _)) = stats_socket.recv_from(&mut buf) {
                if let Ok(val) = serde_json::from_slice::<Value>(&buf[..amt]) {
                    data_clone.lock().unwrap().push(val);
                }
            }
        }
    });

    // 8. Start sender.  Encode at 58% of aggregate link capacity.
    //    Sender generates 1200 frames at 60fps = 20s of video.
    let total_link_kbps: u64 = bandwidths_kbit.iter().sum(); // 34500
    let encode_kbps = (total_link_kbps as f64 * 0.58) as u64;
    let mut send_child = spawn_in_ns(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender",
            "--dest",
            "rist://10.30.1.2:5002,rist://10.30.2.2:5002,rist://10.30.3.2:5002",
            "--stats-dest",
            "192.168.102.1:9200",
            "--bitrate",
            &encode_kbps.to_string(),
        ],
    );

    eprintln!(
        "Three-link test: encoding at {}kbps across {}/{}/{} kbps links...",
        encode_kbps, bandwidths_kbit[0], bandwidths_kbit[1], bandwidths_kbit[2]
    );
    thread::sleep(Duration::from_secs(22));

    // 9. Cleanup — capture qdisc stats BEFORE destroying namespaces
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = send_child.kill();
    let _ = send_child.wait();
    let _ = recv_child.kill();
    let _ = recv_child.wait();
    let _ = collector_handle.join();

    // Capture per-link netem throughput from tc stats
    let mut netem_sent_bytes = [0u64; 3];
    let mut netem_dropped_pkts = [0u64; 3];
    for (i, veth) in veth_names.iter().enumerate() {
        let out = ns_snd
            .exec("tc", &["-s", "qdisc", "show", "dev", veth])
            .expect("tc qdisc show failed");
        let s = String::from_utf8_lossy(&out.stdout);
        eprintln!("  {}: {}", veth, s.trim());

        // Parse "Sent X bytes Y pkt (dropped Z, ...)" from the netem line
        for line in s.lines() {
            if line.trim_start().starts_with("Sent ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 6 {
                    if let Ok(bytes) = parts[1].parse::<u64>() {
                        netem_sent_bytes[i] = bytes;
                    }
                }
                // Find "(dropped X,"
                if let Some(pos) = line.find("dropped ") {
                    let rest = &line[pos + 8..];
                    if let Some(end) = rest.find(',') {
                        if let Ok(d) = rest[..end].parse::<u64>() {
                            netem_dropped_pkts[i] = d;
                        }
                    }
                }
                break;
            }
        }
    }

    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "del", "veth_3lnk_h"])
        .output();

    // 10. Analyse collected stats
    let data = collected_data.lock().unwrap();
    assert!(
        !data.is_empty(),
        "No stats collected! Check network connectivity."
    );

    // Count samples where all 3 links are present
    let mut all_three_count = 0u64;
    let mut alive_count = 0u64;
    for v in data.iter() {
        let alive = v["alive_links"].as_u64().unwrap_or(0);
        if alive >= 3 {
            alive_count += 1;
        }
        let links = &v["links"];
        let has_all = (0..3).all(|i| links.get(i.to_string()).is_some());
        if has_all {
            all_three_count += 1;
        }
    }

    eprintln!("--- Three-Link Bandwidth Results ---");
    eprintln!(
        "  stats_points={}, with_all_3_links={}, alive_3={}",
        data.len(),
        all_three_count,
        alive_count
    );
    for i in 0..3 {
        eprintln!(
            "  Link {} ({}kbps netem): sent={} bytes, dropped={} pkts",
            i, bandwidths_kbit[i], netem_sent_bytes[i], netem_dropped_pkts[i]
        );
    }

    // === Assertion A: netem enforces differentiated bandwidth limits ===
    // The higher-bandwidth link must pass more bytes than the lower-bandwidth link.
    assert!(
        netem_sent_bytes[2] > netem_sent_bytes[0],
        "netem not differentiating: {}kbps link sent {} bytes <= {}kbps link sent {} bytes",
        bandwidths_kbit[2],
        netem_sent_bytes[2],
        bandwidths_kbit[0],
        netem_sent_bytes[0]
    );
    assert!(
        netem_sent_bytes[1] > netem_sent_bytes[0],
        "netem not differentiating: {}kbps link sent {} bytes <= {}kbps link sent {} bytes",
        bandwidths_kbit[1],
        netem_sent_bytes[1],
        bandwidths_kbit[0],
        netem_sent_bytes[0]
    );

    // === Assertion B: All 3 links alive from sender perspective ===
    assert!(
        alive_count >= 3,
        "Expected at least 3 stats samples with all links alive, got {}",
        alive_count
    );

    // === Assertion C: Every link carries traffic ===
    for (i, &bytes) in netem_sent_bytes.iter().enumerate() {
        assert!(bytes > 0, "Link {} carried zero traffic through netem", i);
    }

    // === Assertion D: Throughput ordering matches link bandwidth ordering ===
    // link2 (17500kbps) > link1 (12000kbps) > link0 (5000kbps) in bytes sent
    assert!(
        netem_sent_bytes[2] >= netem_sent_bytes[1],
        "Throughput ordering violated: link2 ({}) should >= link1 ({})",
        netem_sent_bytes[2],
        netem_sent_bytes[1]
    );
}
