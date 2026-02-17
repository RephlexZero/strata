//! # Strata Cloud Receiver
//!
//! Standalone receiver binary for the Strata bonded transport protocol.
//! Receives packets from multiple UDP links, reassembles via jitter
//! buffer, and writes the output to stdout, a file, or an RTMP relay.
//!
//! This binary does NOT require GStreamer — it uses `strata-bonding`
//! directly. For GStreamer-based receiving, use `strata-node receiver`.
//!
//! ## Usage
//!
//! ```text
//! strata-receiver --bind 0.0.0.0:5000,0.0.0.0:5002 --output capture.ts
//! strata-receiver --bind 0.0.0.0:5000 --metrics-addr 0.0.0.0:9090
//! strata-receiver --bind 0.0.0.0:5000  # output to stdout
//! ```

use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use strata_bonding::metrics::MetricsServer;
use strata_bonding::receiver::transport::TransportBondingReceiver;

fn main() {
    strata_bonding::init();

    let args: Vec<String> = std::env::args().collect();
    let config = match parse_args(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("Error: {msg}");
            eprint!("{HELP}");
            std::process::exit(1);
        }
    };

    if config.show_help {
        eprint!("{HELP}");
        return;
    }

    if config.bind_addrs.is_empty() {
        eprintln!("Error: at least one --bind address is required");
        eprint!("{HELP}");
        std::process::exit(1);
    }

    // ── Receiver ────────────────────────────────────────────────
    let latency_ms = config.latency_ms.unwrap_or(50);
    let rcv = TransportBondingReceiver::new(Duration::from_millis(latency_ms));

    for addr in &config.bind_addrs {
        rcv.add_link(*addr)
            .unwrap_or_else(|e| panic!("Failed to bind link {addr}: {e}"));
        eprintln!("Bound link: {addr}");
    }

    // ── Metrics server (optional) ───────────────────────────────
    let _metrics_server = if let Some(metrics_addr) = config.metrics_addr {
        // Use the receiver's runtime metrics (empty for now — the standalone
        // receiver doesn't own a BondingRuntime, so we create a bare source).
        let metrics_source = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        match MetricsServer::start(metrics_addr, metrics_source) {
            Ok(s) => {
                eprintln!("Metrics server: http://{}/metrics", s.addr());
                Some(s)
            }
            Err(e) => {
                eprintln!("Warning: failed to start metrics server: {e}");
                None
            }
        }
    } else {
        None
    };

    // ── Output ──────────────────────────────────────────────────
    let mut output: Box<dyn Write> = if let Some(ref path) = config.output_file {
        let f = std::fs::File::create(path)
            .unwrap_or_else(|e| panic!("Failed to create output file '{path}': {e}"));
        eprintln!("Writing output to: {path}");
        Box::new(std::io::BufWriter::new(f))
    } else {
        eprintln!("Writing output to stdout");
        Box::new(std::io::BufWriter::new(std::io::stdout().lock()))
    };

    // ── Shutdown handler ────────────────────────────────────────
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    ctrlc::set_handler(move || {
        eprintln!("\nReceived shutdown signal.");
        running_clone.store(false, Ordering::Relaxed);
    })
    .expect("Failed to set Ctrl-C handler");

    // ── Main receive loop ───────────────────────────────────────
    eprintln!("Receiving on {} link(s)...", config.bind_addrs.len());
    let mut total_bytes: u64 = 0;
    let mut total_packets: u64 = 0;

    while running.load(Ordering::Relaxed) {
        match rcv.output_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(data) => {
                total_packets += 1;
                total_bytes += data.len() as u64;
                if output.write_all(&data).is_err() {
                    break; // Broken pipe or disk full
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    // ── Stats ───────────────────────────────────────────────────
    let stats = rcv.get_stats();
    eprintln!(
        "Receiver stopped. Packets: {total_packets}, Bytes: {total_bytes}, \
         Lost: {}, Late: {}, Dup: {}",
        stats.lost_packets, stats.late_packets, stats.duplicate_packets
    );
}

struct Config {
    bind_addrs: Vec<SocketAddr>,
    output_file: Option<String>,
    metrics_addr: Option<SocketAddr>,
    latency_ms: Option<u64>,
    show_help: bool,
}

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut config = Config {
        bind_addrs: Vec::new(),
        output_file: None,
        metrics_addr: None,
        latency_ms: None,
        show_help: false,
    };

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                config.show_help = true;
                return Ok(config);
            }
            "--bind" if i + 1 < args.len() => {
                i += 1;
                for part in args[i].split(',') {
                    let addr: SocketAddr = part
                        .trim()
                        .parse()
                        .map_err(|e| format!("invalid bind address '{part}': {e}"))?;
                    config.bind_addrs.push(addr);
                }
            }
            "--output" if i + 1 < args.len() => {
                i += 1;
                config.output_file = Some(args[i].clone());
            }
            "--metrics-addr" if i + 1 < args.len() => {
                i += 1;
                config.metrics_addr = Some(
                    args[i]
                        .parse()
                        .map_err(|e| format!("invalid metrics address '{}': {e}", args[i]))?,
                );
            }
            "--latency" if i + 1 < args.len() => {
                i += 1;
                config.latency_ms = Some(
                    args[i]
                        .parse()
                        .map_err(|e| format!("invalid latency '{}': {e}", args[i]))?,
                );
            }
            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
        i += 1;
    }

    Ok(config)
}

const HELP: &str = r#"
USAGE: strata-receiver [OPTIONS] --bind <ADDR>[,<ADDR>...]

Standalone Strata bonded transport receiver (no GStreamer required).

OPTIONS:
  --bind <addr>[,<addr>...]  Bind addresses for receiver links (required)
                             e.g. 0.0.0.0:5000 or 0.0.0.0:5000,0.0.0.0:5002
  --output <path>            Write received data to file (default: stdout)
  --metrics-addr <addr>      Prometheus metrics server address (disabled if omitted)
  --latency <ms>             Jitter buffer latency in milliseconds (default: 50)
  --help                     Show this help

EXAMPLES:
  # Single link, output to stdout
  strata-receiver --bind 0.0.0.0:5000

  # Two links, write to file with metrics
  strata-receiver --bind 0.0.0.0:5000,0.0.0.0:5002 \
    --output capture.ts --metrics-addr 0.0.0.0:9090

  # Low-latency mode
  strata-receiver --bind 0.0.0.0:5000 --latency 20
"#;
