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
    ConfigSetPayload, ConfigUpdatePayload, Envelope, FilesListPayload, InterfaceCommandPayload,
    InterfacesScanPayload, SourceSwitchPayload, TestRunPayload,
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
            "/{id}/stream/config",
            axum::routing::post(update_stream_config),
        )
        .route("/{id}/source", axum::routing::post(switch_source))
        .route("/{id}/files", get(list_sender_files))
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
    interface_command(&state, &user, &id, &iface_name, "enable").await
}

async fn disable_interface(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, iface_name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    interface_command(&state, &user, &id, &iface_name, "disable").await
}

async fn interface_command(
    state: &AppState,
    user: &AuthUser,
    sender_id: &str,
    iface_name: &str,
    action: &str,
) -> Result<Json<serde_json::Value>, ApiError> {
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

// ── Helpers ─────────────────────────────────────────────────────────

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
