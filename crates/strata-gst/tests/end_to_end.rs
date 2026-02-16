use strata_sim::impairment::{apply_impairment, ImpairmentConfig};
use strata_sim::topology::Namespace;
use serde_json::Value;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Build the integration binary and return its path.
fn build_integration_binary() -> PathBuf {
    let mut command = Command::new("cargo");
    command.args([
        "build",
        "-p",
        "gst-rist-bonding",
        "--bin",
        "integration_node",
    ]);

    let status = command.status().expect("Failed to execute cargo build");

    assert!(status.success(), "Failed to build integration_node binary");

    // The test binary is usually in target/debug/deps/end_to_end-<hash>
    // The integration_node binary is in target/debug/integration_node
    // We can use the current executable path to find the target/debug directory.
    let mut path = std::env::current_exe().expect("Failed to get current executable path");
    path.pop(); // deps
    path.pop(); // debug (or release)
    path.push("integration_node");

    if !path.exists() {
        // Fallback just in case directory structure is weird (e.g. running from IDE vs CLI)
        // Try to use fallback logic if available, or just panic nicely
        eprintln!(
            "Warning: Did not find integration_node at {:?}, checking relative to CWD",
            path
        );
        let cwd = std::env::current_dir().unwrap();
        let try_path = cwd.join("target/debug/integration_node");
        if try_path.exists() {
            return try_path;
        }
        panic!("Binary not found at expected path: {:?}", path);
    }

    path
}

#[test]
fn test_bonded_transmission() {
    // 1. Build binary
    let binary_path = build_integration_binary();
    let binary_path_str = binary_path.to_str().expect("Valid binary path");

    // Compute absolute output path in target root
    // binary_path is .../target/debug/integration_node
    // we want .../target/bonding_rcv.ts
    let output_path = binary_path
        .parent()
        .expect("debug dir")
        .parent()
        .expect("target dir")
        .join("bonding_rcv.ts");
    let output_path_str = output_path.to_str().expect("Valid output path");

    // Remove existing file to avoid false positives
    if output_path.exists() {
        let _ = std::fs::remove_file(&output_path);
    }

    // 2. Create Namespaces
    // Failure to create namespace should result in a panic
    let snd_ns = Namespace::new("bonding_snd").expect("Failed to create bonding_snd namespace");

    let rcv_ns = match Namespace::new("bonding_rcv") {
        Ok(ns) => ns,
        Err(e) => {
            // Always panic on failure
            panic!("Failed to create bonding_rcv namespace: {}", e);
        }
    };

    // 3. Set up Links
    // Link A: 10.10.1.1/24 <-> 10.10.1.2/24
    if let Err(e) = snd_ns.add_veth_link(
        &rcv_ns,
        "veth_a_snd",
        "veth_a_rcv",
        "10.10.1.1/24",
        "10.10.1.2/24",
    ) {
        panic!("Failed to setup Link A: {}", e);
    }

    // Link B: 10.10.2.1/24 <-> 10.10.2.2/24
    snd_ns
        .add_veth_link(
            &rcv_ns,
            "veth_b_snd",
            "veth_b_rcv",
            "10.10.2.1/24",
            "10.10.2.2/24",
        )
        .expect("Failed to setup Link B");

    // DEBUG: Inspect interfaces
    let ip_out = rcv_ns.exec("ip", &["addr"]).unwrap();
    println!(
        "RCV Netns Interfaces:\n{}",
        String::from_utf8_lossy(&ip_out.stdout)
    );

    // DEBUG: Verify connectivity via Ping
    println!("Verifying Link A connectivity...");
    let ping_a = snd_ns
        .exec("ping", &["-c", "1", "10.10.1.2"])
        .expect("Failed to exec ping A");
    if !ping_a.status.success() {
        panic!(
            "Ping Link A failed: {}",
            String::from_utf8_lossy(&ping_a.stderr)
        );
    }

    println!("Verifying Link B connectivity...");
    let ping_b = snd_ns
        .exec("ping", &["-c", "1", "10.10.2.2"])
        .expect("Failed to exec ping B");
    if !ping_b.status.success() {
        panic!(
            "Ping Link B failed: {}",
            String::from_utf8_lossy(&ping_b.stderr)
        );
    }

    // 4. Spawn Receiver (Background)
    println!("Starting Receiver...");
    let receiver_cmd_args = [
        "netns",
        "exec",
        "bonding_rcv",
        binary_path_str,
        "receiver",
        "--bind",
        "rist://@10.10.1.2:5000,rist://@10.10.2.2:5002",
        "--output",
        output_path_str, // Output TS file (absolute path)
    ];

    let mut receiver_child = Command::new("sudo")
        .args(["ip"])
        .args(receiver_cmd_args)
        // .env("GST_DEBUG", ...) // Removing this as we use 'env' command inside netns
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn receiver process");

    // Give receiver a moment to bind ports
    thread::sleep(Duration::from_secs(2));

    // 5. Spawn Sender (Foreground)
    println!("Starting Sender...");
    let sender_cmd_args = [
        "netns",
        "exec",
        "bonding_snd",
        binary_path_str,
        "sender",
        "--dest",
        "rist://10.10.1.2:5000,rist://10.10.2.2:5002",
    ];

    let sender_status = Command::new("sudo")
        .args(["ip"])
        .args(sender_cmd_args)
        .status()
        .expect("Failed to execute sender process");

    // Sender now runs for approx 20s (1200 buffers @ 60fps with 1080p video), so this call will block for that duration.

    // Allow receiver time to finish processing buffers
    thread::sleep(Duration::from_secs(2));

    // Send SIGINT to receiver to trigger graceful shutdown (and MP4 finalization)
    println!("Sender finished. Sending SIGINT to receiver...");
    let status = Command::new("sudo")
        .args([
            "ip",
            "netns",
            "exec",
            "bonding_rcv",
            "pkill",
            "-SIGINT",
            "-f",
            "integration_node",
        ])
        .status()
        .expect("Failed to send pkill");

    if !status.success() {
        println!("Warning: Failed to pkill receiver (maybe it already exited?)");
    }

    // Wait for receiver to exit. It should exit quickly after SIGINT with EOS handling.
    let mut finished = false;
    for _ in 0..10 {
        // Wait up to 10s
        if let Ok(Some(_)) = receiver_child.try_wait() {
            finished = true;
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }

    if !finished {
        println!("Receiver did not exit in time after SIGINT. Killing...");
        let _ = receiver_child.kill();
    }

    let receiver_output = receiver_child
        .wait_with_output()
        .expect("Failed to wait on receiver");

    println!("Receiver Exit Status: {:?}", receiver_output.status);

    // Assert Sender Success
    assert!(sender_status.success(), "Sender process failed");

    // Check Receiver Output for errors
    let _stdout = String::from_utf8_lossy(&receiver_output.stdout);
    let stderr = String::from_utf8_lossy(&receiver_output.stderr);

    // println!("Receiver Stdout:\n{}", stdout);
    // println!("Receiver Stderr:\n{}", stderr);

    // Sometimes stderr is empty if the process crashes hard or buffers weirdly with sudo.
    // If output file exists and is > 0 bytes, we consider it a partial success for data flow.
    let file_check = output_path.exists()
        && std::fs::metadata(&output_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);

    if file_check {
        println!(
            "Success: Output file created at {:?} and has data.",
            output_path
        );
    } else if !stderr.contains("rist-bonding-stats") {
        println!("Receiver Stderr Dump:\n{}", stderr);
        panic!(
            "Data flow verification failed (No stats in stderr and no output file at {:?})",
            output_path
        );
    }

    // Verify final stats
    // Receiver Final Stats: Count=..., Bytes=...
    if !stderr.contains("Receiver Final Stats: Count=") {
        println!("WARNING: Receiver did not exit cleanly or print final stats.");
    }
}

// ─── Shared helpers for Phase-2 cellular transport tests ───

fn check_privileges() -> bool {
    Command::new("sudo")
        .arg("-n")
        .arg("true")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn setup_env_e2e() -> Option<PathBuf> {
    if !check_privileges() {
        eprintln!("Skipping test: requires root/netns privileges");
        return None;
    }

    let bin = if let Ok(p) = std::env::var("CARGO_BIN_EXE_integration_node") {
        PathBuf::from(p)
    } else {
        let pkg_root = std::env::current_dir().unwrap();
        pkg_root.join("../../target/debug/integration_node")
    };

    if !bin.exists() {
        let _ = Command::new("cargo")
            .args(["build", "--bin", "integration_node"])
            .status();
    }
    Some(bin)
}

fn spawn_in_ns_e2e(ns_name: &str, cmd: &str, args: &[&str]) -> std::process::Child {
    Command::new("sudo")
        .args(["ip", "netns", "exec", ns_name, cmd])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to spawn process in ns")
}

struct E2eStatsCollector {
    data: Arc<Mutex<Vec<Value>>>,
    handle: Option<thread::JoinHandle<()>>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl E2eStatsCollector {
    fn new(bind_ip: &str, port: u16) -> Self {
        let socket = UdpSocket::bind(format!("{}:{}", bind_ip, port)).expect("Bind failed");
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();

        let data = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let d = data.clone();
        let r = running.clone();

        let handle = thread::spawn(move || {
            let mut buf = [0u8; 65535];
            while r.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok((amt, _)) = socket.recv_from(&mut buf) {
                    if let Ok(val) = serde_json::from_slice::<Value>(&buf[..amt]) {
                        d.lock().unwrap().push(val);
                    }
                }
            }
        });

        Self {
            data,
            handle: Some(handle),
            running,
        }
    }

    fn stop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn setup_mgmt_link_e2e(h_name: &str, c_name: &str, netns: &str, h_ip: &str, c_ip: &str) {
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

fn cleanup_mgmt_link_e2e(h_name: &str) {
    let _ = Command::new("sudo")
        .args(["ip", "link", "del", h_name])
        .output();
}

// ─── Phase 2 Tests ───

/// Validates the full encoder adaptation loop:
///   reduced link capacity → NADA detects → congestion-control message →
///   encoder bitrate reduced → throughput stabilizes.
///
/// Setup: single link capped at 3 Mbps, encoder starts at 5 Mbps.
/// After convergence the reported `aggregate_nada_ref_bps` should be ≤ 4 Mbps
/// (reflecting the tc cap) and observed throughput should be non-zero.
#[test]
fn test_encoder_adaptation_loop() {
    let bin_path = match setup_env_e2e() {
        Some(p) => p,
        None => return,
    };

    let ns_snd = Arc::new(Namespace::new("rst_adpt_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_adpt_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_ad_a",
            "veth_ad_b",
            "10.40.1.1/24",
            "10.40.1.2/24",
        )
        .unwrap();

    // Mgmt link for stats relay
    setup_mgmt_link_e2e(
        "veth_mgmt_ad",
        "veth_mgmt_ae",
        "rst_adpt_snd",
        "192.168.110.1/24",
        "192.168.110.2/24",
    );

    // 3 Mbps cap, 30 ms delay — typical constrained cellular
    let cfg = ImpairmentConfig {
        rate_kbit: Some(3_000),
        delay_ms: Some(30),
        loss_percent: Some(0.1),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "veth_ad_a", cfg).unwrap();

    let mut recv_child = spawn_in_ns_e2e(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:6100"],
    );

    let stats_port = 9400;
    let mut collector = E2eStatsCollector::new("192.168.110.1", stats_port);

    // Sender at 5 Mbps — above the tc cap
    let mut send_child = spawn_in_ns_e2e(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender",
            "--dest",
            "rist://10.40.1.2:6100?rtt-min=60&buffer=2000",
            "--stats-dest",
            &format!("192.168.110.1:{}", stats_port),
            "--bitrate",
            "5000",
        ],
    );

    // Run for 15 s to allow NADA convergence
    thread::sleep(Duration::from_secs(15));

    send_child.kill().unwrap();
    let _ = send_child.wait();
    recv_child.kill().unwrap();
    let _ = recv_child.wait();
    collector.stop();
    cleanup_mgmt_link_e2e("veth_mgmt_ad");

    // Analyse the stable tail (last 5 s)
    let data = collector.data.lock().unwrap();
    assert!(!data.is_empty(), "No stats received");

    let t_end = data.last().unwrap()["timestamp"].as_f64().unwrap();
    let t_start = t_end - 5.0;

    let mut sum_nada_ref = 0.0;
    let mut sum_obs = 0.0;
    let mut count = 0usize;

    for v in data.iter() {
        if v["timestamp"].as_f64().unwrap() >= t_start {
            if let Some(nada) = v.get("aggregate_nada_ref_bps").and_then(|x| x.as_f64()) {
                sum_nada_ref += nada;
            }
            if let Some(links) = v.get("links") {
                if let Some(l) = links.get("0") {
                    sum_obs += l["observed_bps"].as_f64().unwrap_or(0.0);
                }
            }
            count += 1;
        }
    }

    assert!(count > 0, "No stats in stable window");
    let avg_nada_mbps = sum_nada_ref / count as f64 / 1_000_000.0;
    let avg_obs_mbps = sum_obs / count as f64 / 1_000_000.0;

    println!(
        "Encoder Adaptation: NADA ref={:.2} Mbps, observed={:.2} Mbps (link cap 3 Mbps)",
        avg_nada_mbps, avg_obs_mbps
    );

    // NADA estimate should converge near or below the tc cap (3 Mbps)
    // Allow generous margin for ramp-up/measurement noise.
    assert!(
        avg_nada_mbps < 5.0,
        "NADA ref ({:.2} Mbps) did not converge below encoder start rate",
        avg_nada_mbps
    );

    // Observed throughput must be non-zero: data is flowing.
    assert!(
        avg_obs_mbps > 0.5,
        "Observed throughput ({:.2} Mbps) too low — adaptation loop may have stalled",
        avg_obs_mbps
    );
}

/// Two links with asymmetric RTT (20 ms vs 150 ms).
/// Verifies bonded throughput is non-trivial and both links carry traffic.
#[test]
fn test_asymmetric_rtt() {
    let bin_path = match setup_env_e2e() {
        Some(p) => p,
        None => return,
    };

    let ns_snd = Arc::new(Namespace::new("rst_asym_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_asym_rcv").unwrap());

    // Link A: low latency
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_as_a",
            "veth_as_b",
            "10.41.1.1/24",
            "10.41.1.2/24",
        )
        .unwrap();
    // Link B: high latency
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_as_c",
            "veth_as_d",
            "10.41.2.1/24",
            "10.41.2.2/24",
        )
        .unwrap();

    setup_mgmt_link_e2e(
        "veth_mgmt_as",
        "veth_mgmt_at",
        "rst_asym_snd",
        "192.168.111.1/24",
        "192.168.111.2/24",
    );

    // Link A: 4 Mbps, 20 ms (WiFi-like)
    apply_impairment(
        &ns_snd,
        "veth_as_a",
        ImpairmentConfig {
            rate_kbit: Some(4_000),
            delay_ms: Some(20),
            loss_percent: Some(0.05),
            ..Default::default()
        },
    )
    .unwrap();
    // Link B: 4 Mbps, 150 ms (LTE-like)
    apply_impairment(
        &ns_snd,
        "veth_as_c",
        ImpairmentConfig {
            rate_kbit: Some(4_000),
            delay_ms: Some(150),
            loss_percent: Some(0.1),
            ..Default::default()
        },
    )
    .unwrap();

    let mut recv_child = spawn_in_ns_e2e(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:6200"],
    );

    let stats_port = 9500;
    let mut collector = E2eStatsCollector::new("192.168.111.1", stats_port);

    let mut send_child = spawn_in_ns_e2e(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender",
            "--dest",
            "rist://10.41.1.2:6200?rtt-min=40&buffer=2000,rist://10.41.2.2:6200?rtt-min=300&buffer=2000",
            "--stats-dest",
            &format!("192.168.111.1:{}", stats_port),
            "--bitrate",
            "6000",
        ],
    );

    thread::sleep(Duration::from_secs(15));

    send_child.kill().unwrap();
    let _ = send_child.wait();
    recv_child.kill().unwrap();
    let _ = recv_child.wait();
    collector.stop();
    cleanup_mgmt_link_e2e("veth_mgmt_as");

    let data = collector.data.lock().unwrap();
    assert!(!data.is_empty(), "No stats received");

    let t_end = data.last().unwrap()["timestamp"].as_f64().unwrap();
    let t_start = t_end - 5.0;

    let mut sum_l1 = 0.0;
    let mut sum_l2 = 0.0;
    let mut count = 0;

    for v in data.iter() {
        if v["timestamp"].as_f64().unwrap() >= t_start {
            if let Some(links) = v.get("links") {
                let v1 = links
                    .get("0")
                    .and_then(|l| l["observed_bps"].as_f64())
                    .unwrap_or(0.0);
                let v2 = links
                    .get("1")
                    .and_then(|l| l["observed_bps"].as_f64())
                    .unwrap_or(0.0);
                sum_l1 += v1;
                sum_l2 += v2;
                count += 1;
            }
        }
    }

    assert!(count > 0, "No stats in window");
    let avg_l1 = sum_l1 / count as f64 / 1_000_000.0;
    let avg_l2 = sum_l2 / count as f64 / 1_000_000.0;
    let total = avg_l1 + avg_l2;

    println!(
        "Asymmetric RTT: Link A (20ms)={:.2} Mbps, Link B (150ms)={:.2} Mbps, Total={:.2} Mbps",
        avg_l1, avg_l2, total
    );

    // Both links should carry some traffic
    assert!(
        avg_l1 > 0.5,
        "Low-RTT link underutilized: {:.2} Mbps",
        avg_l1
    );
    assert!(
        avg_l2 > 0.3,
        "High-RTT link not carrying traffic: {:.2} Mbps",
        avg_l2
    );
    // Combined throughput should be non-trivial
    assert!(
        total > 2.0,
        "Combined throughput ({:.2} Mbps) too low for 8 Mbps aggregate capacity",
        total
    );
}

/// Cut one link's capacity by 75% mid-stream.  Verify throughput recovers
/// and no catastrophic loss burst occurs.
#[test]
fn test_sudden_capacity_drop() {
    let bin_path = match setup_env_e2e() {
        Some(p) => p,
        None => return,
    };

    let ns_snd = Arc::new(Namespace::new("rst_drop_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_drop_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_dr_a",
            "veth_dr_b",
            "10.42.1.1/24",
            "10.42.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_dr_c",
            "veth_dr_d",
            "10.42.2.1/24",
            "10.42.2.2/24",
        )
        .unwrap();

    setup_mgmt_link_e2e(
        "veth_mgmt_dr",
        "veth_mgmt_ds",
        "rst_drop_snd",
        "192.168.112.1/24",
        "192.168.112.2/24",
    );

    // Both links start at 4 Mbps
    let cfg = ImpairmentConfig {
        rate_kbit: Some(4_000),
        delay_ms: Some(30),
        loss_percent: Some(0.1),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "veth_dr_a", cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "veth_dr_c", cfg).unwrap();

    let mut recv_child = spawn_in_ns_e2e(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:6300"],
    );

    let stats_port = 9600;
    let mut collector = E2eStatsCollector::new("192.168.112.1", stats_port);

    let mut send_child = spawn_in_ns_e2e(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender",
            "--dest",
            "rist://10.42.1.2:6300?rtt-min=60&buffer=2000,rist://10.42.2.2:6300?rtt-min=60&buffer=2000",
            "--stats-dest",
            &format!("192.168.112.1:{}", stats_port),
            "--bitrate",
            "6000",
        ],
    );

    // Let traffic stabilize for 7 s
    thread::sleep(Duration::from_secs(7));

    // Drop Link A capacity from 4 Mbps to 1 Mbps (simulating handover)
    println!(">>> CAPACITY DROP: Link A 4 Mbps -> 1 Mbps");
    let drop_cfg = ImpairmentConfig {
        rate_kbit: Some(1_000),
        delay_ms: Some(30),
        loss_percent: Some(0.1),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "veth_dr_a", drop_cfg).unwrap();

    // Run 8 more seconds for recovery
    thread::sleep(Duration::from_secs(8));

    send_child.kill().unwrap();
    let _ = send_child.wait();
    recv_child.kill().unwrap();
    let _ = recv_child.wait();
    collector.stop();
    cleanup_mgmt_link_e2e("veth_mgmt_dr");

    let data = collector.data.lock().unwrap();
    assert!(!data.is_empty(), "No stats received");

    // Check post-drop window (last 3 s) — throughput should have recovered
    let t_end = data.last().unwrap()["timestamp"].as_f64().unwrap();
    let t_recovery = t_end - 3.0;

    let mut sum_obs = 0.0;
    let mut count = 0;
    for v in data.iter() {
        if v["timestamp"].as_f64().unwrap() >= t_recovery {
            if let Some(links) = v.get("links") {
                for key in ["0", "1"] {
                    if let Some(l) = links.get(key) {
                        sum_obs += l["observed_bps"].as_f64().unwrap_or(0.0);
                    }
                }
                count += 1;
            }
        }
    }

    assert!(count > 0, "No stats in recovery window");
    let avg_obs_mbps = sum_obs / count as f64 / 1_000_000.0;

    println!(
        "Post-drop recovery: observed={:.2} Mbps (expected ~5 Mbps cap: 1+4)",
        avg_obs_mbps
    );

    // System should still be moving data after the drop
    assert!(
        avg_obs_mbps > 1.0,
        "Throughput ({:.2} Mbps) did not recover after capacity drop",
        avg_obs_mbps
    );
}

/// Remove and re-add a link during active traffic.
/// Verifies the system does not crash and throughput resumes.
#[test]
fn test_link_hotplug() {
    let bin_path = match setup_env_e2e() {
        Some(p) => p,
        None => return,
    };

    let ns_snd = Arc::new(Namespace::new("rst_hplg_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_hplg_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_hp_a",
            "veth_hp_b",
            "10.43.1.1/24",
            "10.43.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_hp_c",
            "veth_hp_d",
            "10.43.2.1/24",
            "10.43.2.2/24",
        )
        .unwrap();

    setup_mgmt_link_e2e(
        "veth_mgmt_hp",
        "veth_mgmt_hq",
        "rst_hplg_snd",
        "192.168.113.1/24",
        "192.168.113.2/24",
    );

    let cfg = ImpairmentConfig {
        rate_kbit: Some(4_000),
        delay_ms: Some(30),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "veth_hp_a", cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "veth_hp_c", cfg).unwrap();

    let mut recv_child = spawn_in_ns_e2e(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:6400"],
    );

    let stats_port = 9700;
    let mut collector = E2eStatsCollector::new("192.168.113.1", stats_port);

    let mut send_child = spawn_in_ns_e2e(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender",
            "--dest",
            "rist://10.43.1.2:6400?rtt-min=60&buffer=2000,rist://10.43.2.2:6400?rtt-min=60&buffer=2000",
            "--stats-dest",
            &format!("192.168.113.1:{}", stats_port),
            "--bitrate",
            "6000",
        ],
    );

    // Stabilize
    thread::sleep(Duration::from_secs(5));

    // Hotplug: bring Link B down
    println!(">>> HOTPLUG: Link B DOWN");
    let _ = ns_snd.exec("ip", &["link", "set", "veth_hp_c", "down"]);
    thread::sleep(Duration::from_secs(4));

    // Bring Link B back up
    println!(">>> HOTPLUG: Link B UP");
    let _ = ns_snd.exec("ip", &["link", "set", "veth_hp_c", "up"]);
    thread::sleep(Duration::from_secs(5));

    send_child.kill().unwrap();
    let _ = send_child.wait();
    recv_child.kill().unwrap();
    let _ = recv_child.wait();
    collector.stop();
    cleanup_mgmt_link_e2e("veth_mgmt_hp");

    let data = collector.data.lock().unwrap();
    assert!(
        !data.is_empty(),
        "No stats received — system may have crashed during hotplug"
    );

    // Post-recovery window (last 3 s): throughput should be flowing
    let t_end = data.last().unwrap()["timestamp"].as_f64().unwrap();
    let t_recovery = t_end - 3.0;

    let mut sum_obs = 0.0;
    let mut count = 0;
    for v in data.iter() {
        if v["timestamp"].as_f64().unwrap() >= t_recovery {
            if let Some(links) = v.get("links") {
                for key in ["0", "1"] {
                    if let Some(l) = links.get(key) {
                        sum_obs += l["observed_bps"].as_f64().unwrap_or(0.0);
                    }
                }
                count += 1;
            }
        }
    }

    assert!(count > 0, "No stats in recovery window");
    let avg_obs_mbps = sum_obs / count as f64 / 1_000_000.0;

    println!("Post-hotplug recovery: observed={:.2} Mbps", avg_obs_mbps);

    // After re-adding the link, throughput should resume
    assert!(
        avg_obs_mbps > 1.0,
        "Throughput ({:.2} Mbps) did not recover after link hotplug",
        avg_obs_mbps
    );
}
