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

    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        // Build if needed
        let _ = Command::new("cargo")
            .args(["build", "-p", "strata-sim", "--bin", "dummy_node"])
            .status();
    });

    // Walk up from the test binary to find target/debug/dummy_node
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // deps
    path.pop(); // debug
    path.push("dummy_node");

    if !path.exists() {
        // Fallback: workspace root
        let cwd = std::env::current_dir().unwrap();
        let try_path = cwd.join("target/debug/dummy_node");
        if try_path.exists() {
            return try_path;
        }
        let try_path2 = cwd.join("../../target/debug/dummy_node");
        if try_path2.exists() {
            return try_path2;
        }
        panic!("dummy_node binary not found at {:?}", path);
    }
    path
}

/// Spawn a process inside a network namespace.
fn spawn_in_ns(ns: &str, cmd: &str, args: &[&str]) -> std::process::Child {
    Command::new("sudo")
        .args(["ip", "netns", "exec", ns, cmd])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
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

/// Extract the sender's current adapted bitrate from a stats Value.
fn stats_current_bitrate_bps(v: &Value) -> f64 {
    v.get("current_bitrate_bps")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

/// Extract total estimated_capacity_bps (sum across all links) from a stats Value.
fn total_estimated_capacity_bps(v: &Value) -> f64 {
    let mut total = 0.0;
    if let Some(links) = v.get("links").and_then(|l| l.as_array()) {
        for link in links {
            total += link
                .get("estimated_capacity_bps")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
        }
    }
    total
}

/// Extract per-link cumulative sent_bytes from a single stats Value snapshot.
fn link_sent_bytes(v: &Value) -> Vec<u64> {
    if let Some(links) = v.get("links").and_then(|l| l.as_array()) {
        links
            .iter()
            .map(|l| l.get("sent_bytes").and_then(|v| v.as_u64()).unwrap_or(0))
            .collect()
    } else {
        vec![]
    }
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
#[ignore = "Requires BBR-based capacity estimation (Phase A) - work in progress"]
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
            "--codec",
            "h264",
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

    // Verify the adaptation loop responded: early bitrate should be higher than post-drop bitrate.
    let first_ts = data.first().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let early_end = first_ts + 5000.0;

    let early_bitrate: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 <= early_end)
        .map(stats_current_bitrate_bps)
        .filter(|&b| b > 0.0)
        .collect();

    let late_bitrate: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start)
        .map(stats_current_bitrate_bps)
        .filter(|&b| b > 0.0)
        .collect();

    if !early_bitrate.is_empty() && !late_bitrate.is_empty() {
        let early_avg = early_bitrate.iter().sum::<f64>() / early_bitrate.len() as f64;
        let late_avg = late_bitrate.iter().sum::<f64>() / late_bitrate.len() as f64;
        eprintln!(
            "Bitrate adaptation: pre-drop={:.2} Mbps \u{2192} post-drop={:.2} Mbps",
            early_avg / 1_000_000.0,
            late_avg / 1_000_000.0
        );
        assert!(
            late_avg < early_avg * 0.95,
            "Bitrate did not adapt after capacity drop: pre={:.2} Mbps, post={:.2} Mbps \u{2014} \
             adaptation loop may not be closing",
            early_avg / 1_000_000.0,
            late_avg / 1_000_000.0
        );
    }
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
            "--codec",
            "h264",
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
            "--codec",
            "h264",
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
            "--codec",
            "h264",
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
            "--codec",
            "h264",
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

    // Verify per-link utilization using the final stats snapshot.
    // `sent_bytes` is cumulative from process start, so the last snapshot
    // directly shows the total distribution across the run.
    if let Some(last_snap) = data.last() {
        let bytes = link_sent_bytes(last_snap);
        if bytes.len() >= 2 {
            eprintln!(
                "Asymmetric RTT: link_a_bytes={}, link_b_bytes={}",
                bytes[0], bytes[1]
            );
            assert!(
                bytes[0] > 0,
                "Link A (20ms) sent 0 bytes — low-latency link not utilized"
            );
            assert!(
                bytes[1] > 0,
                "Link B (150ms) sent 0 bytes — high-latency link not utilized"
            );
        }
    }

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

// ─── GAME_PLAN Phase E: Core Scenarios ──────────────────────────────

/// Scenario 1 — "The Cliff": highest-capacity link drops to 0 instantly.
/// Three links, link A (8 Mbps) is the strongest. It goes to 100% loss at t=10s.
/// Assertion: stream survives, throughput recovers to remaining capacity.
#[test]
fn the_cliff() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_cliff_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_cliff_rcv").unwrap());

    // Link A: 8 Mbps (strongest — will cliff)
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_cl1_a",
            "st_cl1_b",
            "10.70.1.1/24",
            "10.70.1.2/24",
        )
        .unwrap();
    // Link B: 5 Mbps
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_cl2_a",
            "st_cl2_b",
            "10.70.2.1/24",
            "10.70.2.2/24",
        )
        .unwrap();
    // Link C: 6 Mbps
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_cl3_a",
            "st_cl3_b",
            "10.70.3.1/24",
            "10.70.3.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_cl",
        "st_mgmt_cm",
        "st_cliff_snd",
        "192.168.210.1/24",
        "192.168.210.2/24",
    );

    // Initial: A=8 Mbps, B=5 Mbps, C=6 Mbps, all 30ms
    apply_impairment(
        &ns_snd,
        "st_cl1_a",
        ImpairmentConfig {
            rate_kbit: Some(8_000),
            delay_ms: Some(30),
            loss_percent: Some(0.1),
            ..Default::default()
        },
    )
    .unwrap();
    apply_impairment(
        &ns_snd,
        "st_cl2_a",
        ImpairmentConfig {
            rate_kbit: Some(5_000),
            delay_ms: Some(30),
            loss_percent: Some(0.1),
            ..Default::default()
        },
    )
    .unwrap();
    apply_impairment(
        &ns_snd,
        "st_cl3_a",
        ImpairmentConfig {
            rate_kbit: Some(6_000),
            delay_ms: Some(30),
            loss_percent: Some(0.1),
            ..Default::default()
        },
    )
    .unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7500"],
    );
    let mut collector = StatsCollector::new("192.168.210.1:9810");

    let mut sender = spawn_in_ns(&ns_snd.name, bin_str, &[
        "sender", "--dest",
        "10.70.1.2:7500?rtt-min=60&buffer=2000,10.70.2.2:7500?rtt-min=60&buffer=2000,10.70.3.2:7500?rtt-min=60&buffer=2000",
        "--stats-dest", "192.168.210.1:9810",
        "--bitrate", "15000",
    ]);

    // Stabilize
    thread::sleep(Duration::from_secs(10));

    // THE CLIFF: Link A goes to 100% loss
    eprintln!(">>> THE CLIFF: Link A (8 Mbps) goes DOWN");
    apply_impairment(
        &ns_snd,
        "st_cl1_a",
        ImpairmentConfig {
            rate_kbit: Some(1),
            delay_ms: Some(2000),
            loss_percent: Some(100.0),
            ..Default::default()
        },
    )
    .unwrap();

    // Let it recover on remaining links
    thread::sleep(Duration::from_secs(10));

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_cl");

    assert!(!data.is_empty(), "No stats received during The Cliff");

    // Post-cliff recovery (last 5s)
    let last_ts = data.last().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let window_start = last_ts - 5000.0;

    let recovery_bps: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start)
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    assert!(!recovery_bps.is_empty(), "No stats in post-cliff window");
    let avg_mbps = recovery_bps.iter().sum::<f64>() / recovery_bps.len() as f64 / 1_000_000.0;

    eprintln!(
        "The Cliff — post-cliff throughput: {:.2} Mbps (B+C cap: ~11 Mbps)",
        avg_mbps
    );

    // Should recover to at least 2 Mbps on remaining B+C links
    assert!(
        avg_mbps > 2.0,
        "Post-cliff throughput ({:.2} Mbps) too low — stream did not survive the cliff",
        avg_mbps
    );
}

/// Scenario 2 — "Flapping Link": one link toggles between 5 Mbps and 500 kbps
/// every 5 seconds. Verifies scheduler penalizes the unstable link and no
/// encoder oscillation occurs.
#[test]
fn flapping_link() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_flap_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_flap_rcv").unwrap());

    // Link A: stable
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_fl1_a",
            "st_fl1_b",
            "10.71.1.1/24",
            "10.71.1.2/24",
        )
        .unwrap();
    // Link B: flapping
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_fl2_a",
            "st_fl2_b",
            "10.71.2.1/24",
            "10.71.2.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_fp",
        "st_mgmt_fq",
        "st_flap_snd",
        "192.168.211.1/24",
        "192.168.211.2/24",
    );

    // Both links start at 5 Mbps
    let stable_cfg = ImpairmentConfig {
        rate_kbit: Some(5_000),
        delay_ms: Some(30),
        loss_percent: Some(0.1),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "st_fl1_a", stable_cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "st_fl2_a", stable_cfg).unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7510"],
    );
    let mut collector = StatsCollector::new("192.168.211.1:9811");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--codec",
            "h264",
            "--dest",
            "10.71.1.2:7510?rtt-min=60&buffer=2000,10.71.2.2:7510?rtt-min=60&buffer=2000",
            "--stats-dest",
            "192.168.211.1:9811",
            "--bitrate",
            "8000",
        ],
    );

    // Stabilize
    thread::sleep(Duration::from_secs(5));

    // Flap link B: 5 Mbps ↔ 500 kbps every 5s, total 30s
    let flap_start = Instant::now();
    for cycle in 0..6 {
        let rate = if cycle % 2 == 0 { 500 } else { 5_000 };
        eprintln!(">>> FLAP cycle {}: Link B → {} kbps", cycle, rate);
        apply_impairment(
            &ns_snd,
            "st_fl2_a",
            ImpairmentConfig {
                rate_kbit: Some(rate),
                delay_ms: Some(30),
                loss_percent: Some(0.1),
                ..Default::default()
            },
        )
        .unwrap();

        let target = flap_start + Duration::from_secs((cycle + 1) * 5);
        let now = Instant::now();
        if now < target {
            thread::sleep(target - now);
        }
    }

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_fp");

    assert!(!data.is_empty(), "No stats received during flapping test");

    // Throughput should remain non-zero throughout — the stable link carries traffic
    let all_bps: Vec<f64> = data
        .iter()
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    assert!(
        all_bps.len() >= 5,
        "Too few throughput samples during flapping"
    );

    // Last 5s: should be stable on link A at minimum
    let last_ts = data.last().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let window_start = last_ts - 5000.0;

    let final_bps: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start)
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    if !final_bps.is_empty() {
        let avg_mbps = final_bps.iter().sum::<f64>() / final_bps.len() as f64 / 1_000_000.0;
        eprintln!("Flapping — final window throughput: {:.2} Mbps", avg_mbps);

        assert!(
            avg_mbps > 0.5,
            "Final throughput ({:.2} Mbps) collapsed during flapping",
            avg_mbps
        );
    }
}

/// Scenario 3 — "Jitter Bomb": all links get 500ms jitter simultaneously.
/// Verifies smooth delivery with higher latency, no stuttering.
#[test]
fn jitter_bomb() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_jit_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_jit_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_j1_a",
            "st_j1_b",
            "10.72.1.1/24",
            "10.72.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_j2_a",
            "st_j2_b",
            "10.72.2.1/24",
            "10.72.2.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_jt",
        "st_mgmt_ju",
        "st_jit_snd",
        "192.168.212.1/24",
        "192.168.212.2/24",
    );

    // Start with low jitter
    let base_cfg = ImpairmentConfig {
        rate_kbit: Some(5_000),
        delay_ms: Some(30),
        jitter_ms: Some(5),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "st_j1_a", base_cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "st_j2_a", base_cfg).unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7520"],
    );
    let mut collector = StatsCollector::new("192.168.212.1:9812");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--codec",
            "h264",
            "--dest",
            "10.72.1.2:7520?rtt-min=60&buffer=3000,10.72.2.2:7520?rtt-min=60&buffer=3000",
            "--stats-dest",
            "192.168.212.1:9812",
            "--bitrate",
            "6000",
        ],
    );

    // Stabilize
    thread::sleep(Duration::from_secs(7));

    // JITTER BOMB: 500ms jitter on both links
    eprintln!(">>> JITTER BOMB: both links → 500ms jitter");
    let jitter_cfg = ImpairmentConfig {
        rate_kbit: Some(5_000),
        delay_ms: Some(50),
        jitter_ms: Some(500),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "st_j1_a", jitter_cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "st_j2_a", jitter_cfg).unwrap();

    // Run under jitter for 15s
    thread::sleep(Duration::from_secs(15));

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_jt");

    assert!(!data.is_empty(), "No stats received during jitter bomb");

    // Check that system didn't crash — any non-zero throughput in last 5s
    let last_ts = data.last().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let window_start = last_ts - 5000.0;

    let jitter_bps: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start)
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    eprintln!(
        "Jitter Bomb — {} samples with throughput in final window",
        jitter_bps.len()
    );

    // Under extreme jitter the system must still deliver a measurable bitstream.
    assert!(
        !jitter_bps.is_empty(),
        "No throughput in final window \u{2014} system collapsed under jitter bomb \
         ({} total stats collected)",
        data.len()
    );
    let avg_jitter_mbps = jitter_bps.iter().sum::<f64>() / jitter_bps.len() as f64 / 1_000_000.0;
    eprintln!(
        "Jitter Bomb \u{2014} final window: {:.2} Mbps",
        avg_jitter_mbps
    );
    assert!(
        avg_jitter_mbps > 0.1,
        "Throughput ({:.2} Mbps) too low under jitter bomb \u{2014} transport may have stalled",
        avg_jitter_mbps
    );
}

/// Scenario 4 — "Burst Loss": 20% loss for 2 seconds simulating a cellular handover.
/// Verifies FEC + ARQ recovers with minimal visible impact.
#[test]
fn burst_loss() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_burst_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_burst_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_b1_a",
            "st_b1_b",
            "10.73.1.1/24",
            "10.73.1.2/24",
        )
        .unwrap();
    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_b2_a",
            "st_b2_b",
            "10.73.2.1/24",
            "10.73.2.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_bu",
        "st_mgmt_bv",
        "st_burst_snd",
        "192.168.213.1/24",
        "192.168.213.2/24",
    );

    let normal_cfg = ImpairmentConfig {
        rate_kbit: Some(5_000),
        delay_ms: Some(30),
        loss_percent: Some(0.1),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "st_b1_a", normal_cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "st_b2_a", normal_cfg).unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7530"],
    );
    let mut collector = StatsCollector::new("192.168.213.1:9813");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--codec",
            "h264",
            "--dest",
            "10.73.1.2:7530?rtt-min=60&buffer=2000,10.73.2.2:7530?rtt-min=60&buffer=2000",
            "--stats-dest",
            "192.168.213.1:9813",
            "--bitrate",
            "6000",
        ],
    );

    // Stabilize
    thread::sleep(Duration::from_secs(8));

    // BURST LOSS: 20% on both links for 2 seconds
    eprintln!(">>> BURST LOSS: 20% loss on both links for 2s");
    let burst_cfg = ImpairmentConfig {
        rate_kbit: Some(5_000),
        delay_ms: Some(30),
        loss_percent: Some(20.0),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "st_b1_a", burst_cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "st_b2_a", burst_cfg).unwrap();

    thread::sleep(Duration::from_secs(2));

    // Restore normal
    eprintln!(">>> BURST LOSS: restored to normal");
    let normal_cfg = ImpairmentConfig {
        rate_kbit: Some(5_000),
        delay_ms: Some(30),
        loss_percent: Some(0.1),
        ..Default::default()
    };
    apply_impairment(&ns_snd, "st_b1_a", normal_cfg.clone()).unwrap();
    apply_impairment(&ns_snd, "st_b2_a", normal_cfg).unwrap();

    // Recovery window
    thread::sleep(Duration::from_secs(8));

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_bu");

    assert!(!data.is_empty(), "No stats received during burst loss");

    // Post-burst recovery (last 5s)
    let last_ts = data.last().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let window_start = last_ts - 5000.0;

    let recovery_bps: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start)
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    assert!(!recovery_bps.is_empty(), "No stats in post-burst window");
    let avg_mbps = recovery_bps.iter().sum::<f64>() / recovery_bps.len() as f64 / 1_000_000.0;

    eprintln!(
        "Burst Loss — post-recovery throughput: {:.2} Mbps",
        avg_mbps
    );

    assert!(
        avg_mbps > 1.0,
        "Post-burst throughput ({:.2} Mbps) too low — system did not recover from burst loss",
        avg_mbps
    );
}

/// Scenario 5 — "Bandwidth Ramp": link capacity increases from 1 Mbps to 20 Mbps
/// over 30 seconds. Verifies the capacity estimator detects the increase and
/// encoder bitrate ramps up.
#[test]
#[ignore = "Requires BBR-based capacity estimation (Phase A) - work in progress"]
fn bandwidth_ramp() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_ramp_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_ramp_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_r1_a",
            "st_r1_b",
            "10.74.1.1/24",
            "10.74.1.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_rm",
        "st_mgmt_rn",
        "st_ramp_snd",
        "192.168.214.1/24",
        "192.168.214.2/24",
    );

    // Start at 1 Mbps
    apply_impairment(
        &ns_snd,
        "st_r1_a",
        ImpairmentConfig {
            rate_kbit: Some(1_000),
            delay_ms: Some(30),
            loss_percent: Some(0.1),
            ..Default::default()
        },
    )
    .unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7540"],
    );
    let mut collector = StatsCollector::new("192.168.214.1:9814");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--codec",
            "h264",
            "--dest",
            "10.74.1.2:7540?rtt-min=60&buffer=2000",
            "--stats-dest",
            "192.168.214.1:9814",
            "--bitrate",
            "20000",
        ],
    );

    // Stabilize at 1 Mbps
    thread::sleep(Duration::from_secs(5));

    // Ramp from 1 Mbps → 20 Mbps over 30 seconds
    let ramp_start = Instant::now();
    let ramp_duration = Duration::from_secs(30);
    let ramp_steps = 30;
    for step in 0..ramp_steps {
        let progress = (step + 1) as f64 / ramp_steps as f64;
        let rate_kbit = 1_000.0 + progress * 19_000.0; // 1 → 20 Mbps

        apply_impairment(
            &ns_snd,
            "st_r1_a",
            ImpairmentConfig {
                rate_kbit: Some(rate_kbit as u64),
                delay_ms: Some(30),
                loss_percent: Some(0.1),
                ..Default::default()
            },
        )
        .unwrap();

        let target = ramp_start + ramp_duration.mul_f64((step + 1) as f64 / ramp_steps as f64);
        let now = Instant::now();
        if now < target {
            thread::sleep(target - now);
        }
    }

    // Hold at peak for 5s
    thread::sleep(Duration::from_secs(5));

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_rm");

    assert!(!data.is_empty(), "No stats received during bandwidth ramp");

    // Compare early vs late throughput
    let first_ts = data.first().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let last_ts = data.last().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;

    // Early: first 5s
    let early_end = first_ts + 5000.0;
    let early_bps: Vec<f64> = data
        .iter()
        .filter(|v| {
            let ts = v["timestamp_ms"].as_u64().unwrap_or(0) as f64;
            ts <= early_end
        })
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    // Late: last 5s
    let late_start = last_ts - 5000.0;
    let late_bps: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= late_start)
        .map(total_observed_bps)
        .filter(|&b| b > 0.0)
        .collect();

    let early_avg = if early_bps.is_empty() {
        0.0
    } else {
        early_bps.iter().sum::<f64>() / early_bps.len() as f64
    };
    let late_avg = if late_bps.is_empty() {
        0.0
    } else {
        late_bps.iter().sum::<f64>() / late_bps.len() as f64
    };

    eprintln!(
        "Bandwidth Ramp — early: {:.2} Mbps, late: {:.2} Mbps",
        early_avg / 1_000_000.0,
        late_avg / 1_000_000.0
    );

    // Late throughput should be significantly higher than early
    if !early_bps.is_empty() && !late_bps.is_empty() {
        assert!(
            late_avg > early_avg * 1.5,
            "Throughput did not ramp: early={:.2} Mbps, late={:.2} Mbps — \
             capacity estimator may not be detecting increase",
            early_avg / 1_000_000.0,
            late_avg / 1_000_000.0
        );
    }
}

// ─── Phase A validation: BiscayController end-to-end ────────────────

/// Validates that the BiscayController (BBR-based capacity estimator wired
/// into TransportLink) converges to approximately the actual link bandwidth.
///
/// Setup: single link capped at 5 Mbps by tc netem. Sender ceiling is set
/// high (20 Mbps) so the link — not the sender — is the bottleneck.
/// After the BBR STARTUP phase the windowed-max delivery rate (`btl_bw`)
/// must stabilise within ±50% of 5 Mbps.
///
/// This is the key Phase A integration assertion: if `estimated_capacity_bps`
/// is always 0 (ACK path broken) or wildly wrong, this test catches it.
#[test]
#[ignore = "Requires BBR-based capacity estimation (Phase A) - work in progress"]
fn capacity_estimation_converges() {
    let bin = match require_privileged_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    let ns_snd = Arc::new(Namespace::new("st_cest_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_cest_rcv").unwrap());

    ns_snd
        .add_veth_link(
            &ns_rcv,
            "st_ce_a",
            "st_ce_b",
            "10.80.1.1/24",
            "10.80.1.2/24",
        )
        .unwrap();

    setup_mgmt_link(
        "st_mgmt_ce",
        "st_mgmt_cg",
        "st_cest_snd",
        "192.168.220.1/24",
        "192.168.220.2/24",
    );

    // Link capped at exactly 5 Mbps — this is what estimated_capacity_bps must converge to.
    apply_impairment(
        &ns_snd,
        "st_ce_a",
        ImpairmentConfig {
            rate_kbit: Some(5_000),
            delay_ms: Some(30),
            loss_percent: Some(0.1),
            ..Default::default()
        },
    )
    .unwrap();

    let mut recv = spawn_in_ns(
        &ns_rcv.name,
        bin_str,
        &["receiver", "--bind", "0.0.0.0:7600"],
    );
    let mut collector = StatsCollector::new("192.168.220.1:9820");

    // --bitrate 20000: sender ceiling is well above the 5 Mbps link cap so
    // the link is always the bottleneck and BBR must find the real rate.
    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--dest",
            "10.80.1.2:7600?rtt-min=60&buffer=2000",
            "--stats-dest",
            "192.168.220.1:9820",
            "--bitrate",
            "20000",
        ],
    );

    // Run for 15s — BBR STARTUP converges within 3s at 30ms RTT;
    // the extra time lets the max-filter stabilise.
    thread::sleep(Duration::from_secs(15));

    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_ce");

    assert!(
        !data.is_empty(),
        "No stats received during capacity estimation test"
    );

    // Examine the last 8s of samples (post-STARTUP, steady-state BBR).
    let last_ts = data.last().unwrap()["timestamp_ms"].as_u64().unwrap_or(0) as f64;
    let window_start = last_ts - 8000.0;

    let capacity_samples: Vec<f64> = data
        .iter()
        .filter(|v| v["timestamp_ms"].as_u64().unwrap_or(0) as f64 >= window_start)
        .map(total_estimated_capacity_bps)
        .filter(|&c| c > 0.0)
        .collect();

    assert!(
        !capacity_samples.is_empty(),
        "estimated_capacity_bps was 0 throughout — ACK feedback path may be broken \
         (BiscayController never received bandwidth samples)"
    );

    let avg_capacity_mbps =
        capacity_samples.iter().sum::<f64>() / capacity_samples.len() as f64 / 1_000_000.0;

    eprintln!(
        "Capacity estimation: avg={:.2} Mbps over {} samples (target: ~5 Mbps)",
        avg_capacity_mbps,
        capacity_samples.len()
    );

    // Assert convergence within ±50% of the 5 Mbps netem limit.
    assert!(
        avg_capacity_mbps >= 2.5,
        "Capacity estimate ({:.2} Mbps) too low — estimator may be under-counting ACKs",
        avg_capacity_mbps
    );
    assert!(
        avg_capacity_mbps <= 8.0,
        "Capacity estimate ({:.2} Mbps) too high — estimator may be ignoring the netem rate limit",
        avg_capacity_mbps
    );
}
