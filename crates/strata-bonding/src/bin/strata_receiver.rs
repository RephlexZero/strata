//! # Strata Cloud Receiver
//!
//! Standalone cloud gateway binary that receives bonded transport streams
//! without requiring GStreamer. Accepts UDP traffic on multiple links,
//! performs multi-link reassembly (FEC + ARQ + jitter buffer), and outputs
//! the recovered MPEG-TS stream to file, stdout, or an RTMP relay.
//!
//! ## Usage
//!
//! ```bash
//! # Monitor mode (log stats, discard output)
//! strata-receiver --bind 0.0.0.0:5000,0.0.0.0:5002,0.0.0.0:5004
//!
//! # Write to file
//! strata-receiver --bind 0.0.0.0:5000,0.0.0.0:5002 --output stream.ts
//!
//! # Relay to RTMP (pipes to ffmpeg)
//! strata-receiver --bind 0.0.0.0:5000,0.0.0.0:5002 \
//!   --relay-url rtmp://a.rtmp.youtube.com/live2/KEY
//!
//! # Prometheus metrics
//! strata-receiver --bind 0.0.0.0:5000 --metrics-port 9090
//! ```

use std::io::Write;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use strata_bonding::metrics::render_receiver_prometheus;
use strata_bonding::receiver::transport::TransportBondingReceiver;

fn main() -> anyhow::Result<()> {
    // ── Logging ─────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .compact()
        .init();

    // ── Parse CLI ───────────────────────────────────────────────
    let args = parse_args()?;

    tracing::info!(
        bind = ?args.bind_addrs,
        latency_ms = args.latency_ms,
        relay = ?args.relay_url,
        output = ?args.output,
        metrics_port = ?args.metrics_port,
        "strata-receiver starting"
    );

    // ── Receiver ────────────────────────────────────────────────
    let rcv = TransportBondingReceiver::new(Duration::from_millis(args.latency_ms));

    for addr in &args.bind_addrs {
        rcv.add_link(*addr)?;
        tracing::info!(%addr, "link bound");
    }

    // ── Metrics server (optional) ───────────────────────────────
    let stats_handle = rcv.stats_handle();
    if let Some(port) = args.metrics_port {
        let stats_for_metrics = stats_handle.clone();
        std::thread::Builder::new()
            .name("metrics".into())
            .spawn(move || {
                if let Err(e) = run_metrics_server(port, stats_for_metrics) {
                    tracing::error!(error = %e, "metrics server failed");
                }
            })?;
    }

    // ── Graceful shutdown ───────────────────────────────────────
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::handle(move || {
            tracing::info!("shutting down...");
            running.store(false, Ordering::Relaxed);
        });
    }

    // ── Output sink ─────────────────────────────────────────────
    let mut sink: Box<dyn OutputSink> = match (&args.relay_url, &args.output) {
        (Some(url), _) => Box::new(FfmpegRelay::start(url)?),
        (None, Some(path)) => Box::new(FileSink::open(path)?),
        (None, None) => Box::new(NullSink::new()),
    };

    // ── Main receive loop ───────────────────────────────────────
    let mut total_bytes: u64 = 0;
    let mut total_packets: u64 = 0;
    let mut last_stats_log = std::time::Instant::now();
    let stats_interval = Duration::from_secs(5);

    while running.load(Ordering::Relaxed) {
        match rcv.output_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(payload) => {
                total_bytes += payload.len() as u64;
                total_packets += 1;
                if let Err(e) = sink.write(&payload) {
                    tracing::error!(error = %e, "output write failed");
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        // Periodic stats logging
        if last_stats_log.elapsed() >= stats_interval {
            let stats = rcv.get_stats();
            tracing::info!(
                packets = total_packets,
                bytes = total_bytes,
                queue_depth = stats.queue_depth,
                lost = stats.lost_packets,
                late = stats.late_packets,
                duplicates = stats.duplicate_packets,
                "receiver stats"
            );
            last_stats_log = std::time::Instant::now();
        }
    }

    // ── Cleanup ─────────────────────────────────────────────────
    drop(sink);
    tracing::info!(total_packets, total_bytes, "strata-receiver stopped");

    Ok(())
}

// ─── CLI Parsing ────────────────────────────────────────────────────────────

struct Args {
    bind_addrs: Vec<SocketAddr>,
    latency_ms: u64,
    relay_url: Option<String>,
    output: Option<String>,
    metrics_port: Option<u16>,
}

fn parse_args() -> anyhow::Result<Args> {
    let args: Vec<String> = std::env::args().collect();
    let mut bind_addrs = Vec::new();
    let mut latency_ms = 200u64;
    let mut relay_url = None;
    let mut output = None;
    let mut metrics_port = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" | "-b" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--bind requires a value"))?;
                for part in val.split(',') {
                    let addr: SocketAddr = part.trim().parse().map_err(|e| {
                        anyhow::anyhow!("invalid bind address '{}': {}", part.trim(), e)
                    })?;
                    bind_addrs.push(addr);
                }
            }
            "--latency" | "-l" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--latency requires a value"))?;
                latency_ms = val
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid latency '{}': {}", val, e))?;
            }
            "--relay-url" | "-r" => {
                i += 1;
                relay_url = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--relay-url requires a value"))?
                        .clone(),
                );
            }
            "--output" | "-o" => {
                i += 1;
                output = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--output requires a value"))?
                        .clone(),
                );
            }
            "--metrics-port" | "-m" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--metrics-port requires a value"))?;
                metrics_port = Some(
                    val.parse()
                        .map_err(|e| anyhow::anyhow!("invalid port '{}': {}", val, e))?,
                );
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                anyhow::bail!("unknown argument: {other}\nRun with --help for usage.");
            }
        }
        i += 1;
    }

    // Fallback: env vars
    if bind_addrs.is_empty() {
        if let Ok(val) = std::env::var("BIND_ADDRS") {
            for part in val.split(',') {
                bind_addrs.push(part.trim().parse()?);
            }
        }
    }
    if let Ok(val) = std::env::var("LATENCY_MS") {
        latency_ms = val.parse().unwrap_or(latency_ms);
    }
    if relay_url.is_none() {
        relay_url = std::env::var("RELAY_URL").ok().filter(|s| !s.is_empty());
    }
    if metrics_port.is_none() {
        if let Ok(val) = std::env::var("METRICS_PORT") {
            metrics_port = val.parse().ok();
        }
    }

    if bind_addrs.is_empty() {
        anyhow::bail!("no bind addresses specified. Use --bind or BIND_ADDRS env var.\nRun with --help for usage.");
    }

    Ok(Args {
        bind_addrs,
        latency_ms,
        relay_url,
        output,
        metrics_port,
    })
}

fn print_help() {
    eprintln!(
        r#"strata-receiver — Standalone bonded transport cloud receiver

USAGE:
  strata-receiver --bind <ADDR[,ADDR...]> [OPTIONS]

OPTIONS:
  --bind, -b <addrs>      Comma-separated UDP bind addresses (required)
                           e.g. 0.0.0.0:5000,0.0.0.0:5002,0.0.0.0:5004
  --latency, -l <ms>      Jitter buffer latency in ms (default: 200)
  --relay-url, -r <url>   RTMP/RTMPS URL to relay stream via ffmpeg
  --output, -o <path>     Write recovered MPEG-TS to file
  --metrics-port, -m <port> Prometheus metrics on 0.0.0.0:<port>/metrics
  --help, -h              Show this help

ENVIRONMENT VARIABLES:
  BIND_ADDRS     Comma-separated bind addresses (fallback for --bind)
  LATENCY_MS     Jitter buffer latency (fallback for --latency)
  RELAY_URL      RTMP relay URL (fallback for --relay-url)
  METRICS_PORT   Prometheus port (fallback for --metrics-port)
  RUST_LOG       Log level filter (e.g. info, debug, strata_bonding=trace)

EXAMPLES:
  # Monitor mode (logs stats, discards output)
  strata-receiver --bind 0.0.0.0:5000,0.0.0.0:5002,0.0.0.0:5004

  # Relay to YouTube
  strata-receiver --bind 0.0.0.0:5000,0.0.0.0:5002,0.0.0.0:5004 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/STREAM_KEY"

  # Record to file
  strata-receiver --bind 0.0.0.0:5000 --output capture.ts

  # With Prometheus metrics
  strata-receiver --bind 0.0.0.0:5000 --metrics-port 9090
"#
    );
}

// ─── Output Sinks ───────────────────────────────────────────────────────────

trait OutputSink {
    fn write(&mut self, data: &[u8]) -> anyhow::Result<()>;
}

/// Discards output, just logs stats (monitor mode).
struct NullSink;

impl NullSink {
    fn new() -> Self {
        tracing::info!("output: monitor mode (set --relay-url or --output to capture)");
        NullSink
    }
}

impl OutputSink for NullSink {
    fn write(&mut self, _data: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Writes raw MPEG-TS to a file.
struct FileSink {
    file: std::fs::File,
}

impl FileSink {
    fn open(path: &str) -> anyhow::Result<Self> {
        let file = std::fs::File::create(path)?;
        tracing::info!(path, "output: writing MPEG-TS to file");
        Ok(FileSink { file })
    }
}

impl OutputSink for FileSink {
    fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.file.write_all(data)?;
        Ok(())
    }
}

/// Pipes MPEG-TS to ffmpeg for RTMP relay.
struct FfmpegRelay {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
}

impl FfmpegRelay {
    fn start(relay_url: &str) -> anyhow::Result<Self> {
        tracing::info!(url = relay_url, "output: starting ffmpeg RTMP relay");

        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "warning",
                // Input: raw MPEG-TS from stdin
                "-f",
                "mpegts",
                "-i",
                "pipe:0",
                // Copy — no re-encoding
                "-c",
                "copy",
                // Output: FLV over RTMP
                "-f",
                "flv",
                relay_url,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to start ffmpeg: {e} (is ffmpeg installed?)"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open ffmpeg stdin"))?;

        Ok(FfmpegRelay {
            child,
            stdin: Some(stdin),
        })
    }
}

impl OutputSink for FfmpegRelay {
    fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
        if let Some(ref mut stdin) = self.stdin {
            stdin.write_all(data)?;
        }
        Ok(())
    }
}

impl Drop for FfmpegRelay {
    fn drop(&mut self) {
        // Close stdin first to signal EOF, then wait for ffmpeg to exit
        drop(self.stdin.take());
        let _ = self.child.wait();
        tracing::info!("ffmpeg relay stopped");
    }
}

// ─── Metrics Server ─────────────────────────────────────────────────────────

fn run_metrics_server(
    port: u16,
    stats: Arc<std::sync::Mutex<strata_bonding::receiver::aggregator::ReassemblyStats>>,
) -> anyhow::Result<()> {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::net::TcpListener;

    let listener = TcpListener::bind(format!("0.0.0.0:{port}"))?;
    tracing::info!(port, "prometheus metrics server listening");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "metrics accept error");
                continue;
            }
        };

        // Read the HTTP request line (we only care about GET /metrics)
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            continue;
        }

        // Drain remaining headers
        let mut header = String::new();
        loop {
            header.clear();
            match reader.read_line(&mut header) {
                Ok(0) | Err(_) => break,
                Ok(_) if header.trim().is_empty() => break,
                _ => {}
            }
        }

        let body = if request_line.starts_with("GET /metrics") {
            let s = stats.lock().unwrap_or_else(|e| e.into_inner());
            render_receiver_prometheus(&s)
        } else {
            "404 Not Found".to_string()
        };

        let status = if request_line.starts_with("GET /metrics") {
            "200 OK"
        } else {
            "404 Not Found"
        };

        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        let _ = stream.write_all(response.as_bytes());
    }

    Ok(())
}

// ─── Signal Handling ────────────────────────────────────────────────────────

mod ctrlc {
    use std::sync::atomic::{AtomicBool, Ordering};

    static HANDLER_SET: AtomicBool = AtomicBool::new(false);

    pub fn handle(f: impl Fn() + Send + 'static) {
        if HANDLER_SET.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = std::thread::Builder::new()
            .name("signal".into())
            .spawn(move || {
                wait_for_signal();
                f();
            });
    }

    #[cfg(unix)]
    fn wait_for_signal() {
        unsafe {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGINT);
            libc::sigaddset(&mut mask, libc::SIGTERM);
            let mut sig: libc::c_int = 0;
            libc::sigwait(&mask, &mut sig);
        }
    }

    #[cfg(not(unix))]
    fn wait_for_signal() {
        // Fallback: busy-wait checking a flag (shouldn't happen in production)
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
