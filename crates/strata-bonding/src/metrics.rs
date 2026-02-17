//! # Prometheus Metrics
//!
//! Renders bonding link metrics in Prometheus text exposition format
//! and provides a lightweight HTTP server for scraping.

use crate::net::interface::LinkMetrics;
use std::collections::HashMap;
use std::fmt::Write;
use std::io::{Read, Write as IoWrite};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Render a set of link metrics as Prometheus text exposition format.
pub fn render_prometheus(links: &HashMap<usize, LinkMetrics>) -> String {
    let mut out = String::with_capacity(2048);

    // ── Per-link gauges ─────────────────────────────────────────

    writeln!(
        out,
        "# HELP strata_link_rtt_ms Smoothed RTT in milliseconds."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_rtt_ms gauge").unwrap();
    for (id, m) in links {
        writeln!(
            out,
            "strata_link_rtt_ms{{link_id=\"{id}\"}} {:.3}",
            m.rtt_ms
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_capacity_bps Estimated link capacity in bits per second."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_capacity_bps gauge").unwrap();
    for (id, m) in links {
        writeln!(
            out,
            "strata_link_capacity_bps{{link_id=\"{id}\"}} {:.0}",
            m.capacity_bps
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_loss_rate Observed packet loss rate (0.0-1.0)."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_loss_rate gauge").unwrap();
    for (id, m) in links {
        writeln!(
            out,
            "strata_link_loss_rate{{link_id=\"{id}\"}} {:.6}",
            m.loss_rate
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_observed_bps Actual throughput in bits per second."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_observed_bps gauge").unwrap();
    for (id, m) in links {
        writeln!(
            out,
            "strata_link_observed_bps{{link_id=\"{id}\"}} {:.0}",
            m.observed_bps
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_bytes_sent_total Total bytes sent on this link."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_bytes_sent_total counter").unwrap();
    for (id, m) in links {
        writeln!(
            out,
            "strata_link_bytes_sent_total{{link_id=\"{id}\"}} {}",
            m.observed_bytes
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_queue_depth Current packets in DWRR queue."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_queue_depth gauge").unwrap();
    for (id, m) in links {
        writeln!(
            out,
            "strata_link_queue_depth{{link_id=\"{id}\"}} {}",
            m.queue_depth
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_alive Whether the link is currently alive (1) or dead (0)."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_alive gauge").unwrap();
    for (id, m) in links {
        let v = if m.alive { 1 } else { 0 };
        writeln!(out, "strata_link_alive{{link_id=\"{id}\"}} {v}").unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_phase Lifecycle phase encoded as integer (0=init..6=reset)."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_phase gauge").unwrap();
    for (id, m) in links {
        let phase_num = phase_to_u8(&m.phase);
        writeln!(
            out,
            "strata_link_phase{{link_id=\"{id}\",phase=\"{}\"}} {phase_num}",
            m.phase.as_str()
        )
        .unwrap();
    }

    // ── Aggregate metrics ───────────────────────────────────────

    let alive_count = links.values().filter(|m| m.alive).count();
    // `+ 0.0` normalizes negative zero to positive zero for clean formatting.
    let total_capacity: f64 = links
        .values()
        .filter(|m| m.alive)
        .map(|m| m.capacity_bps)
        .sum::<f64>()
        + 0.0;
    let total_observed: f64 = links
        .values()
        .filter(|m| m.alive)
        .map(|m| m.observed_bps)
        .sum::<f64>()
        + 0.0;

    writeln!(
        out,
        "# HELP strata_links_total Total number of configured links."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_links_total gauge").unwrap();
    writeln!(out, "strata_links_total {}", links.len()).unwrap();

    writeln!(
        out,
        "# HELP strata_links_alive Number of links currently alive."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_links_alive gauge").unwrap();
    writeln!(out, "strata_links_alive {alive_count}").unwrap();

    writeln!(
        out,
        "# HELP strata_total_capacity_bps Aggregate capacity of alive links."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_total_capacity_bps gauge").unwrap();
    writeln!(out, "strata_total_capacity_bps {total_capacity:.0}").unwrap();

    writeln!(
        out,
        "# HELP strata_total_observed_bps Aggregate observed throughput."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_total_observed_bps gauge").unwrap();
    writeln!(out, "strata_total_observed_bps {total_observed:.0}").unwrap();

    out
}

fn phase_to_u8(phase: &crate::net::interface::LinkPhase) -> u8 {
    use crate::net::interface::LinkPhase;
    match phase {
        LinkPhase::Init => 0,
        LinkPhase::Probe => 1,
        LinkPhase::Warm => 2,
        LinkPhase::Live => 3,
        LinkPhase::Degrade => 4,
        LinkPhase::Cooldown => 5,
        LinkPhase::Reset => 6,
    }
}

/// A lightweight HTTP server that serves `/metrics` for Prometheus scraping.
///
/// Runs in a background thread, reading from a shared `Arc<Mutex<HashMap>>`
/// that is updated by the `BondingRuntime`.
pub struct MetricsServer {
    running: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    addr: SocketAddr,
}

impl MetricsServer {
    /// Start the metrics server on the given address.
    ///
    /// `metrics_source` should be the same `Arc<Mutex<HashMap>>` returned
    /// by `BondingRuntime::metrics_handle()`.
    pub fn start(
        bind_addr: SocketAddr,
        metrics_source: Arc<Mutex<HashMap<usize, LinkMetrics>>>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind_addr)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let running_clone = running.clone();

        let handle = thread::Builder::new()
            .name("strata-metrics".into())
            .spawn(move || {
                serve_loop(listener, metrics_source, running_clone);
            })
            .map_err(std::io::Error::other)?;

        Ok(MetricsServer {
            running,
            handle: Some(handle),
            addr,
        })
    }

    /// The address the server is actually listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Gracefully stop the server.
    pub fn stop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve_loop(
    listener: TcpListener,
    metrics_source: Arc<Mutex<HashMap<usize, LinkMetrics>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
) {
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let snap = metrics_source
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                handle_connection(stream, &snap);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_connection(mut stream: TcpStream, metrics: &HashMap<usize, LinkMetrics>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    // Read enough of the request to determine the path
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let request = String::from_utf8_lossy(&buf[..n]);

    // Only serve GET /metrics
    if request.starts_with("GET /metrics") {
        let body = render_prometheus(metrics);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
    } else {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(response.as_bytes());
    }
}

/// Serialize link metrics to the same JSON shape the agent telemetry expects.
///
/// Produces `{"links": [...], "timestamp_ms": ...}` — identical to what
/// `strata-node` emits from GStreamer bus messages.  Useful for
/// standalone (non-GStreamer) deployments that need to feed the agent
/// telemetry loop directly.
pub fn to_telemetry_json(links: &HashMap<usize, LinkMetrics>) -> String {
    let mut arr: Vec<serde_json::Value> = links
        .iter()
        .map(|(&id, m)| {
            let os_up: i64 = match m.os_up {
                Some(true) => 1,
                Some(false) => 0,
                None => -1,
            };
            serde_json::json!({
                "id": id,
                "rtt_us": (m.rtt_ms * 1000.0) as u64,
                "loss_rate": m.loss_rate,
                "capacity_bps": m.capacity_bps.round() as u64,
                "sent_bytes": m.observed_bytes,
                "observed_bps": m.observed_bps.round() as u64,
                "interface": m.iface.as_deref().unwrap_or("unknown"),
                "alive": m.alive,
                "phase": m.phase.as_str(),
                "os_up": os_up,
                "link_kind": m.link_kind.as_deref().unwrap_or(""),
            })
        })
        .collect();
    arr.sort_by_key(|v| v.get("id").and_then(|v| v.as_u64()).unwrap_or(0));

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    serde_json::json!({
        "links": arr,
        "timestamp_ms": now_ms,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::interface::LinkPhase;

    fn sample_metrics() -> HashMap<usize, LinkMetrics> {
        let mut map = HashMap::new();
        map.insert(
            0,
            LinkMetrics {
                rtt_ms: 25.5,
                capacity_bps: 5_000_000.0,
                loss_rate: 0.02,
                observed_bps: 3_000_000.0,
                observed_bytes: 100_000,
                queue_depth: 3,
                max_queue: 100,
                alive: true,
                phase: LinkPhase::Live,
                os_up: Some(true),
                mtu: Some(1500),
                iface: Some("wwan0".into()),
                link_kind: Some("cellular".into()),
            },
        );
        map.insert(
            1,
            LinkMetrics {
                rtt_ms: 50.0,
                capacity_bps: 2_000_000.0,
                loss_rate: 0.05,
                observed_bps: 1_500_000.0,
                observed_bytes: 50_000,
                queue_depth: 7,
                max_queue: 100,
                alive: true,
                phase: LinkPhase::Probe,
                os_up: Some(true),
                mtu: Some(1400),
                iface: Some("wwan1".into()),
                link_kind: Some("cellular".into()),
            },
        );
        map
    }

    #[test]
    fn render_prometheus_contains_help_lines() {
        let metrics = sample_metrics();
        let out = render_prometheus(&metrics);
        assert!(out.contains("# HELP strata_link_rtt_ms"));
        assert!(out.contains("# TYPE strata_link_rtt_ms gauge"));
        assert!(out.contains("# HELP strata_link_capacity_bps"));
        assert!(out.contains("# HELP strata_link_loss_rate"));
        assert!(out.contains("# HELP strata_link_alive"));
        assert!(out.contains("# HELP strata_links_alive"));
        assert!(out.contains("# HELP strata_total_capacity_bps"));
    }

    #[test]
    fn render_prometheus_per_link_values() {
        let metrics = sample_metrics();
        let out = render_prometheus(&metrics);
        // Link 0
        assert!(out.contains("strata_link_rtt_ms{link_id=\"0\"} 25.500"));
        assert!(out.contains("strata_link_capacity_bps{link_id=\"0\"} 5000000"));
        assert!(out.contains("strata_link_loss_rate{link_id=\"0\"} 0.020000"));
        assert!(out.contains("strata_link_alive{link_id=\"0\"} 1"));
        assert!(out.contains("strata_link_bytes_sent_total{link_id=\"0\"} 100000"));
        // Link 1
        assert!(out.contains("strata_link_rtt_ms{link_id=\"1\"} 50.000"));
        assert!(out.contains("strata_link_alive{link_id=\"1\"} 1"));
    }

    #[test]
    fn render_prometheus_aggregate_values() {
        let metrics = sample_metrics();
        let out = render_prometheus(&metrics);
        assert!(out.contains("strata_links_total 2"));
        assert!(out.contains("strata_links_alive 2"));
        assert!(out.contains("strata_total_capacity_bps 7000000"));
        assert!(out.contains("strata_total_observed_bps 4500000"));
    }

    #[test]
    fn render_prometheus_dead_link_excluded_from_alive() {
        let mut metrics = sample_metrics();
        metrics.get_mut(&1).unwrap().alive = false;
        let out = render_prometheus(&metrics);
        assert!(out.contains("strata_links_alive 1"));
        assert!(out.contains("strata_link_alive{link_id=\"1\"} 0"));
        // Total capacity should only include alive link
        assert!(out.contains("strata_total_capacity_bps 5000000"));
    }

    #[test]
    fn render_prometheus_empty_links() {
        let metrics = HashMap::new();
        let out = render_prometheus(&metrics);
        assert!(out.contains("strata_links_total 0"));
        assert!(out.contains("strata_links_alive 0"));
        assert!(out.contains("strata_total_capacity_bps 0"));
    }

    #[test]
    fn render_prometheus_phase_label() {
        let metrics = sample_metrics();
        let out = render_prometheus(&metrics);
        assert!(out.contains("strata_link_phase{link_id=\"0\",phase=\"live\"} 3"));
        assert!(out.contains("strata_link_phase{link_id=\"1\",phase=\"probe\"} 1"));
    }

    #[test]
    fn phase_to_u8_all_variants() {
        assert_eq!(phase_to_u8(&LinkPhase::Init), 0);
        assert_eq!(phase_to_u8(&LinkPhase::Probe), 1);
        assert_eq!(phase_to_u8(&LinkPhase::Warm), 2);
        assert_eq!(phase_to_u8(&LinkPhase::Live), 3);
        assert_eq!(phase_to_u8(&LinkPhase::Degrade), 4);
        assert_eq!(phase_to_u8(&LinkPhase::Cooldown), 5);
        assert_eq!(phase_to_u8(&LinkPhase::Reset), 6);
    }

    #[test]
    fn metrics_server_serves_prometheus() {
        let metrics = Arc::new(Mutex::new(sample_metrics()));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = MetricsServer::start(addr, metrics).expect("server should start");
        let actual_addr = server.addr();

        // Give server a moment to start
        thread::sleep(Duration::from_millis(100));

        // Make an HTTP request
        let mut stream = TcpStream::connect(actual_addr).expect("should connect to metrics server");
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("text/plain"));
        assert!(response.contains("strata_link_rtt_ms"));
        assert!(response.contains("strata_links_alive 2"));

        server.stop();
    }

    #[test]
    fn metrics_server_404_on_wrong_path() {
        let metrics = Arc::new(Mutex::new(HashMap::new()));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = MetricsServer::start(addr, metrics).expect("server should start");
        let actual_addr = server.addr();

        thread::sleep(Duration::from_millis(100));

        let mut stream = TcpStream::connect(actual_addr).expect("should connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);

        assert!(response.starts_with("HTTP/1.1 404"));

        server.stop();
    }

    #[test]
    fn metrics_server_dynamic_updates() {
        let metrics = Arc::new(Mutex::new(HashMap::new()));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = MetricsServer::start(addr, metrics.clone()).expect("server should start");
        let actual_addr = server.addr();

        thread::sleep(Duration::from_millis(100));

        // First request — empty
        let body1 = fetch_metrics(actual_addr);
        assert!(body1.contains("strata_links_total 0"));

        // Add a link
        {
            let mut m = metrics.lock().unwrap();
            m.insert(
                0,
                LinkMetrics {
                    rtt_ms: 10.0,
                    alive: true,
                    ..LinkMetrics::default()
                },
            );
        }

        // Second request — should see the link
        let body2 = fetch_metrics(actual_addr);
        assert!(body2.contains("strata_links_total 1"));
        assert!(body2.contains("strata_link_rtt_ms{link_id=\"0\"} 10.000"));

        server.stop();
    }

    fn fetch_metrics(addr: SocketAddr) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }
}
