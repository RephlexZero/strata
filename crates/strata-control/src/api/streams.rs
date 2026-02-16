//! Stream management endpoints.
//!
//! POST /api/senders/:id/stream/start — start a broadcast
//! POST /api/senders/:id/stream/stop  — stop a broadcast
//! GET  /api/streams                  — list active streams
//! GET  /api/streams/:id              — get stream details

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use strata_common::ids;
use strata_common::protocol::{Envelope, StreamStartPayload, StreamStopPayload};

use crate::api::auth::ApiError;
use crate::state::AppState;

use super::auth_extractor::AuthUser;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_streams))
        .route("/{id}", get(get_stream))
        // These are nested under senders in the actual mount, but we handle
        // the sender path here for simplicity:
        .route("/start/{sender_id}", post(start_stream))
        .route("/stop/{sender_id}", post(stop_stream))
}

// ── Start Stream ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct StartStreamRequest {
    pub destination_id: Option<String>,
    pub source: Option<strata_common::protocol::SourceConfig>,
    pub encoder: Option<strata_common::protocol::EncoderConfig>,
}

#[derive(Debug, Serialize)]
pub struct StartStreamResponse {
    pub stream_id: String,
    pub state: String,
}

async fn start_stream(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sender_id): Path<String>,
    Json(body): Json<StartStreamRequest>,
) -> Result<(StatusCode, Json<StartStreamResponse>), ApiError> {
    // Verify sender ownership
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM senders WHERE id = $1 AND owner_id = $2)",
    )
    .bind(&sender_id)
    .bind(&user.user_id)
    .fetch_one(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    if !exists {
        return Err(ApiError::not_found("sender not found"));
    }

    // Resolve destination → RTMP relay URL (optional — bonded RIST
    // streams don't require a destination record)
    let relay_url = if let Some(ref dest_id) = body.destination_id {
        if dest_id.is_empty() {
            String::new()
        } else {
            let dest_row = sqlx::query_as::<_, (String, String, Option<String>)>(
                "SELECT platform, url, stream_key FROM destinations WHERE id = $1 AND owner_id = $2",
            )
            .bind(dest_id)
            .bind(&user.user_id)
            .fetch_optional(state.pool())
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .ok_or_else(|| ApiError::not_found("destination not found"))?;

            let (_platform, dest_url, stream_key) = dest_row;
            if let Some(ref key) = stream_key {
                if dest_url.ends_with('/') {
                    format!("{dest_url}{key}")
                } else {
                    format!("{dest_url}/{key}")
                }
            } else {
                dest_url
            }
        }
    } else {
        String::new()
    };

    // Check sender is connected
    let agent = state
        .agents()
        .get(&sender_id)
        .ok_or_else(|| ApiError::bad_request("sender is offline"))?;
    let agent_tx = agent.tx.clone();
    drop(agent);

    // Create stream record
    let stream_id = ids::stream_id();

    // Serialize request before consuming body fields
    let request_json = serde_json::to_value(&body).unwrap_or_default();

    // Build stream.start command and send to agent.
    //
    // RIST destinations are built dynamically based on the sender's
    // currently enabled network interfaces.  Each link targets the
    // receiver's IP on the matching subnet so traffic flows over
    // physically separate paths.
    //
    // RECEIVER_LINKS env var provides the list of receiver endpoints
    // (one per network link), e.g. "172.30.0.20:5000,172.30.1.20:5002,..."
    // Falls back to RECEIVER_HOST with 3 hardcoded ports for single-network setups.
    let receiver_links = build_receiver_links();
    let enabled_count = state
        .device_status()
        .get(&sender_id)
        .map(|s| {
            s.network_interfaces
                .iter()
                .filter(|i| {
                    i.enabled && i.state == strata_common::models::InterfaceState::Connected
                })
                .count()
        })
        .unwrap_or(receiver_links.len());
    let link_count = enabled_count.min(receiver_links.len());
    let rist_dests: Vec<String> = receiver_links[..link_count]
        .iter()
        .map(|addr| format!("rist://{addr}?buffer=2000"))
        .collect();

    tracing::info!(
        links = link_count,
        dests = ?rist_dests,
        "building RIST destinations for sender"
    );

    let start_payload = StreamStartPayload {
        stream_id: stream_id.clone(),
        source: body
            .source
            .unwrap_or(strata_common::protocol::SourceConfig {
                mode: "test".into(),
                device: None,
                uri: None,
                resolution: Some("1280x720".into()),
                framerate: Some(30),
            }),
        encoder: body
            .encoder
            .unwrap_or(strata_common::protocol::EncoderConfig {
                bitrate_kbps: 1000,
                tune: Some("zerolatency".into()),
                keyint_max: Some(60),
            }),
        destinations: rist_dests,
        bonding_config: serde_json::json!({
            "version": 1,
            "scheduler": {
                "critical_broadcast": true,
                "redundancy_enabled": true,
                "capacity_floor_bps": 5_000_000.0,
                "failover_enabled": true,
                "failover_duration_ms": 3000
            }
        }),
        rist_psk: None,
        relay_url: if relay_url.is_empty() {
            None
        } else {
            Some(relay_url.clone())
        },
    };

    // Store the relay URL and request in the stream config for operational visibility.
    let full_config = serde_json::json!({
        "request": request_json,
        "relay_url": relay_url,
    });
    let config_json_final = serde_json::to_string(&full_config).ok();

    // Insert stream row into DB
    sqlx::query(
        "INSERT INTO streams (id, sender_id, destination_id, state, started_at, config_json) \
         VALUES ($1, $2, $3, 'starting', $4, $5)",
    )
    .bind(&stream_id)
    .bind(&sender_id)
    .bind(body.destination_id.as_deref().filter(|s| !s.is_empty()))
    .bind(Utc::now())
    .bind(&config_json_final)
    .execute(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    let envelope = Envelope::new("stream.start", &start_payload);
    let json = serde_json::to_string(&envelope).unwrap();

    agent_tx
        .send(json)
        .await
        .map_err(|_| ApiError::internal("failed to send to agent"))?;

    // Notify dashboard
    state.broadcast_dashboard(
        strata_common::protocol::DashboardEvent::StreamStateChanged {
            stream_id: stream_id.clone(),
            sender_id: sender_id.clone(),
            state: strata_common::models::StreamState::Starting,
            error: None,
        },
    );

    tracing::info!(stream_id = %stream_id, sender_id = %sender_id, "stream starting");

    Ok((
        StatusCode::CREATED,
        Json(StartStreamResponse {
            stream_id,
            state: "starting".into(),
        }),
    ))
}

// ── Stop Stream ─────────────────────────────────────────────────────

async fn stop_stream(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sender_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Find the active stream for this sender
    let stream_id = sqlx::query_scalar::<_, String>(
        "SELECT s.id FROM streams s JOIN senders sn ON s.sender_id = sn.id \
         WHERE s.sender_id = $1 AND sn.owner_id = $2 AND s.state IN ('starting', 'live') \
         ORDER BY s.started_at DESC LIMIT 1",
    )
    .bind(&sender_id)
    .bind(&user.user_id)
    .fetch_optional(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .ok_or_else(|| ApiError::not_found("no active stream for this sender"))?;

    // Update state
    sqlx::query("UPDATE streams SET state = 'stopping' WHERE id = $1")
        .bind(&stream_id)
        .execute(state.pool())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    // Send stop command to agent
    if let Some(agent) = state.agents().get(&sender_id) {
        let stop_payload = StreamStopPayload {
            stream_id: stream_id.clone(),
            reason: "user_request".into(),
        };
        let envelope = Envelope::new("stream.stop", &stop_payload);
        let json = serde_json::to_string(&envelope).unwrap();
        let _ = agent.tx.send(json).await;
    }

    // Notify dashboard
    state.broadcast_dashboard(
        strata_common::protocol::DashboardEvent::StreamStateChanged {
            stream_id,
            sender_id,
            state: strata_common::models::StreamState::Stopping,
            error: None,
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

// ── List Streams ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct StreamSummary {
    pub id: String,
    pub sender_id: String,
    pub state: String,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn list_streams(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<StreamSummary>>, ApiError> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        "SELECT s.id, s.sender_id, s.state, s.started_at \
         FROM streams s JOIN senders sn ON s.sender_id = sn.id \
         WHERE sn.owner_id = $1 \
         ORDER BY s.created_at DESC LIMIT 50",
    )
    .bind(&user.user_id)
    .fetch_all(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    let streams = rows
        .into_iter()
        .map(|(id, sender_id, state_str, started_at)| StreamSummary {
            id,
            sender_id,
            state: state_str,
            started_at,
        })
        .collect();

    Ok(Json(streams))
}

// ── Get Stream ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct StreamDetail {
    pub id: String,
    pub sender_id: String,
    pub destination_id: Option<String>,
    pub state: String,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub total_bytes: i64,
    pub error_message: Option<String>,
}

async fn get_stream(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<StreamDetail>, ApiError> {
    let row = sqlx::query_as::<_, (String, String, Option<String>, String, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, i64, Option<String>)>(
        "SELECT s.id, s.sender_id, s.destination_id, s.state, s.started_at, s.ended_at, s.total_bytes, s.error_message \
         FROM streams s JOIN senders sn ON s.sender_id = sn.id \
         WHERE s.id = $1 AND sn.owner_id = $2",
    )
    .bind(&id)
    .bind(&user.user_id)
    .fetch_optional(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .ok_or_else(|| ApiError::not_found("stream not found"))?;

    let (
        id,
        sender_id,
        destination_id,
        state_str,
        started_at,
        ended_at,
        total_bytes,
        error_message,
    ) = row;

    Ok(Json(StreamDetail {
        id,
        sender_id,
        destination_id,
        state: state_str,
        started_at,
        ended_at,
        total_bytes,
        error_message,
    }))
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Build the list of receiver link addresses from environment config.
///
/// Reads `RECEIVER_LINKS` (comma-separated `host:port` pairs).
/// Falls back to `RECEIVER_HOST` with ports 5000, 5002, 5004.
fn build_receiver_links() -> Vec<String> {
    if let Ok(links) = std::env::var("RECEIVER_LINKS") {
        return links
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    let host = std::env::var("RECEIVER_HOST").unwrap_or_else(|_| "strata-receiver".into());
    vec![
        format!("{host}:5000"),
        format!("{host}:5002"),
        format!("{host}:5004"),
    ]
}
