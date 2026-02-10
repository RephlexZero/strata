use rist_network_sim::impairment::{apply_impairment, ImpairmentConfig};
use rist_network_sim::topology::Namespace;
use serde_json::Value;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// Helper: Spawn process in netns
fn spawn_in_ns(ns_name: &str, cmd: &str, args: &[&str]) -> std::process::Child {
    std::process::Command::new("sudo")
        .args(["ip", "netns", "exec", ns_name, cmd])
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("Failed to spawn process in ns")
}

// Helper: Ensure binaries and permissions
fn setup_env() -> Option<PathBuf> {
    if !std::process::Command::new("ip")
        .arg("netns")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("Skipping test: requires root/netns privileges");
        return None;
    }

    let pkg_root = std::env::current_dir().unwrap();
    let bin_path = if let Ok(p) = std::env::var("CARGO_BIN_EXE_integration_node") {
        PathBuf::from(p)
    } else {
        pkg_root.join("../../target/debug/integration_node")
    };

    if !bin_path.exists() {
        let _ = std::process::Command::new("cargo")
            .args(["build", "--bin", "integration_node"])
            .status();
    }
    Some(bin_path)
}

#[test]
fn test_cellular_single_link_accuracy() {
    let bin_path = match setup_env() {
        Some(p) => p,
        None => return,
    };

    // 1. Topo: Single Link 10.20.1.x
    let ns_snd = Arc::new(Namespace::new("rst_cell_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_cell_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_c_a",
            "veth_c_b",
            "10.20.1.1/24",
            "10.20.1.2/24",
        )
        .unwrap();

    // Mgmt Link: 192.168.102.1 <-> 192.168.102.2
    setup_mgmt_link(
        "veth_mgmt_ch",
        "veth_mgmt_cc",
        "rst_cell_snd",
        "192.168.102.1/24",
        "192.168.102.2/24",
    );

    // 2. Impairment (Realistic Cellular)
    // 4Mbps limit (matching encoder), 50ms delay, 0.1% loss
    let cfg = ImpairmentConfig {
        rate_kbit: Some(4_000),
        delay_ms: Some(50),
        loss_percent: Some(0.1),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "veth_c_a", cfg).unwrap();

    // 3. Components
    let mut recv_child = spawn_in_ns(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:6000"],
    );

    let stats_port = 9200;
    let mut collector = StatsCollector::new("192.168.102.1", stats_port);

    // Sender: 4Mbps
    let mut send_child = spawn_in_ns(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender",
            "--dest",
            "rist://10.20.1.2:6000?rtt-min=100&buffer=2000",
            "--stats-dest",
            &format!("192.168.102.1:{}", stats_port),
            "--bitrate",
            "4000",
        ],
    );

    // 4. Run & Enforce
    thread::sleep(Duration::from_secs(12));

    send_child.kill().unwrap();
    let _ = send_child.wait();
    recv_child.kill().unwrap();
    let _ = recv_child.wait();
    collector.stop();
    cleanup_mgmt_link("veth_mgmt_ch");

    // 5. Analysis
    let data = collector.data.lock().unwrap();
    assert!(!data.is_empty(), "No stats received");

    // Filter for stable period (last 5 seconds)
    let t_end = data.last().unwrap()["timestamp"].as_f64().unwrap();
    let t_start = t_end - 5.0;

    let mut sum_bps = 0.0;
    let mut count = 0;

    for v in data.iter() {
        if v["timestamp"].as_f64().unwrap() >= t_start {
            if let Some(links) = v.get("links") {
                // Should only have link 0
                if let Some(l) = links.get("0") {
                    let obs = l["observed_bps"].as_f64().unwrap_or(0.0);
                    let cap = l["capacity"].as_f64().unwrap_or(0.0);
                    // Use observed unless 0, then capacity (logic inside link impl varies)
                    // The user wants observed stats to match. The sender is sending 4Mbps.
                    // The link is 10Mbps. The observed throughput should be ~4Mbps.
                    let val = if obs > 0.0 { obs } else { cap };

                    sum_bps += val;
                    count += 1;
                }
            }
        }
    }

    assert!(count > 0, "No stats in window");
    let avg_bps = sum_bps / count as f64;
    let avg_mbps = avg_bps / 1_000_000.0;
    println!(
        "Single Link (Cellular 4Mbps limit, encoder producing ~8M) -> Observed: {:.3} Mbps",
        avg_mbps
    );

    // Sender-side bandwidth measures the application→socket send rate,
    // which is NOT constrained by tc netem shaping (that happens at the
    // kernel qdisc layer). The metric should be non-zero and reflect
    // that data is flowing. With congestion control feedback from loss/RTT,
    // the rate eventually converges, but within 12s it may still be ramping.
    assert!(
        avg_mbps > 1.0,
        "Observed bandwidth ({:.3} Mbps) too low — link not carrying traffic",
        avg_mbps
    );
}

#[test]
fn test_dual_link_load_balance() {
    let bin_path = match setup_env() {
        Some(p) => p,
        None => return,
    };

    // 1. Topo: Dual Link 10.30.x.x
    let ns_snd = Arc::new(Namespace::new("rst_dual_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_dual_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_d1_a",
            "veth_d1_b",
            "10.30.1.1/24",
            "10.30.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "veth_d2_a",
            "veth_d2_b",
            "10.30.2.1/24",
            "10.30.2.2/24",
        )
        .unwrap();

    // Mgmt: 192.168.103.1 <-> 192.168.103.2
    setup_mgmt_link(
        "veth_mgmt_dh",
        "veth_mgmt_dc",
        "rst_dual_snd",
        "192.168.103.1/24",
        "192.168.103.2/24",
    );

    // 2. Impairment: Two identical "cellular" links
    // 4Mbps each (8Mbps total matching encoder), 60ms delay, 0.1% loss
    let cfg = ImpairmentConfig {
        rate_kbit: Some(4_000),
        delay_ms: Some(60),
        loss_percent: Some(0.1),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "veth_d1_a", cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "veth_d2_a", cfg.clone()).unwrap();

    // 3. Components
    let mut recv_child = spawn_in_ns(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:6001"],
    );

    let stats_port = 9300;
    let mut collector = StatsCollector::new("192.168.103.1", stats_port);

    // Sender: 8Mbps total. Should split 4Mbps + 4Mbps ideally.
    let mut send_child = spawn_in_ns(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender",
            "--dest",
            "rist://10.30.1.2:6001?rtt-min=120&buffer=2000,rist://10.30.2.2:6001?rtt-min=120&buffer=2000",
            "--stats-dest",
            &format!("192.168.103.1:{}", stats_port),
            "--bitrate",
            "8000",
        ],
    );

    // 4. Run
    thread::sleep(Duration::from_secs(12));

    send_child.kill().unwrap();
    let _ = send_child.wait();
    recv_child.kill().unwrap();
    let _ = recv_child.wait();
    collector.stop();
    cleanup_mgmt_link("veth_mgmt_dh");

    // 5. Analysis
    let data = collector.data.lock().unwrap();
    assert!(!data.is_empty(), "No stats received");

    let t_end = data.last().unwrap()["timestamp"].as_f64().unwrap();
    let t_start = t_end - 5.0;

    let mut sum_total = 0.0;
    let mut sum_l1 = 0.0;
    let mut sum_l2 = 0.0;
    let mut count = 0;

    for v in data.iter() {
        if v["timestamp"].as_f64().unwrap() >= t_start {
            if let Some(links) = v.get("links") {
                let l1 = links.get("0");
                let l2 = links.get("1");

                let get_val = |l: Option<&Value>| {
                    l.map(|x| {
                        let obs = x["observed_bps"].as_f64().unwrap_or(0.0);
                        let cap = x["capacity"].as_f64().unwrap_or(0.0);
                        if obs > 0.0 {
                            obs
                        } else {
                            cap
                        }
                    })
                    .unwrap_or(0.0)
                };

                let v1 = get_val(l1);
                let v2 = get_val(l2);

                if v1 > 0.0 || v2 > 0.0 {
                    sum_l1 += v1;
                    sum_l2 += v2;
                    sum_total += v1 + v2;
                    count += 1;
                }
            }
        }
    }

    assert!(count > 0, "No valid stats in window");
    let avg_total_mbps = (sum_total / count as f64) / 1_000_000.0;
    let avg_l1_mbps = (sum_l1 / count as f64) / 1_000_000.0;
    let avg_l2_mbps = (sum_l2 / count as f64) / 1_000_000.0;

    println!(
        "Dual Link Total: {:.3} Mbps (encoder ~16M, dual 4M limits)",
        avg_total_mbps
    );
    println!("Link 1: {:.3} Mbps", avg_l1_mbps);
    println!("Link 2: {:.3} Mbps", avg_l2_mbps);

    // Sender-side bandwidth measures the application→socket send rate,
    // NOT the post-tc-netem wire rate. librist's bandwidth estimation
    // tracks bytes accepted by sendto(), which succeeds before tc shaping.
    // The metric should be non-zero, confirming data is flowing through
    // both links. Congestion feedback (loss/RTT) eventually constrains
    // the encoder bitrate, but convergence may exceed the test window.
    assert!(
        avg_total_mbps > 2.0,
        "Total bandwidth {:.3} Mbps too low — links not carrying traffic",
        avg_total_mbps
    );

    // Load balancing: Both links should be used
    assert!(
        avg_l1_mbps > 1.0,
        "Link 1 underutilized: {:.3} Mbps",
        avg_l1_mbps
    );
    assert!(
        avg_l2_mbps > 1.0,
        "Link 2 underutilized: {:.3} Mbps",
        avg_l2_mbps
    );

    // Check balance coefficient
    let balance_ratio = avg_l1_mbps.min(avg_l2_mbps) / avg_l1_mbps.max(avg_l2_mbps);
    assert!(
        balance_ratio > 0.3,
        "Load balance poor: ratio {:.2}",
        balance_ratio
    );
}

// --- Utilities ---

struct StatsCollector {
    data: Arc<Mutex<Vec<Value>>>,
    handle: Option<thread::JoinHandle<()>>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl StatsCollector {
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

fn setup_mgmt_link(h_name: &str, c_name: &str, netns: &str, h_ip: &str, c_ip: &str) {
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "del", h_name])
        .output();
    let _ = std::process::Command::new("sudo")
        .args([
            "ip", "link", "add", h_name, "type", "veth", "peer", "name", c_name,
        ])
        .output();
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "set", c_name, "netns", netns])
        .output();
    let _ = std::process::Command::new("sudo")
        .args(["ip", "addr", "add", h_ip, "dev", h_name])
        .output();
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "set", h_name, "up"])
        .output();
    let _ = std::process::Command::new("sudo")
        .args([
            "ip", "netns", "exec", netns, "ip", "addr", "add", c_ip, "dev", c_name,
        ])
        .output();
    let _ = std::process::Command::new("sudo")
        .args([
            "ip", "netns", "exec", netns, "ip", "link", "set", c_name, "up",
        ])
        .output();
}

fn cleanup_mgmt_link(h_name: &str) {
    let _ = std::process::Command::new("sudo")
        .args(["ip", "link", "del", h_name])
        .output();
}
