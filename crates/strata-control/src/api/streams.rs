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

use strata_common::ids;
use strata_protocol::api::{StartStreamRequest, StartStreamResponse, StreamDetail, StreamSummary};
use strata_protocol::profiles;
use strata_protocol::{
    ControlMessage, Envelope, ReceiverControlMessage, StreamStartPayload, StreamStopPayload,
};

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
            // For HLS ingest URLs (e.g. YouTube HLS), the URL is used as-is.
            // The CID/key is already embedded in the URL query parameters and
            // segment filenames are appended to the `file=` parameter.
            if dest_url.contains("http_upload_hls") && dest_url.contains("file=") {
                dest_url
            } else if let Some(ref key) = stream_key {
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

    // How many links the stream needs — one per connected sender interface.
    // The heartbeat cache can be briefly empty right after connect; fall
    // back to the same width as the env-var port list.
    let enabled_count = state
        .device_status()
        .get(&sender_id)
        .map(|s| {
            s.network_interfaces
                .iter()
                .filter(|i| {
                    i.enabled && i.state == strata_protocol::models::InterfaceState::Connected
                })
                .count()
        })
        .unwrap_or(FALLBACK_RECEIVER_PORTS.len());
    if enabled_count == 0 {
        return Err(ApiError::bad_request(
            "sender has no connected network interfaces",
        ));
    }

    // Pick a receiver (capacity-aware, DB-derived) or fall back to env
    // config; managed receivers allocate their own ports via request/ack.
    let relay_url_opt = if relay_url.is_empty() {
        None
    } else {
        Some(relay_url.clone())
    };
    let stream_id = ids::stream_id();
    let (receiver_id_opt, strata_dests) = match pick_receiver(&state, &user.user_id).await {
        Some((rcv_id, bind_host)) => {
            let ports = request_receiver_start(
                &state,
                &rcv_id,
                &stream_id,
                enabled_count as u32,
                relay_url_opt.clone(),
            )
            .await?;
            let dests: Vec<String> = ports
                .iter()
                .map(|p| format!("strata://{bind_host}:{p}"))
                .collect();
            (Some(rcv_id), dests)
        }
        None => {
            // Env-var fallback for unmanaged deployments: fixed ports.
            let links = build_receiver_links();
            let count = enabled_count.min(links.len());
            if count == 0 {
                return Err(ApiError::bad_request("no receiver links configured"));
            }
            let dests = links[..count]
                .iter()
                .map(|addr| format!("strata://{addr}"))
                .collect();
            (None, dests)
        }
    };

    tracing::info!(
        links = strata_dests.len(),
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
        Ok("file") | Ok("uri") => strata_protocol::SourceConfig {
            mode: "uri".into(),
            device: None,
            uri: Some("file:///opt/strata/test-media/sample.mp4".into()),
            resolution: None,
            framerate: None,
            passthrough: Some(true),
        },
        _ => strata_protocol::SourceConfig {
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
            let enc = body.encoder.unwrap_or(strata_protocol::EncoderConfig {
                bitrate_kbps: 0, // placeholder — overridden below
                tune: Some("zerolatency".into()),
                keyint_max: Some(60),
                codec: Some("h265".into()),
                min_bitrate_kbps: None,
                max_bitrate_kbps: None,
            });
            // Resolve codec (default h265). YouTube and other modern
            // platforms accept H.265 via Enhanced RTMP / eflvmux.
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
            strata_protocol::EncoderConfig {
                bitrate_kbps: bitrate,
                tune: enc.tune,
                keyint_max: enc.keyint_max,
                codec: Some(codec),
                min_bitrate_kbps: Some(enc.min_bitrate_kbps.unwrap_or(profile.min_kbps)),
                max_bitrate_kbps: Some(enc.max_bitrate_kbps.unwrap_or(profile.max_kbps)),
            }
        },
        destinations: strata_dests,
        // No override — let `SchedulerConfig::default()` (and the agent's own
        // config) govern. The control plane has no explicit-override
        // mechanism from the REST API today; if one is added, plug it in
        // here instead of forcing a profile on every platform stream.
        bonding_config: serde_json::Value::Null,
        psk: None,
        relay_url: relay_url_opt,
    };

    // Store the resolved payload (with defaults applied) so the dashboard
    // can display accurate stream metadata (codec, resolution, framerate, etc).
    let full_config = serde_json::json!({
        "request": {
            "source": serde_json::to_value(&start_payload.source).unwrap_or_default(),
            "encoder": serde_json::to_value(&start_payload.encoder).unwrap_or_default(),
        },
        "relay_url": relay_url,
    });
    let config_json_final = serde_json::to_string(&full_config).ok();

    // Insert stream row into DB (with receiver_id if assigned)
    sqlx::query(
        "INSERT INTO streams (id, sender_id, destination_id, receiver_id, state, started_at, config_json) \
         VALUES ($1, $2, $3, $4, 'starting', $5, $6)",
    )
    .bind(&stream_id)
    .bind(&sender_id)
    .bind(body.destination_id.as_deref().filter(|s| !s.is_empty()))
    .bind(&receiver_id_opt)
    .bind(Utc::now())
    .bind(&config_json_final)
    .execute(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    let envelope =
        Envelope::from_message(&ControlMessage::StreamStart(Box::new(start_payload))).unwrap();
    let json = serde_json::to_string(&envelope).unwrap();

    if agent_tx.send(json).await.is_err() {
        // Agent channel closed — roll back: mark the row failed, tell the
        // receiver to tear down the pipeline it just started, and release
        // its capacity slot (E7: the old rollback leaked both).
        let _ = crate::stream_state::transition(
            state.pool(),
            &stream_id,
            strata_protocol::models::StreamState::Failed,
            Some("failed to send stream.start to agent"),
        )
        .await;
        if let Some(ref rcv_id) = receiver_id_opt
            && let Some(rcv_handle) = state.receivers().get(rcv_id)
        {
            let stop = strata_protocol::ReceiverControlMessage::StreamStop(
                strata_protocol::ReceiverStreamStopPayload {
                    stream_id: stream_id.clone(),
                    reason: "start rollback".into(),
                },
            );
            if let Ok(env) = Envelope::from_message(&stop)
                && let Ok(j) = serde_json::to_string(&env)
            {
                let _ = rcv_handle.tx.send(j).await;
            }
        }
        return Err(ApiError::internal("failed to send to agent"));
    }

    // Notify dashboard
    state.broadcast_dashboard(
        user.user_id.clone(),
        strata_protocol::DashboardEvent::StreamStateChanged {
            stream_id: stream_id.clone(),
            sender_id: sender_id.clone(),
            state: strata_protocol::models::StreamState::Starting,
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
    let (stream_id, receiver_id) = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT s.id, s.receiver_id FROM streams s JOIN senders sn ON s.sender_id = sn.id \
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
    crate::stream_state::transition(
        state.pool(),
        &stream_id,
        strata_protocol::models::StreamState::Stopping,
        None,
    )
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    // Send stop command to agent
    if let Some(agent) = state.agents().get(&sender_id) {
        let stop_payload = StreamStopPayload {
            stream_id: stream_id.clone(),
            reason: "user_request".into(),
        };
        let envelope = Envelope::from_message(&ControlMessage::StreamStop(stop_payload)).unwrap();
        let json = serde_json::to_string(&envelope).unwrap();
        if agent.tx.send(json).await.is_err() {
            tracing::warn!(
                stream_id = %stream_id,
                sender_id = %sender_id,
                "stream.stop command dropped: agent channel closed"
            );
        }
    }

    // Send stop command to the receiver too — without this the receiver's
    // UDP listener never EOS's and its pipeline keeps running after the
    // sender stops. The receiver responds with `receiver.stream.ended`,
    // which decrements `active_streams` on the normal path (see
    // ws_receiver.rs).
    if let Some(ref rcv_id) = receiver_id
        && let Some(rcv_handle) = state.receivers().get(rcv_id)
    {
        let rcv_stop_payload = strata_protocol::ReceiverStreamStopPayload {
            stream_id: stream_id.clone(),
            reason: "user_request".into(),
        };
        let rcv_envelope =
            Envelope::from_message(&ReceiverControlMessage::StreamStop(rcv_stop_payload)).unwrap();
        let rcv_json = serde_json::to_string(&rcv_envelope).unwrap();
        if rcv_handle.tx.send(rcv_json).await.is_err() {
            tracing::warn!(
                stream_id = %stream_id,
                receiver_id = %rcv_id,
                "receiver.stream.stop command dropped: receiver channel closed"
            );
        }
    }

    // Notify dashboard
    state.broadcast_dashboard(
        user.user_id.clone(),
        strata_protocol::DashboardEvent::StreamStateChanged {
            stream_id: stream_id.clone(),
            sender_id: sender_id.clone(),
            state: strata_protocol::models::StreamState::Stopping,
            error: None,
        },
    );

    // Safety timeout: if the agent never sends stream.ended, force the
    // transition so the UI doesn't get stuck in "stopping".
    const STOP_FORCE_END_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    {
        let state = state.clone();
        let stream_id = stream_id.clone();
        let sender_id = sender_id.clone();
        let owner_id = user.user_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(STOP_FORCE_END_TIMEOUT).await;
            let forced = crate::stream_state::force_end_stopping(state.pool(), &stream_id).await;
            if forced.unwrap_or(false) {
                state.live_streams().remove(&stream_id);
                state.broadcast_dashboard(
                    owner_id,
                    strata_protocol::DashboardEvent::StreamStateChanged {
                        stream_id,
                        sender_id,
                        state: strata_protocol::models::StreamState::Ended,
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

async fn get_stream(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<StreamDetail>, ApiError> {
    let row = sqlx::query_as::<_, (String, String, Option<String>, String, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, Option<String>, i64, Option<String>)>(
        "SELECT s.id, s.sender_id, s.destination_id, s.state, s.started_at, s.ended_at, s.config_json, s.total_bytes, s.error_message \
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
        config_json,
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
        config_json,
        total_bytes,
        error_message,
    }))
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Pick the least-loaded online receiver for this owner, or `None` to fall
/// back to env-var configuration. Load is derived from the streams table
/// (COUNT of active assignments), not the hand-maintained `active_streams`
/// counter — counters drift; the streams table is what reconciliation
/// keeps honest (E7).
async fn pick_receiver(state: &AppState, owner_id: &str) -> Option<(String, String)> {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT r.id, r.bind_host FROM receivers r \
         WHERE r.owner_id = $1 AND r.online = TRUE \
           AND (SELECT COUNT(*) FROM streams s \
                WHERE s.receiver_id = r.id AND s.state = ANY($2)) < r.max_streams \
         ORDER BY (SELECT COUNT(*) FROM streams s \
                   WHERE s.receiver_id = r.id AND s.state = ANY($2)) ASC, \
                  r.last_seen_at DESC \
         LIMIT 1",
    )
    .bind(owner_id)
    .bind(&crate::stream_state::ACTIVE_STATES[..])
    .fetch_optional(state.pool())
    .await
    .ok()
    .flatten()?;

    // Verify this receiver is actually connected right now
    if state.receivers().contains_key(&row.0) {
        Some(row)
    } else {
        None
    }
}

/// Ask the receiver to allocate ports and start its pipeline for a stream.
/// Request/ack: the receiver owns its port pool (E6). Returns the bound
/// ports on success.
async fn request_receiver_start(
    state: &AppState,
    receiver_id: &str,
    stream_id: &str,
    link_count: u32,
    relay_url: Option<String>,
) -> Result<Vec<u16>, ApiError> {
    const RECEIVER_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    let rcv_handle = state
        .receivers()
        .get(receiver_id)
        .ok_or_else(|| ApiError::internal("receiver disconnected"))?;

    let request_id = uuid::Uuid::now_v7().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.pending_requests().insert(request_id.clone(), tx);

    let payload = strata_protocol::ReceiverStreamStartPayload {
        request_id: request_id.clone(),
        stream_id: stream_id.to_string(),
        link_count,
        relay_url,
        bonding_config: serde_json::Value::Null,
    };
    let envelope = Envelope::from_message(&ReceiverControlMessage::StreamStart(payload))
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let json = serde_json::to_string(&envelope).map_err(|e| ApiError::internal(e.to_string()))?;
    if rcv_handle.tx.send(json).await.is_err() {
        state.pending_requests().remove(&request_id);
        return Err(ApiError::internal("receiver channel closed"));
    }
    drop(rcv_handle);

    let ack = match tokio::time::timeout(RECEIVER_START_TIMEOUT, rx).await {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => return Err(ApiError::internal("receiver disconnected")),
        Err(_) => {
            state.pending_requests().remove(&request_id);
            return Err(ApiError::internal("receiver did not answer stream start"));
        }
    };

    let ack: strata_protocol::ReceiverStreamStartedPayload =
        serde_json::from_value(ack).map_err(|e| ApiError::internal(e.to_string()))?;
    if !ack.success {
        return Err(ApiError::internal(format!(
            "receiver refused stream: {}",
            ack.error.unwrap_or_else(|| "unknown".into())
        )));
    }
    Ok(ack.bind_ports)
}

/// Fallback link ports assumed for an unmanaged (env-var-configured)
/// receiver — the first 3 of `strata-receiver`'s own CLI default
/// (`--link-ports 5000,5002,5004,5006,5008,5010`). This is an assumption,
/// not a live discovery: if that default ever changes, this must be
/// updated too, or set `RECEIVER_LINKS` explicitly (E9).
const FALLBACK_RECEIVER_PORTS: [u16; 3] = [5000, 5002, 5004];

/// Build the list of receiver link addresses from environment config.
///
/// Reads `RECEIVER_LINKS` (comma-separated `host:port` pairs).
/// Falls back to `RECEIVER_HOST` with `FALLBACK_RECEIVER_PORTS`.
fn build_receiver_links() -> Vec<String> {
    if let Ok(links) = std::env::var("RECEIVER_LINKS") {
        return links
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    let host = std::env::var("RECEIVER_HOST").unwrap_or_else(|_| "strata-receiver".into());
    FALLBACK_RECEIVER_PORTS
        .iter()
        .map(|p| format!("{host}:{p}"))
        .collect()
}
