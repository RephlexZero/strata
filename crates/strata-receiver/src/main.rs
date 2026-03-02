//! Strata Receiver Daemon
//!
//! Cloud-side daemon that:
//! - Connects to the control plane over WebSocket
//! - Registers capacity (max streams, bind ports, region)
//! - Starts/stops GStreamer receiver pipelines on command
//! - Relays real-time receiver stats to the control plane

mod control;
mod metrics;
mod pipeline;
mod pipeline_monitor;
mod telemetry;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

/// Strata receiver daemon.
#[derive(Parser, Debug)]
#[command(name = "strata-receiver", about = "Strata cloud receiver daemon")]
struct Cli {
    /// Control plane WebSocket URL.
    #[arg(long, default_value = "ws://localhost:3000/receiver/ws")]
    control_url: String,

    /// Enrollment token for registering with the control plane.
    #[arg(long)]
    enrollment_token: Option<String>,

    /// Public IP or hostname that senders can reach this receiver at.
    #[arg(long, env = "BIND_HOST")]
    bind_host: String,

    /// Comma-separated UDP ports available for bonded stream links.
    /// Each active stream consumes N ports (one per sender link).
    #[arg(long, default_value = "5000,5002,5004,5006,5008,5010")]
    link_ports: String,

    /// Maximum concurrent streams this receiver can handle.
    #[arg(long, default_value_t = 6)]
    max_streams: u32,

    /// Region tag for capacity-aware scheduling (e.g. "eu-central", "us-east").
    #[arg(long)]
    region: Option<String>,

    /// Hostname override.
    #[arg(long)]
    hostname: Option<String>,

    /// Heartbeat interval in seconds.
    #[arg(long, default_value_t = 10)]
    heartbeat_interval: u64,

    /// Prometheus metrics server address (e.g. 0.0.0.0:9090). Disabled if empty.
    #[arg(long, default_value = "")]
    metrics_addr: String,
}

/// Shared receiver daemon state accessible from all tasks.
pub struct ReceiverState {
    pub receiver_id: tokio::sync::Mutex<Option<String>>,
    pub session_token: tokio::sync::Mutex<Option<String>>,
    pub pipelines: tokio::sync::Mutex<pipeline::PipelineRegistry>,
    pub control_tx: mpsc::Sender<String>,
    pub shutdown: watch::Receiver<bool>,
    pub shutdown_tx: watch::Sender<bool>,
    pub control_connected: AtomicBool,
    /// Port pool for assigning to streams.
    pub port_pool: tokio::sync::Mutex<PortPool>,
    /// Capacity config.
    pub max_streams: u32,
    pub region: Option<String>,
    pub bind_host: String,
    /// Latest link stats per stream (stream_id → stats).
    pub latest_stats: tokio::sync::RwLock<
        std::collections::HashMap<String, Vec<strata_common::models::LinkStats>>,
    >,
}

/// Pool of available UDP ports for receiver links.
pub struct PortPool {
    all_ports: Vec<u16>,
    in_use: std::collections::HashSet<u16>,
}

impl PortPool {
    pub fn new(ports: Vec<u16>) -> Self {
        Self {
            all_ports: ports,
            in_use: std::collections::HashSet::new(),
        }
    }

    /// Allocate `count` ports from the pool. Returns None if not enough available.
    pub fn allocate(&mut self, count: usize) -> Option<Vec<u16>> {
        let available: Vec<u16> = self
            .all_ports
            .iter()
            .filter(|p| !self.in_use.contains(p))
            .copied()
            .take(count)
            .collect();
        if available.len() < count {
            return None;
        }
        for &p in &available {
            self.in_use.insert(p);
        }
        Some(available)
    }

    /// Return ports to the pool.
    pub fn release(&mut self, ports: &[u16]) {
        for p in ports {
            self.in_use.remove(p);
        }
    }

    /// All ports (for registration).
    pub fn all(&self) -> &[u16] {
        &self.all_ports
    }

    /// Number of active streams (approximation based on ports in use).
    pub fn active_count(&self) -> usize {
        self.in_use.len()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let hostname = cli
        .hostname
        .unwrap_or_else(|| gethostname().unwrap_or_else(|| "strata-receiver".into()));

    let link_ports: Vec<u16> = cli
        .link_ports
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    if link_ports.is_empty() {
        anyhow::bail!("--link-ports must contain at least one valid port");
    }

    tracing::info!(
        hostname = %hostname,
        control_url = %cli.control_url,
        bind_host = %cli.bind_host,
        link_ports = ?link_ports,
        max_streams = cli.max_streams,
        region = ?cli.region,
        "strata-receiver starting"
    );

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Channel for sending messages to the control plane WebSocket
    let (control_tx, control_rx) = mpsc::channel::<String>(128);

    // Build shared state
    let state = Arc::new(ReceiverState {
        receiver_id: tokio::sync::Mutex::new(None),
        session_token: tokio::sync::Mutex::new(None),
        pipelines: tokio::sync::Mutex::new(pipeline::PipelineRegistry::new()),
        control_tx: control_tx.clone(),
        shutdown: shutdown_rx.clone(),
        shutdown_tx,
        control_connected: AtomicBool::new(false),
        port_pool: tokio::sync::Mutex::new(PortPool::new(link_ports.clone())),
        max_streams: cli.max_streams,
        region: cli.region.clone(),
        bind_host: cli.bind_host.clone(),
        latest_stats: tokio::sync::RwLock::new(std::collections::HashMap::new()),
    });

    // ── Task 1: Control plane WebSocket connection ──────────────
    let control_state = state.clone();
    let control_url = cli.control_url.clone();
    let enrollment_token = cli.enrollment_token.clone();
    let hostname_clone = hostname.clone();
    let heartbeat_interval = cli.heartbeat_interval;
    let bind_host = cli.bind_host.clone();
    let region = cli.region.clone();
    let max_streams = cli.max_streams;
    let control_handle = tokio::spawn(async move {
        control::run(
            control_state,
            &control_url,
            enrollment_token.as_deref(),
            &hostname_clone,
            &bind_host,
            link_ports,
            max_streams,
            region.as_deref(),
            heartbeat_interval,
            control_rx,
        )
        .await
    });

    // ── Task 2: Telemetry loop ────────────────────────────────
    let telemetry_state = state.clone();
    let _telemetry_handle = tokio::spawn(async move {
        telemetry::run(telemetry_state).await;
    });

    // ── Task 3: Pipeline child process monitor ────────────────
    let monitor_state = state.clone();
    tokio::spawn(async move {
        pipeline_monitor::run(monitor_state).await;
    });

    // ── Task 4: Dedicated metrics server (if --metrics-addr is set) ──
    if !cli.metrics_addr.is_empty() {
        let metrics_state = state.clone();
        let metrics_addr: SocketAddr = cli.metrics_addr.parse()?;
        tokio::spawn(async move {
            if let Err(e) = metrics::run(metrics_state, metrics_addr).await {
                tracing::error!(error = %e, "metrics server failed");
            }
        });
    }

    // ── Shutdown handling ───────────────────────────────────────
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received SIGINT, shutting down");
            let _ = state.shutdown_tx.send(true);
        }
        result = control_handle => {
            if let Err(e) = result {
                tracing::error!("control task failed: {e}");
            }
        }
    }

    // Stop all running pipelines
    {
        let mut pipelines = state.pipelines.lock().await;
        pipelines.stop_all();
    }

    tracing::info!("strata-receiver stopped");
    Ok(())
}

fn gethostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
}
