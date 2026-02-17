//! Sender onboarding portal — local HTTP server for device setup.
//!
//! Accessed over the sender's Wi-Fi AP (or wired) at `http://10.42.0.1/`
//! (or `http://localhost:3001` in dev). Provides:
//!
//! - Hardware status dashboard (interfaces, inputs, system stats)
//! - Enrollment / unenrollment to the cloud control plane
//! - Receiver address management
//! - Network interface management (enable/disable/discover)
//! - Connectivity testing
//!
//! The UI is a Leptos WASM SPA (`strata-portal` crate), built with trunk
//! and served from a dist directory. Set `PORTAL_DIR` to override the path.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tower_http::services::{ServeDir, ServeFile};

use crate::AgentState;

/// Start the onboarding portal HTTP server.
pub async fn run(state: Arc<AgentState>, addr: SocketAddr) -> anyhow::Result<()> {
    // Serve the trunk-built Leptos WASM SPA.
    // PORTAL_DIR defaults to ../strata-portal/dist (dev) or /app/portal (Docker).
    let portal_dir = std::env::var("PORTAL_DIR").unwrap_or_else(|_| "../strata-portal/dist".into());

    let spa_fallback = ServeFile::new(format!("{portal_dir}/index.html"));
    let portal_service = ServeDir::new(&portal_dir).not_found_service(spa_fallback);

    let app = Router::new()
        // API
        .route("/api/status", get(api_status))
        .route("/api/enroll", post(api_enroll))
        .route("/api/unenroll", post(api_unenroll))
        .route("/api/test", get(api_test))
        .route("/api/config", get(api_get_config).post(api_set_config))
        .route("/api/interfaces/{name}/enable", post(api_interface_enable))
        .route(
            "/api/interfaces/{name}/disable",
            post(api_interface_disable),
        )
        .route("/api/interfaces/scan", post(api_interfaces_scan))
        // Prometheus metrics endpoint
        .route("/metrics", get(api_metrics))
        // Captive portal probes (redirect to /)
        .route("/hotspot-detect.html", get(captive_redirect))
        .route("/generate_204", get(captive_redirect))
        .route("/connecttest.txt", get(captive_redirect))
        // SPA: serve WASM assets, fallback to index.html for client-side routing
        .fallback_service(portal_service)
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state);

    tracing::info!("sender portal on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Captive portal redirect ─────────────────────────────────────────

async fn captive_redirect() -> impl IntoResponse {
    axum::response::Redirect::temporary("/")
}

// ── GET /api/status ─────────────────────────────────────────────────

async fn api_status(State(state): State<Arc<AgentState>>) -> Json<serde_json::Value> {
    let hw = state.hardware.scan().await;
    let sender_id = state.sender_id.lock().await.clone();
    let pipeline = state.pipeline.lock().await;
    let control_connected = state
        .control_connected
        .load(std::sync::atomic::Ordering::Relaxed);
    let receiver_url = state.receiver_url.lock().await.clone();

    Json(serde_json::json!({
        "sender_id": sender_id,
        "enrolled": sender_id.is_some(),
        "cloud_connected": control_connected,
        "simulate": state.simulate,
        "streaming": pipeline.is_running(),
        "stream_id": pipeline.stream_id(),
        "uptime_s": hw.uptime_s,
        "cpu_percent": hw.cpu_percent,
        "mem_used_mb": hw.mem_used_mb,
        "interfaces": hw.interfaces,
        "inputs": hw.inputs,
        "receiver_url": receiver_url,
    }))
}

// ── POST /api/enroll ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EnrollRequest {
    enrollment_token: String,
    control_url: Option<String>,
}

async fn api_enroll(
    State(state): State<Arc<AgentState>>,
    Json(body): Json<EnrollRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Check if already enrolled
    {
        let sid = state.sender_id.lock().await;
        if sid.is_some() {
            return Ok(Json(serde_json::json!({
                "status": "already_enrolled",
                "sender_id": *sid,
            })));
        }
    }

    // Store token for the control loop to pick up
    {
        let mut token = state.pending_enrollment_token.lock().await;
        *token = Some(body.enrollment_token.clone());
    }
    if let Some(url) = &body.control_url {
        let mut u = state.pending_control_url.lock().await;
        *u = Some(url.clone());
    }

    // Signal control loop to reconnect
    let _ = state.reconnect_tx.send(());

    Ok(Json(serde_json::json!({
        "status": "enrolling",
        "message": "Enrollment initiated. The device will connect to the control plane shortly.",
    })))
}

// ── POST /api/unenroll ──────────────────────────────────────────────

async fn api_unenroll(
    State(state): State<Arc<AgentState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let had_id = {
        let mut sid = state.sender_id.lock().await;
        let had = sid.is_some();
        *sid = None;
        had
    };

    if !had_id {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "device is not enrolled"})),
        ));
    }

    // Clear session token
    {
        let mut token = state.session_token.lock().await;
        *token = None;
    }

    // Clear pending enrollment data
    {
        let mut t = state.pending_enrollment_token.lock().await;
        *t = None;
    }
    {
        let mut u = state.pending_control_url.lock().await;
        *u = None;
    }

    // Mark disconnected
    state
        .control_connected
        .store(false, std::sync::atomic::Ordering::Relaxed);

    tracing::info!("device unenrolled via portal");

    Ok(Json(serde_json::json!({
        "status": "unenrolled",
        "message": "Device has been unenrolled. Enter a new enrollment token to re-enroll.",
    })))
}

// ── GET /api/config ─────────────────────────────────────────────────

async fn api_get_config(State(state): State<Arc<AgentState>>) -> Json<serde_json::Value> {
    let control_url = state.control_url.lock().await.clone();
    let receiver_url = state.receiver_url.lock().await.clone();

    Json(serde_json::json!({
        "control_url": control_url,
        "receiver_url": receiver_url,
    }))
}

// ── POST /api/config ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ConfigUpdate {
    receiver_url: Option<String>,
    control_url: Option<String>,
}

async fn api_set_config(
    State(state): State<Arc<AgentState>>,
    Json(body): Json<ConfigUpdate>,
) -> Json<serde_json::Value> {
    if let Some(url) = &body.receiver_url {
        let mut r = state.receiver_url.lock().await;
        *r = if url.is_empty() {
            None
        } else {
            Some(url.clone())
        };
        tracing::info!(receiver_url = %url, "receiver URL updated via portal");
    }

    if let Some(url) = &body.control_url {
        let mut c = state.pending_control_url.lock().await;
        *c = Some(url.clone());
        let _ = state.reconnect_tx.send(());
        tracing::info!(control_url = %url, "control URL updated via portal");
    }

    let receiver_url = state.receiver_url.lock().await.clone();
    let control_url = state.control_url.lock().await.clone();

    Json(serde_json::json!({
        "status": "ok",
        "receiver_url": receiver_url,
        "control_url": control_url,
    }))
}

// ── GET /api/test ───────────────────────────────────────────────────

async fn api_test(State(state): State<Arc<AgentState>>) -> Json<serde_json::Value> {
    let control_connected = state
        .control_connected
        .load(std::sync::atomic::Ordering::Relaxed);
    let sender_id = state.sender_id.lock().await.clone();

    let control_url = state.control_url.lock().await.clone();
    let cloud_reachable = match &control_url {
        Some(url) => crate::util::check_tcp_reachable(url, 5).await,
        None => false,
    };

    let receiver_url = state.receiver_url.lock().await.clone();
    let receiver_reachable = match &receiver_url {
        Some(url) => crate::util::check_tcp_reachable(url, 3).await,
        None => false,
    };

    Json(serde_json::json!({
        "cloud_reachable": cloud_reachable,
        "cloud_connected": control_connected,
        "receiver_reachable": receiver_reachable,
        "receiver_url": receiver_url,
        "enrolled": sender_id.is_some(),
        "sender_id": sender_id,
        "control_url": *state.control_url.lock().await,
    }))
}

// ── POST /api/interfaces/:name/enable ───────────────────────────────

async fn api_interface_enable(
    State(state): State<Arc<AgentState>>,
    Path(name): Path<String>,
) -> Json<serde_json::Value> {
    let ok = state.hardware.set_interface_enabled(&name, true);
    if ok {
        state.pipeline.lock().await.toggle_link(&name, true);
    }
    tracing::info!(interface = %name, "interface enabled via portal");
    Json(serde_json::json!({
        "interface": name,
        "enabled": true,
        "success": ok,
    }))
}

// ── POST /api/interfaces/:name/disable ──────────────────────────────

async fn api_interface_disable(
    State(state): State<Arc<AgentState>>,
    Path(name): Path<String>,
) -> Json<serde_json::Value> {
    let ok = state.hardware.set_interface_enabled(&name, false);
    if ok {
        state.pipeline.lock().await.toggle_link(&name, false);
    }
    tracing::info!(interface = %name, "interface disabled via portal");
    Json(serde_json::json!({
        "interface": name,
        "enabled": false,
        "success": ok,
    }))
}

// ── POST /api/interfaces/scan ───────────────────────────────────────

async fn api_interfaces_scan(State(state): State<Arc<AgentState>>) -> Json<serde_json::Value> {
    let new_ifaces = state.hardware.discover_interfaces().await;
    let hw = state.hardware.scan().await;
    tracing::info!(new_count = new_ifaces.len(), "interface scan via portal");
    Json(serde_json::json!({
        "discovered": new_ifaces,
        "total": hw.interfaces.len(),
        "interfaces": hw.interfaces,
    }))
}

// ── GET /metrics ──────────────────────────────────────────────────

async fn api_metrics(State(state): State<Arc<AgentState>>) -> impl IntoResponse {
    let links = state.latest_link_stats.read().await;
    let body = strata_common::metrics::render_prometheus(&links);
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}
