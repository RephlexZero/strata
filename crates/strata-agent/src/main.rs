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
mod telemetry;

use std::net::SocketAddr;
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

    // Build shared state
    let state = Arc::new(AgentState {
        simulate: cli.simulate,
        sender_id: tokio::sync::Mutex::new(None),
        session_token: tokio::sync::Mutex::new(None),
        hardware: hardware::HardwareScanner::new(cli.simulate),
        pipeline: tokio::sync::Mutex::new(pipeline::PipelineManager::new(cli.simulate)),
        control_tx: control_tx.clone(),
        shutdown: shutdown_rx.clone(),
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
    let portal_enrollment_token = cli.enrollment_token.clone();
    let portal_control_url = cli.control_url.clone();
    let portal_handle = tokio::spawn(async move {
        run_portal(portal_state, portal_addr, portal_enrollment_token, portal_control_url).await
    });

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

/// Simple onboarding portal — serves status and enrollment API.
async fn run_portal(
    state: Arc<AgentState>,
    addr: SocketAddr,
    _enrollment_token: Option<String>,
    _control_url: String,
) -> anyhow::Result<()> {
    use axum::routing::get;
    use axum::Router;

    let app = Router::new()
        .route("/", get(portal_index))
        .route("/api/status", get(portal_status))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state);

    tracing::info!("onboarding portal on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn portal_index() -> axum::response::Html<&'static str> {
    axum::response::Html(
        r#"<!DOCTYPE html>
<html><head><title>Strata Sender</title></head>
<body>
<h1>Strata Sender Agent</h1>
<p>Onboarding portal. <a href="/api/status">View status JSON</a></p>
</body></html>"#,
    )
}

async fn portal_status(
    axum::extract::State(state): axum::extract::State<Arc<AgentState>>,
) -> axum::Json<serde_json::Value> {
    let status = state.hardware.scan().await;
    let sender_id = state.sender_id.lock().await.clone();
    let pipeline = state.pipeline.lock().await;

    axum::Json(serde_json::json!({
        "sender_id": sender_id,
        "enrolled": sender_id.is_some(),
        "simulate": state.simulate,
        "streaming": pipeline.is_running(),
        "hardware": status,
    }))
}

fn gethostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
}
