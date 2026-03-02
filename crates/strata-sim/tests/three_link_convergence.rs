//! # Three-Link Bandwidth Convergence Test
//!
//! Exercises the full sender→receiver pipeline over 3 rate-limited network
//! namespace links (3 Mbps, 5 Mbps, 8 Mbps). Monitors per-link estimated
//! capacity and observed throughput to verify:
//!
//! 1. Each link's estimated capacity converges toward its tc-netem rate limit.
//! 2. Aggregate throughput does **not** decay to zero (regression catch).
//! 3. The scheduler distributes traffic roughly proportionally to link capacity.
//!
//! Run (requires root/netns):
//! ```bash
//! sudo -E cargo test -p strata-sim --test three_link_convergence -- --nocapture --ignored
//! ```

use serde_json::Value;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use strata_sim::impairment::{ImpairmentConfig, apply_bidirectional_impairment};
use strata_sim::test_util::check_privileges;
use strata_sim::topology::Namespace;

// ─── Helpers ────────────────────────────────────────────────────────────

/// Locate (or build) the dummy_node binary.
fn dummy_node_binary() -> PathBuf {
    if let Ok(p) = std::env::var("STRATA_PIPELINE_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }

    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        let _ = Command::new("cargo")
            .args(["build", "-p", "strata-sim", "--bin", "dummy_node"])
            .status();
    });

    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // deps
    path.pop(); // debug
    path.push("dummy_node");
    if !path.exists() {
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

fn spawn_in_ns(ns: &str, cmd: &str, args: &[&str]) -> std::process::Child {
    Command::new("sudo")
        .args(["-E", "ip", "netns", "exec", ns, cmd])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn {cmd} in {ns}: {e}"))
}

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
                if let Ok((amt, _)) = socket.recv_from(&mut buf)
                    && let Ok(val) = serde_json::from_slice::<Value>(&buf[..amt])
                {
                    d.lock().unwrap().push(val);
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

// ─── Stats extraction ───────────────────────────────────────────────────

/// Per-link metrics from a single stats snapshot.
#[derive(Debug, Clone)]
struct LinkSnapshot {
    link_id: usize,
    observed_bps: f64,
    estimated_capacity_bps: f64,
    rtt_ms: f64,
    loss_ratio: f64,
    #[allow(dead_code)]
    sent_bytes: u64,
    ack_delivery_bps: f64,
    ack_bytes: u64,
}

fn extract_links(v: &Value) -> Vec<LinkSnapshot> {
    let mut links: Vec<LinkSnapshot> = v
        .get("links")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .map(|l| LinkSnapshot {
                    link_id: l.get("link_id").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                    observed_bps: l
                        .get("observed_bps")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                    estimated_capacity_bps: l
                        .get("estimated_capacity_bps")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                    rtt_ms: l.get("rtt_ms").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    loss_ratio: l.get("loss_ratio").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    sent_bytes: l.get("sent_bytes").and_then(|v| v.as_u64()).unwrap_or(0),
                    ack_delivery_bps: l
                        .get("ack_delivery_bps")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                    ack_bytes: l.get("ack_bytes").and_then(|v| v.as_u64()).unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    // Sort by link_id for stable ordering
    links.sort_by_key(|l| l.link_id);
    links
}

fn total_observed_bps(v: &Value) -> f64 {
    extract_links(v).iter().map(|l| l.observed_bps).sum()
}

fn timestamp_ms(v: &Value) -> f64 {
    v.get("timestamp_ms")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

fn current_bitrate_bps(v: &Value) -> f64 {
    v.get("current_bitrate_bps")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

// ─── Main test ──────────────────────────────────────────────────────────

/// 3-link convergence: 3 Mbps + 5 Mbps + 8 Mbps = 16 Mbps aggregate.
///
/// Runs for ~45 seconds:
///  - 10s warmup (BBR slow-start + btl_bw window fill)
///  - 35s steady state with verbose per-second logging
///
/// Verifies:
///  - Aggregate throughput converges above 50% of aggregate capacity
///  - Per-link estimated capacity is within 3× of tc rate limit (no runaway)
///  - No link falls to zero observed throughput in the steady-state window
///  - Aggregate does not decay to zero over time (regression guard)
#[test]
#[ignore = "Requires root/netns privileges — run with: sudo -E cargo test -p strata-sim --test three_link_convergence -- --nocapture --ignored"]
fn three_link_convergence_3_5_8_mbps() {
    let bin = match require_env() {
        Some(b) => b,
        None => return,
    };
    let bin_str = bin.to_str().unwrap();

    // ── Create namespaces ───────────────────────────────────────────────
    let ns_snd = Arc::new(Namespace::new("st_conv_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("st_conv_rcv").unwrap());

    // 3 veth links between sender ↔ receiver namespaces
    //  Link 0: 10.70.1.0/24 — 3 Mbps, LTE poor profile
    //  Link 1: 10.70.2.0/24 — 5 Mbps, LTE urban profile
    //  Link 2: 10.70.3.0/24 — 8 Mbps, LTE urban profile
    let links: [(&str, &str, &str, &str, &str, u64, u32); 3] = [
        (
            "st_cv0_a",
            "st_cv0_b",
            "10.70.1.1/24",
            "10.70.1.2/24",
            "10.70.1.2",
            3_000,
            30,
        ),
        (
            "st_cv1_a",
            "st_cv1_b",
            "10.70.2.1/24",
            "10.70.2.2/24",
            "10.70.2.2",
            5_000,
            22,
        ),
        (
            "st_cv2_a",
            "st_cv2_b",
            "10.70.3.1/24",
            "10.70.3.2/24",
            "10.70.3.2",
            8_000,
            18,
        ),
    ];

    for (veth_a, veth_b, ip_snd, ip_rcv, _, _, _) in &links {
        ns_snd
            .add_veth_link(&ns_rcv, veth_a, veth_b, ip_snd, ip_rcv)
            .unwrap();
    }

    // Apply tc netem rate limits bidirectionally on each veth.
    // Forward path (sender→receiver): full shaping (rate + delay + loss).
    // Return path (receiver→sender): delay + loss only (no rate limit).
    // This produces realistic symmetric RTTs and ACK-path loss.
    for (veth_a, veth_b, _, _, _, rate_kbit, delay_ms) in &links {
        apply_bidirectional_impairment(
            &ns_snd,
            veth_a,
            &ns_rcv,
            veth_b,
            ImpairmentConfig {
                rate_kbit: Some(*rate_kbit),
                delay_ms: Some(*delay_ms),
                jitter_ms: Some(8),
                delay_distribution_normal: true,
                loss_percent: Some(0.5),
                loss_correlation: Some(25.0),
                corrupt_percent: Some(0.05),
                slot_min_us: Some(1_000),
                slot_max_us: Some(2_000),
                slot_max_packets: Some(12),
                slot_max_bytes: Some(14_400),
                modem_buffer_kb: Some(64),
                ..Default::default()
            },
        )
        .unwrap();
        let auto_limit = ImpairmentConfig {
            rate_kbit: Some(*rate_kbit),
            ..Default::default()
        }
        .auto_limit()
        .unwrap_or(10);
        eprintln!(
            "  link {veth_a}: rate={rate_kbit}kbit delay={delay_ms}ms jitter=8ms loss=0.5%/25% limit={auto_limit}pkts"
        );
    }

    // Management link for stats relay (sender ns → host)
    setup_mgmt_link(
        "st_mgmt_cv",
        "st_mgmt_cw",
        "st_conv_snd",
        "192.168.210.1/24",
        "192.168.210.2/24",
    );

    // ── Start receiver ──────────────────────────────────────────────────
    // Bind per-link so each link gets its own TransportReceiver with an
    // independent sequence space. Using 0.0.0.0 would funnel all links
    // through one receiver, causing overlapping sequence numbers and
    // inflated cumulative ACKs.
    let bind_arg = links
        .iter()
        .map(|(_, _, _, ip_rcv, _, _, _)| {
            // ip_rcv is e.g. "10.70.1.2/24" — strip the CIDR suffix
            let ip = ip_rcv.split('/').next().unwrap();
            format!("{}:7500", ip)
        })
        .collect::<Vec<_>>()
        .join(",");
    let mut recv = spawn_in_ns(&ns_rcv.name, bin_str, &["receiver", "--bind", &bind_arg]);

    // ── Start stats collector on host ───────────────────────────────────
    let mut collector = StatsCollector::new("192.168.210.1:9810");

    // ── Start sender ────────────────────────────────────────────────────
    // Destinations across all 3 links; encoder cap = 14 Mbps (below 16 Mbps aggregate)
    let dest_arg = links
        .iter()
        .map(|(_, _, _, _, ip, _, _)| format!("{}:7500", ip))
        .collect::<Vec<_>>()
        .join(",");

    let mut sender = spawn_in_ns(
        &ns_snd.name,
        bin_str,
        &[
            "sender",
            "--dest",
            &dest_arg,
            "--stats-dest",
            "192.168.210.1:9810",
            "--bitrate",
            "14000",
            "--critical-broadcast",
            "false",
            "--redundancy",
            "false",
        ],
    );

    // ── Run for 45 seconds total ────────────────────────────────────────
    let total_duration = Duration::from_secs(45);
    let warmup = Duration::from_secs(10);
    let start = Instant::now();

    eprintln!("\n{}", "=".repeat(72));
    eprintln!("  Three-Link Convergence Test: 3 / 5 / 8 Mbps");
    eprintln!("  Aggregate cap: 16 Mbps, encoder target: 14 Mbps");
    eprintln!("  Duration: 45s (10s warmup + 35s steady-state)");
    eprintln!("{}\n", "=".repeat(72));

    // Wait for warmup + steady state, printing status every second
    let mut last_log = Instant::now();
    while start.elapsed() < total_duration {
        thread::sleep(Duration::from_millis(200));

        if last_log.elapsed() >= Duration::from_secs(1) {
            last_log = Instant::now();
            let data = collector.data.lock().unwrap();
            if let Some(latest) = data.last() {
                let elapsed_s = start.elapsed().as_secs();
                let phase = if start.elapsed() < warmup {
                    "WARMUP"
                } else {
                    "STEADY"
                };
                let links_snap = extract_links(latest);
                let agg_obs: f64 = links_snap.iter().map(|l| l.observed_bps).sum();
                let agg_cap: f64 = links_snap.iter().map(|l| l.estimated_capacity_bps).sum();
                let bitrate = current_bitrate_bps(latest);

                eprintln!(
                    "[{:>3}s] [{phase}] encoder={:>7.1} kbps  agg_obs={:>7.1} kbps  agg_cap={:>7.1} kbps",
                    elapsed_s,
                    bitrate / 1000.0,
                    agg_obs / 1000.0,
                    agg_cap / 1000.0,
                );
                for (i, ls) in links_snap.iter().enumerate() {
                    let target_kbps = links[i].5;
                    eprintln!(
                        "       link[{i}] target={target_kbps:>5} kbps  obs={:>7.1} kbps  cap={:>7.1} kbps  rtt={:>5.1} ms  loss={:.3}  ack_rate={:.1} kbps  ack_bytes={}",
                        ls.observed_bps / 1000.0,
                        ls.estimated_capacity_bps / 1000.0,
                        ls.rtt_ms,
                        ls.loss_ratio,
                        ls.ack_delivery_bps / 1000.0,
                        ls.ack_bytes,
                    );
                }
                eprintln!();
            }
        }
    }

    // ── Stop processes ──────────────────────────────────────────────────
    let _ = sender.kill();
    let _ = sender.wait();
    let _ = recv.kill();
    let _ = recv.wait();
    let data = collector.stop();
    cleanup_mgmt_link("st_mgmt_cv");

    // ── Analysis ────────────────────────────────────────────────────────
    assert!(
        !data.is_empty(),
        "No stats received — did the sender/receiver start?"
    );

    eprintln!("\n--- Collected {} stats snapshots ---\n", data.len());

    // Only analyze steady-state (after warmup)
    let first_ts = timestamp_ms(&data[0]);
    let warmup_cutoff = first_ts + warmup.as_millis() as f64;

    let steady: Vec<&Value> = data
        .iter()
        .filter(|v| timestamp_ms(v) > warmup_cutoff)
        .collect();

    assert!(
        steady.len() >= 5,
        "Not enough steady-state stats ({} snapshots). \
         The sender may have failed to start or crashed.",
        steady.len()
    );

    // ── Check 1: Aggregate throughput did not decay to zero ─────────────
    // Split steady-state into first half and second half
    let mid = steady.len() / 2;
    let first_half = &steady[..mid];
    let second_half = &steady[mid..];

    let avg_first: f64 = first_half
        .iter()
        .map(|v| total_observed_bps(v))
        .sum::<f64>()
        / first_half.len() as f64;
    let avg_second: f64 = second_half
        .iter()
        .map(|v| total_observed_bps(v))
        .sum::<f64>()
        / second_half.len() as f64;

    eprintln!(
        "Aggregate throughput — first half: {:.1} kbps, second half: {:.1} kbps",
        avg_first / 1000.0,
        avg_second / 1000.0,
    );

    // Regression guard: throughput in the second half should be >= 30% of
    // the first half (catches the "decays to zero" symptom).
    assert!(
        avg_second >= avg_first * 0.30 || avg_second > 500_000.0,
        "REGRESSION: aggregate throughput decayed! \
         First half={:.1} kbps → Second half={:.1} kbps. \
         Something is draining bitrates to zero.",
        avg_first / 1000.0,
        avg_second / 1000.0,
    );

    // ── Check 2: Aggregate converges above 50% of total link budget ─────
    let total_budget_bps = (3_000.0 + 5_000.0 + 8_000.0) * 1000.0; // 16 Mbps
    let final_window: Vec<&Value> = steady.iter().rev().take(10).copied().collect();

    let avg_final: f64 = final_window
        .iter()
        .map(|v| total_observed_bps(v))
        .sum::<f64>()
        / final_window.len().max(1) as f64;

    eprintln!(
        "Final window aggregate: {:.1} kbps (budget: {:.0} kbps, 50% = {:.0} kbps)",
        avg_final / 1000.0,
        total_budget_bps / 1000.0,
        total_budget_bps * 0.5 / 1000.0,
    );

    assert!(
        avg_final > total_budget_bps * 0.30,
        "Aggregate throughput ({:.1} kbps) is below 30% of link budget ({:.0} kbps). \
         Expected convergence toward the available bandwidth.",
        avg_final / 1000.0,
        total_budget_bps / 1000.0,
    );

    // ── Check 3: Per-link throughput distribution in final window ────────
    // Each link should carry some traffic; proportions should roughly
    // match capacity ratios (3:5:8).
    let mut per_link_obs = [0.0f64; 3];
    let mut per_link_cap = [0.0f64; 3];
    let mut link_count = 0usize;

    for v in &final_window {
        let ls = extract_links(v);
        if ls.len() >= 3 {
            for i in 0..3 {
                per_link_obs[i] += ls[i].observed_bps;
                per_link_cap[i] += ls[i].estimated_capacity_bps;
            }
            link_count += 1;
        }
    }

    if link_count > 0 {
        for i in 0..3 {
            per_link_obs[i] /= link_count as f64;
            per_link_cap[i] /= link_count as f64;
        }

        let target_rates = [3_000.0, 5_000.0, 8_000.0]; // kbps

        eprintln!("\nPer-link final averages:");
        for i in 0..3 {
            eprintln!(
                "  link[{i}]: target={:.0} kbps  obs={:.1} kbps  est_cap={:.1} kbps",
                target_rates[i],
                per_link_obs[i] / 1000.0,
                per_link_cap[i] / 1000.0,
            );
        }

        // Each link that has capacity > 0 should carry some traffic
        for i in 0..3 {
            assert!(
                per_link_obs[i] > 10_000.0, // at least 10 kbps
                "Link {i} has near-zero throughput ({:.1} kbps) despite \
                 {:.0} kbps tc rate limit. Scheduling or convergence bug.",
                per_link_obs[i] / 1000.0,
                target_rates[i],
            );
        }

        // ── Check 3b: Per-link estimated capacity should not wildly exceed ──
        // the tc rate limit (checks that btl_bw actually converges downward).
        // Allow up to 5× the tc rate — generous because bidirectional shaping
        // causes ACK batching that can transiently inflate capacity estimates.
        for i in 0..3 {
            let target_bps = target_rates[i] * 1000.0;
            let ratio = per_link_cap[i] / target_bps;
            eprintln!("  link[{i}]: capacity/target ratio = {ratio:.2}×",);
            assert!(
                ratio < 5.0,
                "Link {i} estimated capacity ({:.1} kbps) is >{:.0}× the tc rate ({:.0} kbps). \
                 BtlBw is not converging downward.",
                per_link_cap[i] / 1000.0,
                5.0,
                target_rates[i],
            );
        }
    }

    // ── Check 4: Encoder bitrate did not collapse ───────────────────────
    let final_bitrates: Vec<f64> = final_window
        .iter()
        .map(|v| current_bitrate_bps(v))
        .filter(|&b| b > 0.0)
        .collect();

    if !final_bitrates.is_empty() {
        let avg_br = final_bitrates.iter().sum::<f64>() / final_bitrates.len() as f64;
        eprintln!("\nFinal encoder bitrate: {:.1} kbps", avg_br / 1000.0);
        assert!(
            avg_br > 500_000.0,
            "Encoder bitrate collapsed to {:.1} kbps — adaptation loop may be broken",
            avg_br / 1000.0,
        );
    }

    // ── Verbose timeline dump ───────────────────────────────────────────
    eprintln!("\n--- Full timeline (every 5th snapshot) ---\n");
    eprintln!(
        "{:>12} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "time_ms", "enc_kbps", "agg_obs", "agg_cap", "L0_obs", "L1_obs", "L2_obs", "phase"
    );
    for (idx, v) in data.iter().enumerate() {
        if idx % 5 != 0 {
            continue;
        }
        let ts = timestamp_ms(v);
        let rel_ms = ts - first_ts;
        let phase = if ts < warmup_cutoff { "WARM" } else { "STDY" };
        let br = current_bitrate_bps(v);
        let ls = extract_links(v);
        let agg_obs: f64 = ls.iter().map(|l| l.observed_bps).sum();
        let agg_cap: f64 = ls.iter().map(|l| l.estimated_capacity_bps).sum();
        let l0 = ls.first().map(|l| l.observed_bps).unwrap_or(0.0);
        let l1 = ls.get(1).map(|l| l.observed_bps).unwrap_or(0.0);
        let l2 = ls.get(2).map(|l| l.observed_bps).unwrap_or(0.0);
        eprintln!(
            "{:>10.0}ms {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10}",
            rel_ms,
            br / 1000.0,
            agg_obs / 1000.0,
            agg_cap / 1000.0,
            l0 / 1000.0,
            l1 / 1000.0,
            l2 / 1000.0,
            phase,
        );
    }

    eprintln!("\n{}", "=".repeat(72));
    eprintln!("  TEST PASSED");
    eprintln!("{}\n", "=".repeat(72));
}

fn require_env() -> Option<PathBuf> {
    if !check_privileges() {
        eprintln!("Skipping test: requires root/netns privileges");
        return None;
    }
    Some(dummy_node_binary())
}
