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
use strata_protocol::{
    AgentMessage, AuthLoginPayload, AuthLoginResponsePayload, ControlMessage, DashboardEvent,
    Envelope, PROTOCOL_VERSION,
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
    let (sender_id, owner_id, hostname) = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => match authenticate(&state, &text).await {
            Ok((sid, owner_id, hostname, response_json)) => {
                if ws_tx
                    .send(Message::Text(response_json.into()))
                    .await
                    .is_err()
                {
                    return;
                }
                (sid, owner_id, hostname)
            }
            Err(err_json) => {
                let _ = ws_tx.send(Message::Text(err_json.into())).await;
                return;
            }
        },
        _ => return,
    };

    tracing::info!(sender_id = %sender_id, "agent connected");

    // Create a channel for sending messages to this agent. Note: the
    // agent's own outbound channel to its control-plane WS write task
    // (strata-sender's `main.rs`) uses 128, not this value — an unexplained
    // mismatch, flagged here rather than silently unified (E9).
    const AGENT_COMMAND_CHANNEL_CAPACITY: usize = 64;
    let (tx, mut rx) = mpsc::channel::<String>(AGENT_COMMAND_CHANNEL_CAPACITY);

    // Register agent in shared state
    state
        .agents()
        .insert(sender_id.clone(), AgentHandle { tx, hostname });

    // Notify dashboard
    state.broadcast_dashboard(
        owner_id.clone(),
        DashboardEvent::SenderStatus {
            sender_id: sender_id.clone(),
            online: true,
            status: None,
        },
    );

    // Bidirectional message loop
    loop {
        tokio::select! {
            // Messages FROM agent
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_agent_message(&state, &sender_id, &owner_id, &text).await;
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
    state.broadcast_dashboard(
        owner_id.clone(),
        DashboardEvent::SenderStatus {
            sender_id: sender_id.clone(),
            online: false,
            status: None,
        },
    );

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
        state.broadcast_dashboard(
            owner_id.clone(),
            DashboardEvent::StreamStateChanged {
                stream_id: stream_id.clone(),
                sender_id: sender_id.clone(),
                state: strata_protocol::models::StreamState::Ended,
                error: Some("agent disconnected".into()),
            },
        );
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
/// Returns `Ok((sender_id, owner_id, hostname, response_json))` on success.
async fn authenticate(
    state: &AppState,
    raw: &str,
) -> Result<(String, String, Option<String>, String), String> {
    // Parse envelope
    let envelope: Envelope =
        serde_json::from_str(raw).map_err(|e| error_response(&format!("invalid message: {e}")))?;

    if envelope.proto_version != PROTOCOL_VERSION {
        tracing::warn!(
            agent_proto = envelope.proto_version,
            ours = PROTOCOL_VERSION,
            "agent speaks a different protocol version"
        );
    }

    let payload: AuthLoginPayload = match envelope.parse_message::<AgentMessage>() {
        Ok(AgentMessage::AuthLogin(p)) => p,
        Ok(_) => return Err(error_response("first message must be auth.login")),
        Err(e) => return Err(error_response(&format!("invalid auth.login message: {e}"))),
    };

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
) -> Result<(String, String, Option<String>, String), String> {
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
                exp: now + auth::SESSION_TOKEN_TTL_SECS,
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

            let envelope =
                Envelope::from_message(&ControlMessage::AuthLoginResponse(response)).unwrap();
            let json = serde_json::to_string(&envelope).unwrap();

            tracing::info!(sender_id = %sender_id, hostname = %payload.hostname, "sender enrolled");

            return Ok((
                sender_id.clone(),
                owner_id.clone(),
                Some(payload.hostname.clone()),
                json,
            ));
        }
    }

    Err(error_response("invalid enrollment token"))
}

/// Handle an incoming message from an authenticated agent.
async fn handle_agent_message(state: &AppState, sender_id: &str, owner_id: &str, raw: &str) {
    let envelope: Envelope = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(sender_id = %sender_id, "invalid message from agent: {e}");
            return;
        }
    };

    let msg: AgentMessage = match envelope.parse_message() {
        Ok(m) => m,
        Err(_) => {
            tracing::debug!(
                sender_id = %sender_id,
                msg_type = %envelope.msg_type,
                "unhandled agent message type"
            );
            return;
        }
    };

    match msg {
        AgentMessage::AuthLogin(_) => {
            tracing::debug!(sender_id = %sender_id, "duplicate auth.login ignored");
        }
        AgentMessage::DeviceStatus(payload) => {
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
            state.broadcast_dashboard(
                owner_id,
                DashboardEvent::SenderStatus {
                    sender_id: sender_id.to_string(),
                    online: true,
                    status: Some(payload),
                },
            );
        }
        AgentMessage::StreamStats(mut payload) => {
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
                    state.broadcast_dashboard(
                        owner_id,
                        DashboardEvent::StreamStateChanged {
                            stream_id: payload.stream_id.clone(),
                            sender_id: sender_id.to_string(),
                            state: strata_protocol::models::StreamState::Live,
                            error: None,
                        },
                    );
                }
            }

            state.broadcast_dashboard(owner_id, DashboardEvent::StreamStats(payload.clone()));

            // Cache latest stats for the /metrics endpoint
            state.stream_stats().insert(sender_id.to_string(), payload);
        }
        AgentMessage::StreamEnded(payload) => {
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

            state.broadcast_dashboard(
                owner_id,
                DashboardEvent::StreamStateChanged {
                    stream_id: payload.stream_id,
                    sender_id: sender_id.to_string(),
                    state: strata_protocol::models::StreamState::Ended,
                    error: None,
                },
            );
        }
        // RPC responses — route the raw payload back to the pending REST
        // caller by request_id. Listed explicitly (no catch-all) so a new
        // message type is a compile error until this hub decides what to do
        // with it.
        msg @ (AgentMessage::ConfigSetResponse(_)
        | AgentMessage::ConfigUpdateResponse(_)
        | AgentMessage::TestRunResponse(_)
        | AgentMessage::InterfacesScanResponse(_)
        | AgentMessage::InterfaceCommandResponse(_)
        | AgentMessage::FilesListResponse(_)
        | AgentMessage::NetworkToolResponse(_)
        | AgentMessage::PcapCaptureResponse(_)
        | AgentMessage::LogsResponse(_)
        | AgentMessage::PowerCommandResponse(_)
        | AgentMessage::TlsStatusResponse(_)
        | AgentMessage::TlsRenewResponse(_)
        | AgentMessage::ConfigExportResponse(_)
        | AgentMessage::ConfigImportResponse(_)
        | AgentMessage::UpdatesCheckResponse(_)
        | AgentMessage::UpdatesInstallResponse(_)
        | AgentMessage::StreamDestinationsResponse(_)
        | AgentMessage::JitterBufferResponse(_)) => {
            if let Some(request_id) = msg.request_id()
                && let Some((_, tx)) = state.pending_requests().remove(request_id)
            {
                let _ = tx.send(envelope.payload.clone());
            }
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
    let envelope = Envelope::from_message(&ControlMessage::AuthLoginResponse(response)).unwrap();
    serde_json::to_string(&envelope).unwrap()
}
