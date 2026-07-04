//! Strata Sender Daemon
//!
//! Lightweight daemon running on each field sender device.
//!
//! - Connects to the control plane over WebSocket
//! - Reports hardware state (network interfaces, media inputs)
//! - Starts/stops GStreamer sender pipelines on command
//! - Relays real-time bonding telemetry to the control plane

mod control;
mod hardware;
mod metrics;
mod pipeline;
mod pipeline_monitor;
mod portal;
mod telemetry;
pub(crate) mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

/// Strata sender daemon.
#[derive(Parser, Debug)]
#[command(name = "strata-sender", about = "Strata field sender daemon")]
struct Cli {
    /// Control plane WebSocket URL.
    #[arg(long, default_value = "ws://localhost:3000/agent/ws")]
    control_url: String,

    /// Enrollment token (first-time setup).
    #[arg(long)]
    enrollment_token: Option<String>,

    /// Path of the persistent device identity (ed25519 keypair + device id).
    /// Enrollment tokens are single-use — this file is the reconnect
    /// credential after first enrollment.
    #[arg(long, default_value = "/var/lib/strata/sender-identity.json")]
    identity_file: String,

    /// Onboarding portal listen address.
    #[arg(long, default_value = "0.0.0.0:3001")]
    portal_addr: String,

    /// Device hostname override.
    #[arg(long)]
    hostname: Option<String>,

    /// Heartbeat interval in seconds.
    #[arg(long, default_value_t = 10)]
    heartbeat_interval: u64,

    /// Prometheus metrics server address (e.g. 0.0.0.0:9090). Disabled if empty.
    #[arg(long, default_value = "")]
    metrics_addr: String,
}

/// Shared agent state accessible from all tasks.
pub struct AgentState {
    pub sender_id: tokio::sync::Mutex<Option<String>>,
    /// Persistent device identity (keypair + enrolled device id).
    pub identity: tokio::sync::Mutex<strata_common::identity::DeviceIdentity>,
    /// Where `identity` is persisted.
    pub identity_path: std::path::PathBuf,
    pub hardware: hardware::HardwareScanner,
    pub pipeline: tokio::sync::Mutex<pipeline::PipelineManager>,
    pub control_tx: mpsc::Sender<String>,
    pub shutdown: watch::Receiver<bool>,
    /// Whether the control plane WebSocket is currently connected.
    pub control_connected: AtomicBool,
    /// Current control plane URL (may be updated via portal enrollment).
    pub control_url: tokio::sync::Mutex<Option<String>>,
    /// Pending enrollment token (set via portal, consumed by control loop).
    pub pending_enrollment_token: tokio::sync::Mutex<Option<String>>,
    /// Pending control URL override (set via portal).
    pub pending_control_url: tokio::sync::Mutex<Option<String>>,
    /// Notify the control loop to reconnect (e.g. after portal enrollment).
    pub reconnect_tx: tokio::sync::watch::Sender<()>,
    /// Receiver URL — where to send bonded traffic (set via portal or control plane).
    pub receiver_url: tokio::sync::Mutex<Option<String>>,
    /// Sender for triggering graceful shutdown.
    pub shutdown_tx: watch::Sender<bool>,
    /// Latest link stats from the bonding engine (updated by telemetry loop).
    pub latest_link_stats: tokio::sync::RwLock<Vec<strata_protocol::models::LinkStats>>,
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
        .unwrap_or_else(|| gethostname().unwrap_or_else(|| "strata-sender".into()));

    tracing::info!(
        hostname = %hostname,
        control_url = %cli.control_url,
        "strata-sender starting"
    );

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Channel for sending messages to the control plane WebSocket. Note:
    // the control plane's own per-agent command channel (`ws_agent.rs`)
    // uses 64, not this value — an unexplained mismatch, flagged rather
    // than silently unified (E9).
    const CONTROL_OUTGOING_CHANNEL_CAPACITY: usize = 128;
    let (control_tx, control_rx) = mpsc::channel::<String>(CONTROL_OUTGOING_CHANNEL_CAPACITY);

    // Reconnect signal (portal can trigger reconnect after enrollment)
    let (reconnect_tx, _reconnect_rx) = watch::channel(());

    // Device identity: load or generate the keypair before touching the
    // network — enrolling with an unpersistable key would consume the
    // one-time token and strand the device.
    let identity_path = std::path::PathBuf::from(&cli.identity_file);
    let identity = strata_common::identity::DeviceIdentity::load_or_generate(&identity_path)?;

    // Build shared state
    let state = Arc::new(AgentState {
        sender_id: tokio::sync::Mutex::new(None),
        identity: tokio::sync::Mutex::new(identity),
        identity_path,
        hardware: hardware::HardwareScanner::new(),
        pipeline: tokio::sync::Mutex::new(pipeline::PipelineManager::new()),
        control_tx: control_tx.clone(),
        shutdown: shutdown_rx.clone(),
        control_connected: AtomicBool::new(false),
        control_url: tokio::sync::Mutex::new(Some(cli.control_url.clone())),
        pending_enrollment_token: tokio::sync::Mutex::new(cli.enrollment_token.clone()),
        pending_control_url: tokio::sync::Mutex::new(None),
        reconnect_tx,
        receiver_url: tokio::sync::Mutex::new(None),
        shutdown_tx,
        latest_link_stats: tokio::sync::RwLock::new(Vec::new()),
    });

    // ── Task 1: Control plane WebSocket connection ──────────────
    let control_state = state.clone();
    let control_url = cli.control_url.clone();
    let enrollment_token = cli.enrollment_token.clone();
    let hostname_clone = hostname.clone();
    let heartbeat_interval = cli.heartbeat_interval;
    let control_handle = tokio::spawn(async move {
        control::run(
            control_state,
            &control_url,
            enrollment_token.as_deref(),
            &hostname_clone,
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

    // ── Task 2b: Pipeline child process monitor ────────────────
    let monitor_state = state.clone();
    tokio::spawn(async move {
        pipeline_monitor::run(monitor_state).await;
    });

    // ── Task 3: Onboarding portal (HTTP) ────────────────────────
    let portal_state = state.clone();
    let portal_addr: SocketAddr = cli.portal_addr.parse()?;
    let portal_handle = tokio::spawn(async move { portal::run(portal_state, portal_addr).await });

    // ── Task 4: Dedicated metrics server (if --metrics_addr is set) ──
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
        result = portal_handle => {
            if let Err(e) = result {
                tracing::error!("portal task failed: {e}");
            }
        }
    }

    tracing::info!("strata-sender stopped");
    Ok(())
}

fn gethostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
}
