//! Sender management endpoints.
//!
//! GET    /api/senders                            — list senders
//! POST   /api/senders                            — create sender
//! GET    /api/senders/:id                         — get sender details
//! DELETE /api/senders/:id                         — decommission sender
//! GET    /api/senders/:id/status                  — live hardware status
//! POST   /api/senders/:id/unenroll                — unenroll sender
//! POST   /api/senders/:id/interfaces/:name/enable — enable interface
//! POST   /api/senders/:id/interfaces/:name/disable — disable interface
//! POST   /api/senders/:id/config                  — set receiver config
//! POST   /api/senders/:id/test                    — run connectivity test
//! POST   /api/senders/:id/interfaces/scan         — scan for new interfaces

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use strata_common::ids;
use strata_common::protocol::{
    ConfigExportPayload, ConfigImportPayload, ConfigSetPayload, ConfigUpdatePayload, Envelope,
    FilesListPayload, InterfaceCommandPayload, InterfacesScanPayload, JitterBufferPayload,
    LogsRequestPayload, NetworkToolPayload, PcapCapturePayload, PowerCommandPayload,
    SourceSwitchPayload, StreamDestinationsPayload, TestRunPayload, TlsRenewPayload,
    TlsStatusPayload, UpdatesCheckPayload, UpdatesInstallPayload,
};

use crate::api::auth::ApiError;
use crate::state::AppState;

use super::auth_extractor::AuthUser;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_senders).post(create_sender))
        .route("/{id}", get(get_sender).delete(delete_sender))
        .route("/{id}/status", get(get_sender_status))
        .route("/{id}/unenroll", axum::routing::post(unenroll_sender))
        .route("/{id}/config", axum::routing::post(set_sender_config))
        .route("/{id}/test", axum::routing::post(run_sender_test))
        .route(
            "/{id}/interfaces/scan",
            axum::routing::post(scan_interfaces),
        )
        .route(
            "/{id}/interfaces/{name}/enable",
            axum::routing::post(enable_interface),
        )
        .route(
            "/{id}/interfaces/{name}/disable",
            axum::routing::post(disable_interface),
        )
        .route(
            "/{id}/interfaces/{name}/lock_band",
            axum::routing::post(lock_band),
        )
        .route(
            "/{id}/interfaces/{name}/priority",
            axum::routing::post(set_priority),
        )
        .route("/{id}/interfaces/{name}/apn", axum::routing::post(set_apn))
        .route(
            "/{id}/stream/config",
            axum::routing::post(update_stream_config),
        )
        .route("/{id}/source", axum::routing::post(switch_source))
        .route("/{id}/files", get(list_sender_files))
        // Diagnostics
        .route(
            "/{id}/diagnostics/network",
            axum::routing::post(run_network_tool),
        )
        .route("/{id}/diagnostics/pcap", axum::routing::post(capture_pcap))
        .route("/{id}/logs", get(get_logs))
        // Power
        .route("/{id}/power", axum::routing::post(power_command))
        // TLS
        .route("/{id}/tls", get(get_tls_status))
        .route("/{id}/tls/renew", axum::routing::post(renew_tls_cert))
        // Config export/import
        .route("/{id}/config/export", get(export_config))
        .route("/{id}/config/import", axum::routing::post(import_config))
        // OTA updates
        .route("/{id}/updates/check", get(check_updates))
        .route("/{id}/updates/install", axum::routing::post(install_update))
        // Stream routing & jitter buffer
        .route(
            "/{id}/stream/destinations",
            axum::routing::post(set_stream_destinations),
        )
        .route(
            "/{id}/stream/jitter_buffer",
            axum::routing::post(set_jitter_buffer),
        )
        // Alerting
        .route("/{id}/alerts", get(get_alert_rules).post(set_alert_rule))
        .route(
            "/{id}/alerts/{rule_id}",
            axum::routing::delete(delete_alert_rule),
        )
}

// ── List Senders ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SenderSummary {
    pub id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub online: bool,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

async fn list_senders(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<SenderSummary>>, ApiError> {
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<chrono::DateTime<chrono::Utc>>, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, name, hostname, last_seen_at, created_at FROM senders WHERE owner_id = $1 ORDER BY created_at DESC",
    )
    .bind(&user.user_id)
    .fetch_all(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    let senders = rows
        .into_iter()
        .map(|(id, name, hostname, last_seen_at, created_at)| {
            let online = state.agents().contains_key(&id);
            SenderSummary {
                id,
                name,
                hostname,
                online,
                last_seen_at,
                created_at,
            }
        })
        .collect();

    Ok(Json(senders))
}

// ── Create Sender ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSenderRequest {
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateSenderResponse {
    pub sender_id: String,
    pub enrollment_token: String,
}

async fn create_sender(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateSenderRequest>,
) -> Result<(StatusCode, Json<CreateSenderResponse>), ApiError> {
    user.require_role("admin")?;

    let sender_id = ids::sender_id();
    let enrollment_token = ids::enrollment_token();

    // Normalize and hash the enrollment token before storage
    let normalized_token = strata_common::ids::normalize_enrollment_token(&enrollment_token);
    let token_hash = strata_common::auth::hash_password(&normalized_token)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    sqlx::query(
        "INSERT INTO senders (id, owner_id, name, enrollment_token) VALUES ($1, $2, $3, $4)",
    )
    .bind(&sender_id)
    .bind(&user.user_id)
    .bind(&body.name)
    .bind(&token_hash)
    .execute(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    tracing::info!(sender_id = %sender_id, owner = %user.user_id, "sender created");

    Ok((
        StatusCode::CREATED,
        Json(CreateSenderResponse {
            sender_id,
            enrollment_token,
        }),
    ))
}

// ── Get Sender ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SenderDetail {
    pub id: String,
    pub owner_id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub enrolled: bool,
    pub online: bool,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

async fn get_sender(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<SenderDetail>, ApiError> {
    let row = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, bool, Option<chrono::DateTime<chrono::Utc>>, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, owner_id, name, hostname, enrolled, last_seen_at, created_at FROM senders WHERE id = $1 AND owner_id = $2",
    )
    .bind(&id)
    .bind(&user.user_id)
    .fetch_optional(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .ok_or_else(|| ApiError::not_found("sender not found"))?;

    let (id, owner_id, name, hostname, enrolled, last_seen_at, created_at) = row;
    let online = state.agents().contains_key(&id);

    Ok(Json(SenderDetail {
        id,
        owner_id,
        name,
        hostname,
        enrolled,
        online,
        last_seen_at,
        created_at,
    }))
}

// ── Delete Sender ───────────────────────────────────────────────────

async fn delete_sender(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    user.require_role("admin")?;

    let result = sqlx::query("DELETE FROM senders WHERE id = $1 AND owner_id = $2")
        .bind(&id)
        .bind(&user.user_id)
        .execute(state.pool())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("sender not found"));
    }

    // Disconnect agent if connected
    state.agents().remove(&id);

    tracing::info!(sender_id = %id, "sender deleted");

    Ok(StatusCode::NO_CONTENT)
}

// ── Get Sender Status ───────────────────────────────────────────────

async fn get_sender_status(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Verify ownership
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM senders WHERE id = $1 AND owner_id = $2)",
    )
    .bind(&id)
    .bind(&user.user_id)
    .fetch_one(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    if !exists {
        return Err(ApiError::not_found("sender not found"));
    }

    let online = state.agents().contains_key(&id);

    // Return cached DeviceStatusPayload if available (from last heartbeat)
    if let Some(status) = state.device_status().get(&id) {
        let mut val =
            serde_json::to_value(status.clone()).map_err(|e| ApiError::internal(e.to_string()))?;
        if let Some(obj) = val.as_object_mut() {
            obj.insert("sender_id".into(), serde_json::json!(id));
            obj.insert("online".into(), serde_json::json!(online));
        }
        return Ok(Json(val));
    }

    Ok(Json(serde_json::json!({
        "sender_id": id,
        "online": online,
    })))
}

// ── Unenroll Sender ─────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct UnenrollResponse {
    pub sender_id: String,
    pub enrollment_token: String,
    pub message: String,
}

async fn unenroll_sender(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<UnenrollResponse>, ApiError> {
    user.require_role("admin")?;

    // Verify ownership
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM senders WHERE id = $1 AND owner_id = $2)",
    )
    .bind(&id)
    .bind(&user.user_id)
    .fetch_one(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    if !exists {
        return Err(ApiError::not_found("sender not found"));
    }

    // Check if already unenrolled
    let enrolled = sqlx::query_scalar::<_, bool>("SELECT enrolled FROM senders WHERE id = $1")
        .bind(&id)
        .fetch_one(state.pool())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if !enrolled {
        return Err(ApiError::bad_request("sender is not currently enrolled"));
    }

    // Generate a new enrollment token
    let new_token = strata_common::ids::enrollment_token();
    let normalized = strata_common::ids::normalize_enrollment_token(&new_token);
    let token_hash = strata_common::auth::hash_password(&normalized)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    // Reset enrollment state
    sqlx::query(
        "UPDATE senders SET enrolled = FALSE, enrollment_token = $1, hostname = NULL, device_public_key = NULL WHERE id = $2",
    )
    .bind(&token_hash)
    .bind(&id)
    .execute(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    // Disconnect the agent if currently connected
    state.agents().remove(&id);

    tracing::info!(sender_id = %id, "sender unenrolled, new token issued");

    Ok(Json(UnenrollResponse {
        sender_id: id,
        enrollment_token: new_token,
        message: "Sender unenrolled. Use the new enrollment token to re-enroll.".into(),
    }))
}

// ── Interface Management (proxied to agent) ─────────────────────────

async fn enable_interface(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, iface_name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    interface_command(
        &state,
        &user,
        &id,
        &iface_name,
        "enable",
        InterfaceCommandOptions::default(),
    )
    .await
}

async fn disable_interface(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, iface_name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    interface_command(
        &state,
        &user,
        &id,
        &iface_name,
        "disable",
        InterfaceCommandOptions::default(),
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct LockBandRequest {
    pub band: Option<String>,
}

async fn lock_band(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, iface_name)): Path<(String, String)>,
    Json(body): Json<LockBandRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    interface_command(
        &state,
        &user,
        &id,
        &iface_name,
        "lock_band",
        InterfaceCommandOptions {
            band: body.band,
            ..Default::default()
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct SetPriorityRequest {
    pub priority: u32,
}

async fn set_priority(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, iface_name)): Path<(String, String)>,
    Json(body): Json<SetPriorityRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    interface_command(
        &state,
        &user,
        &id,
        &iface_name,
        "set_priority",
        InterfaceCommandOptions {
            priority: Some(body.priority),
            ..Default::default()
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct SetApnRequest {
    pub apn: Option<String>,
    pub sim_pin: Option<String>,
    pub roaming: Option<bool>,
}

async fn set_apn(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, iface_name)): Path<(String, String)>,
    Json(body): Json<SetApnRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    interface_command(
        &state,
        &user,
        &id,
        &iface_name,
        "set_apn",
        InterfaceCommandOptions {
            apn: body.apn,
            sim_pin: body.sim_pin,
            roaming: body.roaming,
            ..Default::default()
        },
    )
    .await
}

#[derive(Default)]
struct InterfaceCommandOptions {
    band: Option<String>,
    priority: Option<u32>,
    apn: Option<String>,
    sim_pin: Option<String>,
    roaming: Option<bool>,
}

async fn interface_command(
    state: &AppState,
    user: &AuthUser,
    sender_id: &str,
    iface_name: &str,
    action: &str,
    opts: InterfaceCommandOptions,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;

    // Verify ownership
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM senders WHERE id = $1 AND owner_id = $2)",
    )
    .bind(sender_id)
    .bind(&user.user_id)
    .fetch_one(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    if !exists {
        return Err(ApiError::not_found("sender not found"));
    }

    // Find the connected agent and send the command
    let agent = state
        .agents()
        .get(sender_id)
        .ok_or_else(|| ApiError::bad_request("sender is not connected"))?;

    let payload = InterfaceCommandPayload {
        interface: iface_name.to_string(),
        action: action.to_string(),
        band: opts.band,
        priority: opts.priority,
        apn: opts.apn,
        sim_pin: opts.sim_pin,
        roaming: opts.roaming,
    };
    let envelope = Envelope::new("interface.command", &payload);
    let json = serde_json::to_string(&envelope).map_err(|e| ApiError::internal(e.to_string()))?;

    agent
        .tx
        .send(json)
        .await
        .map_err(|_| ApiError::internal("failed to send command to agent"))?;

    tracing::info!(
        sender_id = %sender_id,
        interface = %iface_name,
        action = %action,
        "interface command sent to agent"
    );

    Ok(Json(serde_json::json!({
        "ok": true,
        "interface": iface_name,
        "action": action,
    })))
}

// ── Config Set (proxied to agent with request-response) ─────────────

#[derive(Debug, Deserialize)]
pub struct SetConfigRequest {
    pub receiver_url: Option<String>,
}

async fn set_sender_config(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<SetConfigRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;

    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.pending_requests().insert(request_id.clone(), tx);

    let agent = state
        .agents()
        .get(&id)
        .ok_or_else(|| ApiError::bad_request("sender is not connected"))?;

    let payload = ConfigSetPayload {
        request_id: request_id.clone(),
        receiver_url: body.receiver_url,
    };
    let envelope = Envelope::new("config.set", &payload);
    let json = serde_json::to_string(&envelope).map_err(|e| ApiError::internal(e.to_string()))?;

    agent
        .tx
        .send(json)
        .await
        .map_err(|_| ApiError::internal("failed to send command to agent"))?;

    drop(agent); // Release DashMap ref before awaiting

    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(value)) => Ok(Json(value)),
        Ok(Err(_)) => Err(ApiError::internal("agent disconnected")),
        Err(_) => {
            state.pending_requests().remove(&request_id);
            Err(ApiError::internal("request timed out"))
        }
    }
}

// ── Connectivity Test (proxied to agent) ────────────────────────────

async fn run_sender_test(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;

    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.pending_requests().insert(request_id.clone(), tx);

    let agent = state
        .agents()
        .get(&id)
        .ok_or_else(|| ApiError::bad_request("sender is not connected"))?;

    let payload = TestRunPayload {
        request_id: request_id.clone(),
    };
    let envelope = Envelope::new("test.run", &payload);
    let json = serde_json::to_string(&envelope).map_err(|e| ApiError::internal(e.to_string()))?;

    agent
        .tx
        .send(json)
        .await
        .map_err(|_| ApiError::internal("failed to send command to agent"))?;

    drop(agent);

    match tokio::time::timeout(Duration::from_secs(15), rx).await {
        Ok(Ok(value)) => Ok(Json(value)),
        Ok(Err(_)) => Err(ApiError::internal("agent disconnected")),
        Err(_) => {
            state.pending_requests().remove(&request_id);
            Err(ApiError::internal("connectivity test timed out"))
        }
    }
}

// ── Interface Scan (proxied to agent) ───────────────────────────────

async fn scan_interfaces(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;

    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.pending_requests().insert(request_id.clone(), tx);

    let agent = state
        .agents()
        .get(&id)
        .ok_or_else(|| ApiError::bad_request("sender is not connected"))?;

    let payload = InterfacesScanPayload {
        request_id: request_id.clone(),
    };
    let envelope = Envelope::new("interfaces.scan", &payload);
    let json = serde_json::to_string(&envelope).map_err(|e| ApiError::internal(e.to_string()))?;

    agent
        .tx
        .send(json)
        .await
        .map_err(|_| ApiError::internal("failed to send command to agent"))?;

    drop(agent);

    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(value)) => Ok(Json(value)),
        Ok(Err(_)) => Err(ApiError::internal("agent disconnected")),
        Err(_) => {
            state.pending_requests().remove(&request_id);
            Err(ApiError::internal("interface scan timed out"))
        }
    }
}

// ── Hot Stream Config Update ────────────────────────────────────────

/// Update the active stream's encoder or scheduler config at runtime.
///
/// POST /api/senders/:id/stream/config
/// Body: { "encoder": { "bitrate_kbps": 2000 }, "scheduler": { ... } }
async fn update_stream_config(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sender_id): Path<String>,
    Json(body): Json<ConfigUpdatePayload>,
) -> Result<StatusCode, ApiError> {
    user.require_role("operator")?;

    verify_ownership(&state, &user, &sender_id).await?;

    let agent = state
        .agents()
        .get(&sender_id)
        .ok_or_else(|| ApiError::bad_request("sender is offline"))?;
    let agent_tx = agent.tx.clone();
    drop(agent);

    // Build a request_id so we can wait for the response
    let request_id = Uuid::now_v7().to_string();
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    state.pending_requests().insert(request_id.clone(), resp_tx);

    // Wrap the payload with the request_id
    let mut payload_val = serde_json::to_value(&body).unwrap_or_default();
    payload_val["request_id"] = serde_json::json!(request_id);
    let envelope = Envelope::new("config.update", &payload_val);
    let json = serde_json::to_string(&envelope).unwrap();

    agent_tx
        .send(json)
        .await
        .map_err(|_| ApiError::internal("failed to send to agent"))?;

    // Wait for response with timeout
    match tokio::time::timeout(Duration::from_secs(10), resp_rx).await {
        Ok(Ok(resp)) => {
            let success = resp
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if success {
                Ok(StatusCode::OK)
            } else {
                let err = resp
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                Err(ApiError::internal(err.to_string()))
            }
        }
        Ok(Err(_)) => {
            state.pending_requests().remove(&request_id);
            Err(ApiError::internal("agent response channel closed"))
        }
        Err(_) => {
            state.pending_requests().remove(&request_id);
            Err(ApiError::internal("config update timed out"))
        }
    }
}

// ── Source Switch ────────────────────────────────────────────────────

async fn switch_source(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sender_id): Path<String>,
    Json(body): Json<SourceSwitchPayload>,
) -> Result<StatusCode, ApiError> {
    user.require_role("operator")?;

    verify_ownership(&state, &user, &sender_id).await?;

    let agent = state
        .agents()
        .get(&sender_id)
        .ok_or_else(|| ApiError::bad_request("sender is offline"))?;
    let agent_tx = agent.tx.clone();
    drop(agent);

    let envelope = Envelope::new("source.switch", &body);
    let json = serde_json::to_string(&envelope).map_err(|e| ApiError::internal(e.to_string()))?;

    agent_tx
        .send(json)
        .await
        .map_err(|_| ApiError::internal("failed to send to agent"))?;

    Ok(StatusCode::OK)
}

// ── File Browser ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FilesQuery {
    path: Option<String>,
}

async fn list_sender_files(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Query(q): Query<FilesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;

    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.pending_requests().insert(request_id.clone(), tx);

    let agent = state
        .agents()
        .get(&id)
        .ok_or_else(|| ApiError::bad_request("sender is offline"))?;

    let payload = FilesListPayload {
        request_id: request_id.clone(),
        path: q.path,
    };
    let envelope = Envelope::new("files.list", &payload);
    let json = serde_json::to_string(&envelope).map_err(|e| ApiError::internal(e.to_string()))?;

    agent
        .tx
        .send(json)
        .await
        .map_err(|_| ApiError::internal("failed to send to agent"))?;
    drop(agent);

    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(value)) => Ok(Json(value)),
        Ok(Err(_)) => Err(ApiError::internal("agent disconnected")),
        Err(_) => {
            state.pending_requests().remove(&request_id);
            Err(ApiError::internal("request timed out"))
        }
    }
}

// ── Diagnostics: Network Tool ───────────────────────────────────────

#[derive(Debug, Deserialize)]
struct NetworkToolRequest {
    tool: String,
    target: Option<String>,
}

async fn run_network_tool(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<NetworkToolRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = NetworkToolPayload {
        request_id,
        tool: body.tool,
        target: body.target,
    };
    proxy_to_agent(&state, &id, "diagnostics.network", &payload, 30).await
}

// ── Diagnostics: PCAP ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PcapRequest {
    duration_secs: u32,
}

async fn capture_pcap(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<PcapRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = PcapCapturePayload {
        request_id,
        duration_secs: body.duration_secs.min(60),
    };
    proxy_to_agent(&state, &id, "diagnostics.pcap", &payload, 70).await
}

// ── Diagnostics: Logs ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LogsQuery {
    service: Option<String>,
    lines: Option<u32>,
}

async fn get_logs(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = LogsRequestPayload {
        request_id,
        service: q.service,
        lines: q.lines,
    };
    proxy_to_agent(&state, &id, "logs.get", &payload, 10).await
}

// ── Power ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PowerRequest {
    action: String,
}

async fn power_command(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<PowerRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = PowerCommandPayload {
        request_id,
        action: body.action,
    };
    proxy_to_agent(&state, &id, "power.command", &payload, 10).await
}

// ── TLS ─────────────────────────────────────────────────────────────

async fn get_tls_status(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = TlsStatusPayload { request_id };
    proxy_to_agent(&state, &id, "tls.status", &payload, 10).await
}

async fn renew_tls_cert(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = TlsRenewPayload { request_id };
    proxy_to_agent(&state, &id, "tls.renew", &payload, 15).await
}

// ── Config Export/Import ────────────────────────────────────────────

async fn export_config(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = ConfigExportPayload { request_id };
    proxy_to_agent(&state, &id, "config.export", &payload, 10).await
}

async fn import_config(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = ConfigImportPayload {
        request_id,
        config: body,
    };
    proxy_to_agent(&state, &id, "config.import", &payload, 10).await
}

// ── OTA Updates ─────────────────────────────────────────────────────

async fn check_updates(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = UpdatesCheckPayload { request_id };
    proxy_to_agent(&state, &id, "updates.check", &payload, 15).await
}

async fn install_update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("admin")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = UpdatesInstallPayload { request_id };
    proxy_to_agent(&state, &id, "updates.install", &payload, 30).await
}

// ── Stream Destinations ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DestinationsRequest {
    destination_ids: Vec<String>,
}

async fn set_stream_destinations(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<DestinationsRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = StreamDestinationsPayload {
        request_id,
        destination_ids: body.destination_ids,
    };
    proxy_to_agent(&state, &id, "stream.destinations", &payload, 10).await
}

// ── Jitter Buffer ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct JitterBufferRequest {
    mode: String,
    static_ms: Option<u32>,
}

async fn set_jitter_buffer(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<JitterBufferRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    let request_id = Uuid::now_v7().to_string();
    let payload = JitterBufferPayload {
        request_id,
        mode: body.mode,
        static_ms: body.static_ms,
    };
    proxy_to_agent(&state, &id, "stream.jitter_buffer", &payload, 10).await
}

// ── Alerting ────────────────────────────────────────────────────────

async fn get_alert_rules(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    let rules = state
        .alert_rules()
        .get(&id)
        .map(|r| r.clone())
        .unwrap_or_default();
    Ok(Json(rules))
}

async fn set_alert_rule(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(mut body): Json<serde_json::Value>,
) -> Result<StatusCode, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    // Ensure the rule has an ID
    if body.get("id").and_then(|v| v.as_str()).is_none() {
        body["id"] = serde_json::json!(Uuid::now_v7().to_string());
    }
    let rule_id = body["id"].as_str().unwrap_or("").to_string();

    let mut rules = state.alert_rules().entry(id).or_default();
    if let Some(pos) = rules
        .iter()
        .position(|r| r.get("id").and_then(|v| v.as_str()) == Some(&rule_id))
    {
        rules[pos] = body;
    } else {
        rules.push(body);
    }
    Ok(StatusCode::OK)
}

async fn delete_alert_rule(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, rule_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    user.require_role("operator")?;
    verify_ownership(&state, &user, &id).await?;

    if let Some(mut rules) = state.alert_rules().get_mut(&id) {
        rules.retain(|r| r.get("id").and_then(|v| v.as_str()) != Some(&rule_id));
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn proxy_to_agent(
    state: &AppState,
    sender_id: &str,
    msg_type: &str,
    payload: &impl serde::Serialize,
    timeout_secs: u64,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = state
        .agents()
        .get(sender_id)
        .ok_or_else(|| ApiError::bad_request("sender is not connected"))?;

    let envelope = Envelope::new(msg_type, payload);
    let json = serde_json::to_string(&envelope).map_err(|e| ApiError::internal(e.to_string()))?;

    // Extract request_id from the payload (convention: all request payloads have one)
    let request_id = serde_json::to_value(payload)
        .ok()
        .and_then(|v| v.get("request_id")?.as_str().map(String::from))
        .unwrap_or_else(|| envelope.id.clone());

    let (tx, rx) = tokio::sync::oneshot::channel();
    state.pending_requests().insert(request_id.clone(), tx);

    agent
        .tx
        .send(json)
        .await
        .map_err(|_| ApiError::internal("failed to send command to agent"))?;

    drop(agent);

    match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
        Ok(Ok(value)) => Ok(Json(value)),
        Ok(Err(_)) => Err(ApiError::internal("agent disconnected")),
        Err(_) => {
            state.pending_requests().remove(&request_id);
            Err(ApiError::internal("request timed out"))
        }
    }
}

/// Verify the authenticated user owns the given sender.
async fn verify_ownership(
    state: &AppState,
    user: &AuthUser,
    sender_id: &str,
) -> Result<(), ApiError> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM senders WHERE id = $1 AND owner_id = $2)",
    )
    .bind(sender_id)
    .bind(&user.user_id)
    .fetch_one(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    if !exists {
        return Err(ApiError::not_found("sender not found"));
    }
    Ok(())
}
