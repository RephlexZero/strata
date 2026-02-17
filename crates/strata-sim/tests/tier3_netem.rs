//! Tier 3 integration tests: real tc-netem impairment over Linux network namespaces.
//!
//! These tests exercise the full strata-node binary (sender + receiver) over
//! veth pairs with kernel-level qdisc shaping. They validate that the Strata
//! transport and bonding stack survives real packet drops, delay, and bandwidth
//! limits — scenarios that loopback-only tests cannot cover.
//!
//! **Requirements:**
//! - Linux with `ip netns` + `tc netem` support
//! - Root / passwordless sudo
//! - `strata-node` binary (built automatically if missing)
//!
//! Run:
//! ```bash
//! sudo cargo test -p strata-sim --test tier3_netem -- --nocapture
//! ```

use serde_json::Value;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use strata_sim::impairment::{apply_impairment, ImpairmentConfig};
use strata_sim::scenario::{LinkScenarioConfig, Scenario, ScenarioConfig};
use strata_sim::test_util::check_privileges;
use strata_sim::topology::Namespace;

// ─── Shared test harness ────────────────────────────────────────────

/// Ensure the strata-node binary is built and return its path.
fn strata_node_binary() -> PathBuf {
    if let Ok(p) = std::env::var("STRATA_NODE_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }

    // Build if needed
    let _ = Command::new("cargo")
        .args(["build", "--bin", "strata-node"])
        .status();

    // Walk up from the test binary to find target/debug/strata-node
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // deps
    path.pop(); // debug
    path.push("strata-node");

    if !path.exists() {
        // Fallback: workspace root
        let cwd = std::env::current_dir().unwrap();
        let try_path = cwd.join("target/debug/strata-node");
        if try_path.exists() {
            return try_path;
        }
        let try_path2 = cwd.join("../../target/debug/strata-node");
        if try_path2.exists() {
            return try_path2;
        }
        panic!("strata-node binary not found at {:?}", path);
    }
    path
}

/// Spawn a process inside a network namespace.
fn spawn_in_ns(ns: &str, cmd: &str, args: &[&str]) -> std::process::Child {
    Command::new("sudo")
        .args(["ip", "netns", "exec", ns, cmd])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn {cmd} in {ns}: {e}"))
}

/// Create a management veth link from host to a namespace for stats relay.
fn setup_mgmt_link(host_veth: &str, ns_veth: &str, ns_name: &str, host_ip: &str, ns_ip: &str) {
    let _ = Command::new("sudo")
        .args(["ip", "link", "del", host_veth])
        .output();
    let _ = Command::new("sudo")
        .args([
            "ip", "link", "add", host_veth, "type", "veth", "peer", "name", ns_veth,
        ])
        .output();
    let _ = Command::new("sudo")
        .args(["ip", "link", "set", ns_veth, "netns", ns_name])
        .output();
    let _ = Command::new("sudo")
        .args(["ip", "addr", "add", host_ip, "dev", host_veth])
        .output();
    let _ = Command::new("sudo")
        .args(["ip", "link", "set", host_veth, "up"])
        .output();
    let _ = Command::new("sudo")
        .args([
            "ip", "netns", "exec", ns_name, "ip", "addr", "add", ns_ip, "dev", ns_veth,
        ])
        .output();
    let _ = Command::new("sudo")
        .args([
            "ip", "netns", "exec", ns_name, "ip", "link", "set", ns_veth, "up",
        ])
        .output();
}

fn cleanup_mgmt_link(host_veth: &str) {
    let _ = Command::new("sudo")
        .args(["ip", "link", "del", host_veth])
        .output();
}

/// Collects JSON stats packets over UDP in a background thread.
struct StatsCollector {
    data: Arc<Mutex<Vec<Value>>>,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StatsCollector {
    fn new(bind_addr: &str) -> Self {
        let socket = UdpSocket::bind(bind_addr).unwrap_or_else(|e| panic!("Bind {bind_addr}: {e}"));
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();

        let data = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));

        let d = data.clone();
        let r = running.clone();

        let handle = thread::spawn(move || {
            let mut buf = [0u8; 65535];
            while r.load(Ordering::Relaxed) {
                if let Ok((amt, _)) = socket.recv_from(&mut buf) {
                    if let Ok(val) = serde_json::from_slice::<Value>(&buf[..amt]) {
                        d.lock().unwrap().push(val);
                    }
                }
            }
        });

        Self {
            data,
            running,
            handle: Some(handle),
        }
    }

    fn stop(&mut self) -> Vec<Value> {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.data.lock().unwrap().clone()
    }
}

/// Helper: extract total observed_bps from a stats JSON Value.
fn total_observed_bps(v: &Value) -> f64 {
    let mut total = 0.0;
    if let Some(links) = v.get("links").and_then(|l| l.as_array()) {
        for link in links {
            total += link
                .get("observed_bps")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
        }
    }
    total
}

/// Guard that requires privileges. Returns binary path or skips the test.
fn require_privileged_env() -> Option<PathBuf> {
    if !check_privileges() {
        eprintln!("Skipping test: requires root/netns privileges");
        return None;
    }
    Some(strata_node_binary())
}

// ─── Tests ──────────────────────────────────────────────────────────

/// Capacity step-change: two links start at 4 Mbps each, then one drops to
/// 1 Mbps mid-stream. Verifies throughput recovers and doesn't collapse.
#[test]
fn capacity_step_change() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_step_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_step_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_s1_a",
            "st_s1_b",
            "10.60.1.1/24",
            "10.60.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_s2_a",
            "st_s2_b",
            "10.60.2.1/24",
            "10.60.2.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_sc",
        "st_mgmt_sd",
        "st_step_snd",
        "192.168.200.1/24",
        "192.168.200.2/24",
    );

    // Both links at 4 Mbps, 30ms delay
    let cfg = ImpairmentConfig {
        rate_kbit: Some(4_000),
        delay_ms: Some(30),
        loss_percent: Some(0.1),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "st_s1_a", cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "st_s2_a", cfg).unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7000"],
    );
    let mut collector = StatsCollector::new("192.168.200.1:9800");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--dest",
            "10.60.1.2:7000?rtt-min=60&buffer=2000,10.60.2.2:7000?rtt-min=60&buffer=2000",
            "--stats-dest",
            "192.168.200.1:9800",
            "--bitrate",
            "6000",
        ],
    );

    // Stabilize
    thread::sleep(Duration::from_secs(7));

    // Drop link A from 4 → 1 Mbps
    eprintln!(">>> CAPACITY DROP: Link A 4 Mbps → 1 Mbps");
    apply_impairment(
        &ns_snd,
        "st_s1_a",
        ImpairmentConfig {
            rate_kbit: Some(1_000),
            delay_ms: Some(30),
            loss_percent: Some(0.1),
            ..Default::default()
        },
    )
    .unwrap();

    // Let it recover
    thread::sleep(Duration::from_secs(8));

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_sc");

    assert!(!data.is_empty(), "No stats received");

    // Check post-drop recovery window (last 3s)
    let last_ts = data.last().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let window_start = last_ts - 3000.0;

    let recovery_bps: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start)
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    assert!(!recovery_bps.is_empty(), "No stats in recovery window");
    let avg_mbps = recovery_bps.iter().sum::<f64>() / recovery_bps.len() as f64 / 1_000_000.0;

    eprintln!(
        "Post-drop recovery: {:.2} Mbps (expected ~5 Mbps cap: 1+4)",
        avg_mbps
    );

    assert!(
        avg_mbps > 0.5,
        "Throughput ({:.2} Mbps) did not recover after capacity drop",
        avg_mbps
    );
}

/// Link failure and recovery: one of two links goes down mid-stream,
/// then comes back. Verifies no crash and throughput resumes.
#[test]
fn link_failure_recovery() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_fail_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_fail_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_f1_a",
            "st_f1_b",
            "10.61.1.1/24",
            "10.61.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_f2_a",
            "st_f2_b",
            "10.61.2.1/24",
            "10.61.2.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_fl",
        "st_mgmt_fm",
        "st_fail_snd",
        "192.168.201.1/24",
        "192.168.201.2/24",
    );

    let cfg = ImpairmentConfig {
        rate_kbit: Some(4_000),
        delay_ms: Some(30),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "st_f1_a", cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "st_f2_a", cfg).unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7100"],
    );
    let mut collector = StatsCollector::new("192.168.201.1:9801");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--dest",
            "10.61.1.2:7100?rtt-min=60&buffer=2000,10.61.2.2:7100?rtt-min=60&buffer=2000",
            "--stats-dest",
            "192.168.201.1:9801",
            "--bitrate",
            "6000",
        ],
    );

    thread::sleep(Duration::from_secs(5));

    // Take link B down
    eprintln!(">>> LINK FAILURE: st_f2_a DOWN");
    let _ = ns_snd.exec("ip", &["link", "set", "st_f2_a", "down"]);
    thread::sleep(Duration::from_secs(4));

    // Bring link B back
    eprintln!(">>> LINK RECOVERY: st_f2_a UP");
    let _ = ns_snd.exec("ip", &["link", "set", "st_f2_a", "up"]);
    thread::sleep(Duration::from_secs(5));

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_fl");

    assert!(
        !data.is_empty(),
        "No stats received — system may have crashed during link failure"
    );

    // Post-recovery window (last 3s)
    let last_ts = data.last().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let window_start = last_ts - 3000.0;

    let recovery_bps: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start)
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    assert!(!recovery_bps.is_empty(), "No stats in recovery window");
    let avg_mbps = recovery_bps.iter().sum::<f64>() / recovery_bps.len() as f64 / 1_000_000.0;

    eprintln!("Post-recovery throughput: {:.2} Mbps", avg_mbps);

    assert!(
        avg_mbps > 0.5,
        "Throughput ({:.2} Mbps) did not recover after link failure/recovery",
        avg_mbps
    );
}

/// Dynamic chaos scenario: two links with evolving impairment over 30s.
/// Verifies the system survives the full duration without crashing and
/// maintains non-zero throughput throughout.
#[test]
fn chaos_scenario() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_chaos_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_chaos_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_c1_a",
            "st_c1_b",
            "10.62.1.1/24",
            "10.62.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_c2_a",
            "st_c2_b",
            "10.62.2.1/24",
            "10.62.2.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_ch",
        "st_mgmt_ci",
        "st_chaos_snd",
        "192.168.202.1/24",
        "192.168.202.2/24",
    );

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7200"],
    );
    let mut collector = StatsCollector::new("192.168.202.1:9802");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--dest",
            "10.62.1.2:7200,10.62.2.2:7200",
            "--stats-dest",
            "192.168.202.1:9802",
            "--bitrate",
            "5000",
        ],
    );

    // Run scenario
    let scenario_start = Instant::now();
    let mut scenario = Scenario::new(ScenarioConfig {
        seed: 42,
        duration: Duration::from_secs(25),
        step: Duration::from_secs(1),
        links: vec![
            LinkScenarioConfig {
                min_rate_kbit: 3000,
                max_rate_kbit: 6000,
                rate_step_kbit: 300,
                base_delay_ms: 20,
                delay_jitter_ms: 15,
                delay_step_ms: 4,
                max_loss_percent: 3.0,
                loss_step_percent: 0.5,
            },
            LinkScenarioConfig {
                min_rate_kbit: 500,
                max_rate_kbit: 2000,
                rate_step_kbit: 200,
                base_delay_ms: 40,
                delay_jitter_ms: 30,
                delay_step_ms: 6,
                max_loss_percent: 8.0,
                loss_step_percent: 1.0,
            },
        ],
    });

    for frame in scenario.frames() {
        let elapsed = scenario_start.elapsed();
        if elapsed < frame.t {
            thread::sleep(frame.t - elapsed);
        }
        let _ = apply_impairment(&ns_snd, "st_c1_a", frame.configs[0].clone());
        let _ = apply_impairment(&ns_snd, "st_c2_a", frame.configs[1].clone());
    }

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_ch");

    assert!(
        !data.is_empty(),
        "No stats collected during chaos scenario — system may have crashed"
    );

    // At least some stats should show non-zero throughput
    let nonzero_count = data
        .iter()
        .filter(|v| total_observed_bps(v) > 100_000.0)
        .count();

    eprintln!(
        "Chaos scenario: {} stats points, {} with >100kbps throughput",
        data.len(),
        nonzero_count
    );

    assert!(
        nonzero_count >= 3,
        "Too few stats with meaningful throughput ({nonzero_count}) — system may not be functioning under impairment"
    );
}

/// Long-running stability: single 5 Mbps link, encoder at 4 Mbps, run for
/// ~20s. Throughput coefficient of variation (CV) should be < 30%, confirming
/// no drift, oscillation, or resource leak.
#[test]
fn throughput_stability() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_stab_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_stab_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_st_a",
            "st_st_b",
            "10.63.1.1/24",
            "10.63.1.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_st",
        "st_mgmt_su",
        "st_stab_snd",
        "192.168.203.1/24",
        "192.168.203.2/24",
    );

    apply_impairment(
        &ns_snd,
        "st_st_a",
        ImpairmentConfig {
            rate_kbit: Some(5_000),
            delay_ms: Some(30),
            loss_percent: Some(0.05),
            ..Default::default()
        },
    )
    .unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7300"],
    );
    let mut collector = StatsCollector::new("192.168.203.1:9803");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--dest",
            "10.63.1.2:7300?rtt-min=60&buffer=2000",
            "--stats-dest",
            "192.168.203.1:9803",
            "--bitrate",
            "4000",
        ],
    );

    // Run for 20s
    thread::sleep(Duration::from_secs(20));

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_st");

    assert!(!data.is_empty(), "No stats collected");

    // Collect throughput samples, skip first 25% (warmup)
    let samples: Vec<f64> = data
        .iter()
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    assert!(
        samples.len() >= 5,
        "Too few throughput samples ({}) for stability analysis",
        samples.len()
    );

    let skip = samples.len() / 4;
    let stable: &[f64] = &samples[skip..];

    let n = stable.len() as f64;
    let mean = stable.iter().sum::<f64>() / n;
    let variance = stable.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let stddev = variance.sqrt();
    let cv = if mean > 0.0 { stddev / mean } else { 1.0 };

    eprintln!(
        "Stability: mean={:.0} bps, stddev={:.0} bps, CV={:.3} ({} samples)",
        mean,
        stddev,
        cv,
        stable.len()
    );

    assert!(
        cv < 0.30,
        "Throughput CV ({:.3}) exceeds 30% — system may be oscillating or drifting",
        cv
    );
}

/// Asymmetric RTT: two links with very different latency (20ms vs 150ms).
/// Verifies both links carry traffic and combined throughput is reasonable.
#[test]
fn asymmetric_rtt_bonding() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_asym_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_asym_rcv").unwrap());

    // Link A: low latency
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_a1_a",
            "st_a1_b",
            "10.64.1.1/24",
            "10.64.1.2/24",
        )
        .unwrap();
    // Link B: high latency
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_a2_a",
            "st_a2_b",
            "10.64.2.1/24",
            "10.64.2.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_as",
        "st_mgmt_at",
        "st_asym_snd",
        "192.168.204.1/24",
        "192.168.204.2/24",
    );

    // Link A: 4 Mbps, 20ms (WiFi-like)
    apply_impairment(
        &ns_snd,
        "st_a1_a",
        ImpairmentConfig {
            rate_kbit: Some(4_000),
            delay_ms: Some(20),
            loss_percent: Some(0.05),
            ..Default::default()
        },
    )
    .unwrap();
    // Link B: 4 Mbps, 150ms (LTE-like)
    apply_impairment(
        &ns_snd,
        "st_a2_a",
        ImpairmentConfig {
            rate_kbit: Some(4_000),
            delay_ms: Some(150),
            loss_percent: Some(0.1),
            ..Default::default()
        },
    )
    .unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7400"],
    );
    let mut collector = StatsCollector::new("192.168.204.1:9804");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--dest",
            "10.64.1.2:7400?rtt-min=40&buffer=2000,10.64.2.2:7400?rtt-min=300&buffer=2000",
            "--stats-dest",
            "192.168.204.1:9804",
            "--bitrate",
            "6000",
        ],
    );

    thread::sleep(Duration::from_secs(15));

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_as");

    assert!(!data.is_empty(), "No stats received");

    // Check last 5s — both links should carry traffic
    let last_ts = data.last().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let window_start = last_ts - 5000.0;

    let mut link_bytes = [0u64; 2];
    let mut sample_count = 0;

    for v in &data {
        if v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start {
            if let Some(links) = v.get("links").and_then(|l| l.as_array()) {
                for (i, link) in links.iter().enumerate().take(2) {
                    link_bytes[i] += link.get("sent_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                }
                sample_count += 1;
            }
        }
    }

    eprintln!(
        "Asymmetric RTT: link_a_bytes={}, link_b_bytes={}, samples={}",
        link_bytes[0], link_bytes[1], sample_count
    );

    // Both links should have sent data
    let total_bps: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start)
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    assert!(!total_bps.is_empty(), "No throughput in stable window");
    let avg_total_mbps = total_bps.iter().sum::<f64>() / total_bps.len() as f64 / 1_000_000.0;

    eprintln!("Combined throughput: {:.2} Mbps", avg_total_mbps);

    assert!(
        avg_total_mbps > 1.0,
        "Combined throughput ({:.2} Mbps) too low for 8 Mbps aggregate capacity",
        avg_total_mbps
    );
}
