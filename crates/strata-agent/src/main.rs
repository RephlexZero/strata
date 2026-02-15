//! Strata Sender Agent
//!
//! Lightweight daemon running on each field sender device.
//!
//! - Connects to the control plane over WebSocket
//! - Reports hardware state (network interfaces, media inputs)
//! - Starts/stops GStreamer sender pipelines on command
//! - Relays real-time bonding telemetry to the control plane
//! - In `--simulate` mode, generates fake hardware data for local dev

mod control;
mod hardware;
mod pipeline;
mod portal;
mod telemetry;
pub(crate) mod util;

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

/// Strata sender agent daemon.
#[derive(Parser, Debug)]
#[command(name = "strata-agent", about = "Strata field sender agent")]
struct Cli {
    /// Control plane WebSocket URL.
    #[arg(long, default_value = "ws://localhost:3000/agent/ws")]
    control_url: String,

    /// Enrollment token (first-time setup).
    #[arg(long)]
    enrollment_token: Option<String>,

    /// Run in simulation mode (fake hardware, videotestsrc).
    #[arg(long, default_value_t = false)]
    simulate: bool,

    /// Onboarding portal listen address.
    #[arg(long, default_value = "0.0.0.0:3001")]
    portal_addr: String,

    /// Device hostname override.
    #[arg(long)]
    hostname: Option<String>,

    /// Heartbeat interval in seconds.
    #[arg(long, default_value_t = 10)]
    heartbeat_interval: u64,
}

/// Shared agent state accessible from all tasks.
pub struct AgentState {
    pub simulate: bool,
    pub sender_id: tokio::sync::Mutex<Option<String>>,
    pub session_token: tokio::sync::Mutex<Option<String>>,
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
    /// Receiver URL — where to send RIST/SRT traffic (set via portal or control plane).
    pub receiver_url: tokio::sync::Mutex<Option<String>>,
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
        .unwrap_or_else(|| gethostname().unwrap_or_else(|| "strata-agent".into()));

    tracing::info!(
        hostname = %hostname,
        simulate = cli.simulate,
        control_url = %cli.control_url,
        "strata-agent starting"
    );

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Channel for sending messages to the control plane WebSocket
    let (control_tx, control_rx) = mpsc::channel::<String>(128);

    // Reconnect signal (portal can trigger reconnect after enrollment)
    let (reconnect_tx, _reconnect_rx) = watch::channel(());

    // Build shared state
    let state = Arc::new(AgentState {
        simulate: cli.simulate,
        sender_id: tokio::sync::Mutex::new(None),
        session_token: tokio::sync::Mutex::new(None),
        hardware: hardware::HardwareScanner::new(cli.simulate),
        pipeline: tokio::sync::Mutex::new(pipeline::PipelineManager::new(cli.simulate)),
        control_tx: control_tx.clone(),
        shutdown: shutdown_rx.clone(),
        control_connected: AtomicBool::new(false),
        control_url: tokio::sync::Mutex::new(Some(cli.control_url.clone())),
        pending_enrollment_token: tokio::sync::Mutex::new(cli.enrollment_token.clone()),
        pending_control_url: tokio::sync::Mutex::new(None),
        reconnect_tx,
        receiver_url: tokio::sync::Mutex::new(None),
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

    // ── Task 3: Onboarding portal (HTTP) ────────────────────────
    let portal_state = state.clone();
    let portal_addr: SocketAddr = cli.portal_addr.parse()?;
    let portal_handle = tokio::spawn(async move { portal::run(portal_state, portal_addr).await });

    // ── Shutdown handling ───────────────────────────────────────
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received SIGINT, shutting down");
            let _ = shutdown_tx.send(true);
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

    tracing::info!("strata-agent stopped");
    Ok(())
}

fn gethostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
}
