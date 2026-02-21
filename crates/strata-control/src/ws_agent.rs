//! WebSocket handler for sender agent connections.
//!
//! Endpoint: GET /agent/ws
//!
//! Flow:
//! 1. Agent connects, sends `auth.login` message
//! 2. Control plane validates enrollment token or device key
//! 3. On success: registers agent in AppState, starts bidirectional message loop
//! 4. Agent sends heartbeats (`device.status`), stream stats (`stream.stats`)
//! 5. Control plane sends commands (`stream.start`, `stream.stop`, `config.update`)

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use chrono::Utc;
use futures::SinkExt;
use futures::stream::StreamExt;
use tokio::sync::mpsc;

use strata_common::auth;
use strata_common::protocol::{
    AuthLoginPayload, AuthLoginResponsePayload, DashboardEvent, DeviceStatusPayload, Envelope,
    StreamEndedPayload, StreamStatsPayload,
};

use crate::state::{AgentHandle, AppState};

/// Axum handler — upgrades HTTP to WebSocket.
pub async fn handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

/// Main WebSocket handler for a single agent connection.
async fn handle_socket(state: AppState, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Wait for the first message — must be auth.login
    let (sender_id, hostname) = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => match authenticate(&state, &text).await {
            Ok((sid, hostname, response_json)) => {
                if ws_tx
                    .send(Message::Text(response_json.into()))
                    .await
                    .is_err()
                {
                    return;
                }
                (sid, hostname)
            }
            Err(err_json) => {
                let _ = ws_tx.send(Message::Text(err_json.into())).await;
                return;
            }
        },
        _ => return,
    };

    tracing::info!(sender_id = %sender_id, "agent connected");

    // Create a channel for sending messages to this agent
    let (tx, mut rx) = mpsc::channel::<String>(64);

    // Register agent in shared state
    state
        .agents()
        .insert(sender_id.clone(), AgentHandle { tx, hostname });

    // Notify dashboard
    state.broadcast_dashboard(DashboardEvent::SenderStatus {
        sender_id: sender_id.clone(),
        online: true,
        status: None,
    });

    // Bidirectional message loop
    loop {
        tokio::select! {
            // Messages FROM agent
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_agent_message(&state, &sender_id, &text).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // Ping/Pong handled by axum
                }
            }

            // Messages TO agent (from REST API or other control logic)
            msg = rx.recv() => {
                match msg {
                    Some(text) => {
                        if ws_tx.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    // Cleanup
    state.agents().remove(&sender_id);
    state.device_status().remove(&sender_id);
    state.stream_stats().remove(&sender_id);
    state.broadcast_dashboard(DashboardEvent::SenderStatus {
        sender_id: sender_id.clone(),
        online: false,
        status: None,
    });

    // Transition any active streams to 'ended' — the agent is gone so they
    // are definitely not running any more.
    let orphaned: Vec<(String,)> = sqlx::query_as(
        "UPDATE streams SET state = 'ended', ended_at = $1 \
         WHERE sender_id = $2 AND state IN ('starting', 'live', 'stopping') \
         RETURNING id",
    )
    .bind(Utc::now())
    .bind(&sender_id)
    .fetch_all(state.pool())
    .await
    .unwrap_or_default();

    for (stream_id,) in &orphaned {
        state.live_streams().remove(stream_id);
        state.broadcast_dashboard(DashboardEvent::StreamStateChanged {
            stream_id: stream_id.clone(),
            sender_id: sender_id.clone(),
            state: strata_common::models::StreamState::Ended,
            error: Some("agent disconnected".into()),
        });
    }
    if !orphaned.is_empty() {
        tracing::warn!(sender_id = %sender_id, count = orphaned.len(), "cleaned up orphaned streams");
    }

    // Update last_seen_at
    let _ = sqlx::query("UPDATE senders SET last_seen_at = $1 WHERE id = $2")
        .bind(Utc::now())
        .bind(&sender_id)
        .execute(state.pool())
        .await;

    tracing::info!(sender_id = %sender_id, "agent disconnected");
}

/// Authenticate the agent from the first message.
/// Returns `Ok((sender_id, response_json))` on success.
async fn authenticate(
    state: &AppState,
    raw: &str,
) -> Result<(String, Option<String>, String), String> {
    // Parse envelope
    let envelope: Envelope =
        serde_json::from_str(raw).map_err(|e| error_response(&format!("invalid message: {e}")))?;

    if envelope.msg_type != "auth.login" {
        return Err(error_response("first message must be auth.login"));
    }

    let payload: AuthLoginPayload = envelope
        .parse_payload()
        .map_err(|e| error_response(&format!("invalid auth.login payload: {e}")))?;

    // Try enrollment token first, then device key
    if let Some(ref token) = payload.enrollment_token {
        return authenticate_enrollment(state, token, &payload).await;
    }

    if let Some(ref _device_key) = payload.device_key {
        // TODO: implement device key auth (post-enrollment reconnect)
        return Err(error_response("device key auth not yet implemented"));
    }

    Err(error_response("no enrollment_token or device_key provided"))
}

/// Authenticate via enrollment token (first-time enrollment).
async fn authenticate_enrollment(
    state: &AppState,
    token: &str,
    payload: &AuthLoginPayload,
) -> Result<(String, Option<String>, String), String> {
    // Find senders with enrollment tokens.
    // Include already-enrolled senders so agents can reconnect after
    // a restart without requiring a separate device_key auth flow.
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, owner_id, enrollment_token FROM senders WHERE enrollment_token IS NOT NULL",
    )
    .fetch_all(state.pool())
    .await
    .map_err(|e| error_response(&format!("db error: {e}")))?;

    // Normalize the token (strip dashes/spaces, uppercase) before verifying
    let normalized = strata_common::ids::normalize_enrollment_token(token);
    // Try each — the tokens are hashed, so we verify against each
    for (sender_id, owner_id, token_hash) in &rows {
        if let Ok(true) = auth::verify_password(&normalized, token_hash) {
            // Mark as enrolled (keep token for reconnection)
            let _ = sqlx::query("UPDATE senders SET enrolled = TRUE, hostname = $1 WHERE id = $2")
                .bind(&payload.hostname)
                .bind(sender_id)
                .execute(state.pool())
                .await;

            // Issue session JWT
            let now = Utc::now().timestamp();
            let claims = auth::Claims {
                sub: sender_id.clone(),
                iss: "strata-control".into(),
                exp: now + 3600,
                iat: now,
                role: "sender".into(),
                owner: Some(owner_id.clone()),
            };
            let session_token = state
                .jwt()
                .create_token(&claims)
                .map_err(|e| error_response(&format!("JWT error: {e}")))?;

            let response = AuthLoginResponsePayload {
                success: true,
                sender_id: Some(sender_id.clone()),
                session_token: Some(session_token),
                error: None,
            };

            let envelope = Envelope::new("auth.login.response", &response);
            let json = serde_json::to_string(&envelope).unwrap();

            tracing::info!(sender_id = %sender_id, hostname = %payload.hostname, "sender enrolled");

            return Ok((sender_id.clone(), Some(payload.hostname.clone()), json));
        }
    }

    Err(error_response("invalid enrollment token"))
}

/// Handle an incoming message from an authenticated agent.
async fn handle_agent_message(state: &AppState, sender_id: &str, raw: &str) {
    let envelope: Envelope = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(sender_id = %sender_id, "invalid message from agent: {e}");
            return;
        }
    };

    match envelope.msg_type.as_str() {
        "device.status" => {
            if let Ok(payload) = envelope.parse_payload::<DeviceStatusPayload>() {
                // Update last_seen_at
                let _ = sqlx::query("UPDATE senders SET last_seen_at = $1 WHERE id = $2")
                    .bind(Utc::now())
                    .bind(sender_id)
                    .execute(state.pool())
                    .await;

                // Cache latest status for REST API consumers
                state
                    .device_status()
                    .insert(sender_id.to_string(), payload.clone());

                // Broadcast to dashboard
                state.broadcast_dashboard(DashboardEvent::SenderStatus {
                    sender_id: sender_id.to_string(),
                    online: true,
                    status: Some(payload),
                });
            }
        }
        "stream.stats" => {
            if let Ok(mut payload) = envelope.parse_payload::<StreamStatsPayload>() {
                // Stamp sender_id and timestamp at the trust boundary
                payload.sender_id = sender_id.to_string();
                payload.timestamp_ms = chrono::Utc::now().timestamp_millis() as u64;

                // Transition stream from 'starting' → 'live' on first stats message.
                // Only run the UPDATE if we haven't already transitioned this stream.
                if !state.live_streams().contains(&payload.stream_id) {
                    let rows = sqlx::query(
                        "UPDATE streams SET state = 'live' WHERE id = $1 AND state = 'starting'",
                    )
                    .bind(&payload.stream_id)
                    .execute(state.pool())
                    .await;

                    // Track the stream as live so we don't re-query every second
                    state.live_streams().insert(payload.stream_id.clone());

                    // Only broadcast state change on the actual transition
                    if rows.as_ref().map(|r| r.rows_affected()).unwrap_or(0) > 0 {
                        state.broadcast_dashboard(DashboardEvent::StreamStateChanged {
                            stream_id: payload.stream_id.clone(),
                            sender_id: sender_id.to_string(),
                            state: strata_common::models::StreamState::Live,
                            error: None,
                        });
                    }
                }

                state.broadcast_dashboard(DashboardEvent::StreamStats(payload.clone()));

                // Cache latest stats for the /metrics endpoint
                state.stream_stats().insert(sender_id.to_string(), payload);
            }
        }
        "stream.ended" => {
            if let Ok(payload) = envelope.parse_payload::<StreamEndedPayload>() {
                // Remove from live_streams tracking
                state.live_streams().remove(&payload.stream_id);

                // Update stream record
                let _ = sqlx::query(
                    "UPDATE streams SET state = 'ended', ended_at = $1, total_bytes = $2 WHERE id = $3",
                )
                .bind(Utc::now())
                .bind(payload.total_bytes as i64)
                .bind(&payload.stream_id)
                .execute(state.pool())
                .await;

                state.broadcast_dashboard(DashboardEvent::StreamStateChanged {
                    stream_id: payload.stream_id,
                    sender_id: sender_id.to_string(),
                    state: strata_common::models::StreamState::Ended,
                    error: None,
                });
            }
        }
        // Route request-response messages back to pending callers
        "config.set.response"
        | "config.update.response"
        | "test.run.response"
        | "interfaces.scan.response"
        | "interface.command.response"
        | "files.list.response" => {
            if let Some(request_id) = envelope.payload.get("request_id").and_then(|v| v.as_str())
                && let Some((_, tx)) = state.pending_requests().remove(request_id)
            {
                let _ = tx.send(envelope.payload.clone());
            }
        }
        other => {
            tracing::debug!(sender_id = %sender_id, msg_type = %other, "unhandled agent message type");
        }
    }
}

/// Build a JSON error response string.
fn error_response(msg: &str) -> String {
    let response = AuthLoginResponsePayload {
        success: false,
        sender_id: None,
        session_token: None,
        error: Some(msg.to_string()),
    };
    let envelope = Envelope::new("auth.login.response", &response);
    serde_json::to_string(&envelope).unwrap()
}
