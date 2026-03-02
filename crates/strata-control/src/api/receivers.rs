//! Receiver management endpoints.
//!
//! GET    /api/receivers           — list receivers
//! POST   /api/receivers           — create receiver (generates enrollment token)
//! GET    /api/receivers/:id       — get receiver details
//! DELETE /api/receivers/:id       — decommission receiver

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
        .route("/", get(list_receivers).post(create_receiver))
        .route("/{id}", get(get_receiver).delete(delete_receiver))
}

// ── List Receivers ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ReceiverSummary {
    pub id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub region: Option<String>,
    pub bind_host: String,
    pub max_streams: i32,
    pub active_streams: i32,
    pub online: bool,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

async fn list_receivers(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<ReceiverSummary>>, ApiError> {
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>, String, i32, i32, bool, Option<chrono::DateTime<chrono::Utc>>, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, name, hostname, region, bind_host, max_streams, active_streams, online, last_seen_at, created_at \
         FROM receivers WHERE owner_id = $1 ORDER BY created_at DESC",
    )
    .bind(&user.user_id)
    .fetch_all(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    let receivers = rows
        .into_iter()
        .map(
            |(
                id,
                name,
                hostname,
                region,
                bind_host,
                max_streams,
                active_streams,
                online,
                last_seen_at,
                created_at,
            )| {
                // Use in-memory state for real-time online status
                let live_online = state.receivers().contains_key(&id);
                ReceiverSummary {
                    id,
                    name,
                    hostname,
                    region,
                    bind_host,
                    max_streams,
                    active_streams,
                    online: live_online || online,
                    last_seen_at,
                    created_at,
                }
            },
        )
        .collect();

    Ok(Json(receivers))
}

// ── Create Receiver ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateReceiverRequest {
    pub name: Option<String>,
    pub bind_host: String,
    pub region: Option<String>,
    #[serde(default = "default_max_streams")]
    pub max_streams: i32,
}

fn default_max_streams() -> i32 {
    6
}

#[derive(Debug, Serialize)]
pub struct CreateReceiverResponse {
    pub receiver_id: String,
    pub enrollment_token: String,
}

async fn create_receiver(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateReceiverRequest>,
) -> Result<(StatusCode, Json<CreateReceiverResponse>), ApiError> {
    user.require_role("operator")?;

    let receiver_id = ids::receiver_id();
    let enrollment_token = ids::enrollment_token();

    let normalized_token = strata_common::ids::normalize_enrollment_token(&enrollment_token);
    let token_hash = strata_common::auth::hash_password(&normalized_token)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    sqlx::query(
        "INSERT INTO receivers (id, owner_id, name, bind_host, link_ports, max_streams, region, enrollment_token) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&receiver_id)
    .bind(&user.user_id)
    .bind(&body.name)
    .bind(&body.bind_host)
    .bind(Vec::<i32>::new()) // link_ports filled on enrollment
    .bind(body.max_streams)
    .bind(&body.region)
    .bind(&token_hash)
    .execute(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    tracing::info!(receiver_id = %receiver_id, owner = %user.user_id, "receiver created");

    Ok((
        StatusCode::CREATED,
        Json(CreateReceiverResponse {
            receiver_id,
            enrollment_token,
        }),
    ))
}

// ── Get Receiver ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ReceiverDetail {
    pub id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub region: Option<String>,
    pub bind_host: String,
    pub link_ports: Vec<i32>,
    pub max_streams: i32,
    pub active_streams: i32,
    pub online: bool,
    pub enrolled: bool,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

async fn get_receiver(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<ReceiverDetail>, ApiError> {
    let row = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>, String, Vec<i32>, i32, i32, bool, bool, Option<chrono::DateTime<chrono::Utc>>, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, name, hostname, region, bind_host, link_ports, max_streams, active_streams, online, enrolled, last_seen_at, created_at \
         FROM receivers WHERE id = $1 AND owner_id = $2",
    )
    .bind(&id)
    .bind(&user.user_id)
    .fetch_optional(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .ok_or_else(|| ApiError::not_found("receiver not found"))?;

    let (
        rid,
        name,
        hostname,
        region,
        bind_host,
        link_ports,
        max_streams,
        active_streams,
        online,
        enrolled,
        last_seen_at,
        created_at,
    ) = row;
    let live_online = state.receivers().contains_key(&rid);

    Ok(Json(ReceiverDetail {
        id: rid,
        name,
        hostname,
        region,
        bind_host,
        link_ports,
        max_streams,
        active_streams,
        online: live_online || online,
        enrolled,
        last_seen_at,
        created_at,
    }))
}

// ── Delete Receiver ─────────────────────────────────────────────────

async fn delete_receiver(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    user.require_role("operator")?;

    let result = sqlx::query("DELETE FROM receivers WHERE id = $1 AND owner_id = $2")
        .bind(&id)
        .bind(&user.user_id)
        .execute(state.pool())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("receiver not found"));
    }

    // Disconnect if currently connected
    state.receivers().remove(&id);

    tracing::info!(receiver_id = %id, "receiver deleted");
    Ok(StatusCode::NO_CONTENT)
}
