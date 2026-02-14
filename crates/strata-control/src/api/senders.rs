//! Sender management endpoints.
//!
//! GET    /api/senders            — list senders for the authenticated user
//! POST   /api/senders            — create a new sender (returns enrollment token)
//! GET    /api/senders/:id        — get sender details
//! DELETE /api/senders/:id        — decommission a sender
//! GET    /api/senders/:id/status — live hardware status (from connected agent)

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use strata_common::ids;

use crate::api::auth::ApiError;
use crate::state::AppState;

use super::auth_extractor::AuthUser;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_senders).post(create_sender))
        .route("/{id}", get(get_sender).delete(delete_sender))
        .route("/{id}/status", get(get_sender_status))
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

    // Store the hashed enrollment token (not the raw token)
    let token_hash = strata_common::auth::hash_password(&enrollment_token)
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

    // Status is in-memory (from the agent's last heartbeat).
    // For now, return online/offline — full DeviceStatusPayload will come
    // when we store the latest heartbeat in AppState.
    Ok(Json(serde_json::json!({
        "sender_id": id,
        "online": online,
    })))
}
