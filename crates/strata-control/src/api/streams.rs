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
use strata_common::profiles;
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
    user.require_role("operator")?;

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

    // Guard: no concurrent streams for the same sender
    let already_active = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM streams WHERE sender_id = $1 AND state IN ('starting', 'live'))",
    )
    .bind(&sender_id)
    .bind(&user.user_id)
    .fetch_one(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    if already_active {
        return Err(ApiError::bad_request("sender already has an active stream"));
    }

    // Resolve destination → RTMP relay URL (optional — bonded Strata
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
    // Strata destinations are built dynamically based on the sender's
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
    let strata_dests: Vec<String> = receiver_links[..link_count]
        .iter()
        .map(|addr| format!("strata://{addr}?buffer=2000"))
        .collect();

    tracing::info!(
        links = link_count,
        dests = ?strata_dests,
        "building Strata destinations for sender"
    );

    // Extract source config values before they're consumed into the payload.
    let body_source_resolution = body
        .source
        .as_ref()
        .and_then(|s| s.resolution.clone())
        .or_else(|| Some("1920x1080".into()));
    let body_source_framerate = body.source.as_ref().and_then(|s| s.framerate).or(Some(30));

    let default_source = match std::env::var("STRATA_DEFAULT_SOURCE").as_deref() {
        Ok("file") | Ok("uri") => strata_common::protocol::SourceConfig {
            mode: "uri".into(),
            device: None,
            uri: Some("file:///opt/strata/test-media/sample.mp4".into()),
            resolution: None,
            framerate: None,
            passthrough: Some(true),
        },
        _ => strata_common::protocol::SourceConfig {
            mode: "test".into(),
            device: None,
            uri: None,
            resolution: Some("1920x1080".into()),
            framerate: Some(30),
            passthrough: None,
        },
    };

    let start_payload = StreamStartPayload {
        stream_id: stream_id.clone(),
        source: body.source.unwrap_or(default_source),
        encoder: {
            let enc = body
                .encoder
                .unwrap_or(strata_common::protocol::EncoderConfig {
                    bitrate_kbps: 0, // placeholder — overridden below
                    tune: Some("zerolatency".into()),
                    keyint_max: Some(60),
                    codec: Some("h265".into()),
                    min_bitrate_kbps: None,
                    max_bitrate_kbps: None,
                });
            // Resolve codec (default h265)
            let codec = enc.codec.clone().unwrap_or_else(|| "h265".into());
            // Resolution + framerate come from the source config above
            let source_res = body_source_resolution.as_deref();
            let source_fps = body_source_framerate;
            let profile = profiles::lookup_profile(source_res, source_fps, Some(&codec));
            // Apply smart defaults: if the caller didn't set values, use profile
            let bitrate = if enc.bitrate_kbps == 0 {
                profile.default_kbps
            } else {
                enc.bitrate_kbps
            };
            strata_common::protocol::EncoderConfig {
                bitrate_kbps: bitrate,
                tune: enc.tune,
                keyint_max: enc.keyint_max,
                codec: Some(codec),
                min_bitrate_kbps: Some(enc.min_bitrate_kbps.unwrap_or(profile.min_kbps)),
                max_bitrate_kbps: Some(enc.max_bitrate_kbps.unwrap_or(profile.max_kbps)),
            }
        },
        destinations: strata_dests,
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
        psk: None,
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

    if agent_tx.send(json).await.is_err() {
        // Agent channel closed — roll back the DB row
        let _ = sqlx::query("UPDATE streams SET state = 'ended', ended_at = $1 WHERE id = $2")
            .bind(Utc::now())
            .bind(&stream_id)
            .execute(state.pool())
            .await;
        return Err(ApiError::internal("failed to send to agent"));
    }

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
    user.require_role("operator")?;

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
            stream_id: stream_id.clone(),
            sender_id: sender_id.clone(),
            state: strata_common::models::StreamState::Stopping,
            error: None,
        },
    );

    // Safety timeout: if the agent never sends stream.ended, force the
    // transition after 15 seconds so the UI doesn't get stuck in "stopping".
    {
        let state = state.clone();
        let stream_id = stream_id.clone();
        let sender_id = sender_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            let result = sqlx::query(
                "UPDATE streams SET state = 'ended', ended_at = $1 WHERE id = $2 AND state = 'stopping'",
            )
            .bind(Utc::now())
            .bind(&stream_id)
            .execute(state.pool())
            .await;
            if result.as_ref().map(|r| r.rows_affected()).unwrap_or(0) > 0 {
                state.live_streams().remove(&stream_id);
                state.broadcast_dashboard(
                    strata_common::protocol::DashboardEvent::StreamStateChanged {
                        stream_id,
                        sender_id,
                        state: strata_common::models::StreamState::Ended,
                        error: Some("stop timeout".into()),
                    },
                );
                tracing::warn!("stream stop timed out, forced to ended");
            }
        });
    }

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
