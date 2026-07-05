//! WebSocket handler for sender agent connections.
//!
//! Endpoint: GET /agent/ws
//!
//! Flow:
//! 1. Agent connects, sends `auth.login`
//!    - first enrollment: one-time composite token (`<sender_id>.<SECRET>`,
//!      one argon2 verify) + the device's ed25519 public key; the token is
//!      consumed on success
//!    - reconnect: `device_id` → `auth.challenge` nonce → signature verify
//!      against the enrolled public key
//! 2. On success: registers agent in AppState, starts bidirectional message loop
//! 3. Agent sends heartbeats (`device.status`), stream stats (`stream.stats`)
//! 4. Control plane sends commands (`stream.start`, `stream.stop`, `config.update`)

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use chrono::Utc;
use futures::SinkExt;
use futures::stream::StreamExt;
use tokio::sync::mpsc;

use strata_common::auth;
use strata_protocol::{
    AgentMessage, AuthChallengePayload, AuthLoginPayload, AuthLoginResponsePayload, ControlMessage,
    DashboardEvent, Envelope, PROTOCOL_VERSION,
};

use crate::state::{AgentHandle, AppState};

/// Axum handler — upgrades HTTP to WebSocket.
pub async fn handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

/// Main WebSocket handler for a single agent connection.
async fn handle_socket(state: AppState, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Handshake: enrollment (single message) or challenge/response (two).
    let (sender_id, owner_id, hostname) = match authenticate(&state, &mut ws_tx, &mut ws_rx).await {
        Some(identity) => identity,
        None => return,
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

    // A WS drop is "unobserved", not "dead" — the media pipeline doesn't
    // touch the control plane and keeps running through a blip or a control
    // restart. Active streams are left alone here; the next heartbeat
    // reconciles them, and the sweeper ends them if the agent stays away
    // past UNOBSERVED_GRACE (see stream_state.rs).
    let active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM streams WHERE sender_id = $1 AND state = ANY($2)")
            .bind(&sender_id)
            .bind(&crate::stream_state::ACTIVE_STATES[..])
            .fetch_one(state.pool())
            .await
            .unwrap_or(0);
    if active > 0 {
        tracing::info!(
            sender_id = %sender_id,
            count = active,
            "agent disconnected with active streams — left for reconciliation/sweep"
        );
    }

    // Update last_seen_at
    let _ = sqlx::query("UPDATE senders SET last_seen_at = $1 WHERE id = $2")
        .bind(Utc::now())
        .bind(&sender_id)
        .execute(state.pool())
        .await;

    tracing::info!(sender_id = %sender_id, "agent disconnected");
}

/// How long the agent gets to answer an `auth.challenge` before the
/// connection is dropped.
const CHALLENGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

type WsSink = futures::stream::SplitSink<WebSocket, Message>;
type WsStream = futures::stream::SplitStream<WebSocket>;

/// Run the auth handshake. Sends the success/failure response itself and
/// returns `Some((sender_id, owner_id, hostname))` on success.
async fn authenticate(
    state: &AppState,
    ws_tx: &mut WsSink,
    ws_rx: &mut WsStream,
) -> Option<(String, String, Option<String>)> {
    let text = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => text,
        _ => return None,
    };

    let result = match parse_auth_login(&text) {
        Ok(payload) => {
            if let Some(ref token) = payload.enrollment_token {
                enroll(state, token, &payload).await
            } else if payload.device_id.is_some() {
                challenge_auth(state, ws_tx, ws_rx, &payload).await
            } else {
                Err("no enrollment_token or device_id provided".to_string())
            }
        }
        Err(e) => Err(e),
    };

    match result {
        Ok((sender_id, owner_id, hostname)) => {
            let response = AuthLoginResponsePayload {
                success: true,
                sender_id: Some(sender_id.clone()),
                error: None,
            };
            let envelope =
                Envelope::from_message(&ControlMessage::AuthLoginResponse(response)).unwrap();
            let json = serde_json::to_string(&envelope).unwrap();
            if ws_tx.send(Message::Text(json.into())).await.is_err() {
                return None;
            }
            Some((sender_id, owner_id, hostname))
        }
        Err(msg) => {
            let _ = ws_tx.send(Message::Text(error_response(&msg).into())).await;
            None
        }
    }
}

/// Parse the first message as `auth.login`.
fn parse_auth_login(raw: &str) -> Result<AuthLoginPayload, String> {
    let envelope: Envelope =
        serde_json::from_str(raw).map_err(|e| format!("invalid message: {e}"))?;

    if envelope.proto_version != PROTOCOL_VERSION {
        tracing::warn!(
            agent_proto = envelope.proto_version,
            ours = PROTOCOL_VERSION,
            "agent speaks a different protocol version"
        );
    }

    match envelope.parse_message::<AgentMessage>() {
        Ok(AgentMessage::AuthLogin(p)) => Ok(p),
        Ok(_) => Err("first message must be auth.login".into()),
        Err(e) => Err(format!("invalid auth.login message: {e}")),
    }
}

/// First-time enrollment via one-time composite token
/// (`<sender_id>.<SECRET>`): one row lookup, one argon2 verify. When the
/// agent supplies its ed25519 public key the token is consumed — reconnects
/// then authenticate by challenge, and the token is useless if leaked.
async fn enroll(
    state: &AppState,
    token: &str,
    payload: &AuthLoginPayload,
) -> Result<(String, String, Option<String>), String> {
    let Some((sender_id, secret)) = strata_common::ids::split_enrollment_token(token) else {
        return Err(
            "invalid enrollment token format — expected <device-id>.<token> (re-create the \
             device to get a current-format token)"
                .into(),
        );
    };

    let row: Option<(String, Option<String>, bool)> =
        sqlx::query_as("SELECT owner_id, enrollment_token, enrolled FROM senders WHERE id = $1")
            .bind(&sender_id)
            .fetch_optional(state.pool())
            .await
            .map_err(|e| format!("db error: {e}"))?;

    let Some((owner_id, token_hash, _enrolled)) = row else {
        return Err("invalid enrollment token".into());
    };
    let Some(token_hash) = token_hash else {
        // Token already consumed — the device must use its key.
        return Err("enrollment token already used — reconnect with the device key".into());
    };

    if !auth::verify_password(&secret, &token_hash).unwrap_or(false) {
        return Err("invalid enrollment token".into());
    }

    if let Some(ref pubkey) = payload.device_public_key {
        // Bind the key and consume the token — enrollment is one-time.
        sqlx::query(
            "UPDATE senders SET enrolled = TRUE, hostname = $1, device_public_key = $2, \
             enrollment_token = NULL WHERE id = $3",
        )
        .bind(&payload.hostname)
        .bind(pubkey)
        .bind(&sender_id)
        .execute(state.pool())
        .await
        .map_err(|e| format!("db error: {e}"))?;
        tracing::info!(sender_id = %sender_id, hostname = %payload.hostname, "sender enrolled (device key bound, token consumed)");
    } else {
        // Legacy agent without a keypair: the token has to stay valid as its
        // reconnect credential — i.e. a permanent password. Loud, deliberate.
        sqlx::query("UPDATE senders SET enrolled = TRUE, hostname = $1 WHERE id = $2")
            .bind(&payload.hostname)
            .bind(&sender_id)
            .execute(state.pool())
            .await
            .map_err(|e| format!("db error: {e}"))?;
        tracing::warn!(
            sender_id = %sender_id,
            "sender enrolled WITHOUT a device key — enrollment token remains a reusable credential"
        );
    }

    Ok((sender_id, owner_id, Some(payload.hostname.clone())))
}

/// Reconnect auth: nonce challenge signed with the enrolled device key.
async fn challenge_auth(
    state: &AppState,
    ws_tx: &mut WsSink,
    ws_rx: &mut WsStream,
    payload: &AuthLoginPayload,
) -> Result<(String, String, Option<String>), String> {
    let device_id = payload.device_id.as_deref().unwrap_or_default().to_string();

    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT owner_id, device_public_key FROM senders WHERE id = $1 AND enrolled = TRUE",
    )
    .bind(&device_id)
    .fetch_optional(state.pool())
    .await
    .map_err(|e| format!("db error: {e}"))?;

    let Some((owner_id, pubkey)) = row else {
        return Err("unknown or unenrolled device".into());
    };
    let Some(pubkey) = pubkey else {
        return Err("device has no enrolled key — re-enroll with a current agent".into());
    };

    let challenge = auth::generate_challenge();
    let msg = ControlMessage::AuthChallenge(AuthChallengePayload {
        challenge: challenge.clone(),
    });
    let envelope = Envelope::from_message(&msg).map_err(|e| format!("serialize error: {e}"))?;
    let json = serde_json::to_string(&envelope).map_err(|e| format!("serialize error: {e}"))?;
    ws_tx
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| "connection closed".to_string())?;

    let reply = tokio::time::timeout(CHALLENGE_TIMEOUT, ws_rx.next())
        .await
        .map_err(|_| "challenge response timed out".to_string())?;
    let Some(Ok(Message::Text(text))) = reply else {
        return Err("connection closed during challenge".into());
    };
    let envelope: Envelope =
        serde_json::from_str(&text).map_err(|e| format!("invalid message: {e}"))?;
    let response = match envelope.parse_message::<AgentMessage>() {
        Ok(AgentMessage::AuthChallengeResponse(r)) => r,
        _ => return Err("expected auth.challenge.response".into()),
    };

    if response.device_id != device_id
        || !auth::verify_challenge(&pubkey, &challenge, &response.signature).unwrap_or(false)
    {
        tracing::warn!(sender_id = %device_id, "device key challenge failed");
        return Err("challenge verification failed".into());
    }

    let _ = sqlx::query("UPDATE senders SET hostname = $1, last_seen_at = $2 WHERE id = $3")
        .bind(&payload.hostname)
        .bind(Utc::now())
        .bind(&device_id)
        .execute(state.pool())
        .await;

    tracing::info!(sender_id = %device_id, "sender authenticated via device key");
    Ok((device_id, owner_id, Some(payload.hostname.clone())))
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
        AgentMessage::AuthLogin(_) | AgentMessage::AuthChallengeResponse(_) => {
            tracing::debug!(sender_id = %sender_id, "auth message outside handshake ignored");
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

            // Reconcile the DB against what the device says it's running —
            // this, not WS liveness, is the ground truth for stream state.
            crate::stream_state::reconcile_sender(
                state,
                sender_id,
                owner_id,
                &payload.running_streams,
            )
            .await;

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
                let moved = crate::stream_state::transition(
                    state.pool(),
                    &payload.stream_id,
                    strata_protocol::models::StreamState::Live,
                    None,
                )
                .await;

                // Track the stream as live so we don't re-query every second
                state.live_streams().insert(payload.stream_id.clone());

                // Only broadcast state change on the actual transition
                if moved.unwrap_or(false) {
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

            // Device-confirmed end: no error attribution (that's what makes
            // it ineligible for readoption).
            if let Err(e) = crate::stream_state::transition(
                state.pool(),
                &payload.stream_id,
                strata_protocol::models::StreamState::Ended,
                None,
            )
            .await
            {
                tracing::warn!(stream_id = %payload.stream_id, error = %e, "stream.ended transition failed");
            }
            let _ = sqlx::query("UPDATE streams SET total_bytes = $1 WHERE id = $2")
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
        error: Some(msg.to_string()),
    };
    let envelope = Envelope::from_message(&ControlMessage::AuthLoginResponse(response)).unwrap();
    serde_json::to_string(&envelope).unwrap()
}
