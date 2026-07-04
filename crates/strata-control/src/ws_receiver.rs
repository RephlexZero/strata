//! WebSocket handler for receiver daemon connections.
//!
//! Endpoint: GET /receiver/ws
//!
//! Flow:
//! 1. Receiver connects, sends `auth.login` with capacity info
//! 2. Control plane validates enrollment token, registers receiver
//! 3. Bidirectional message loop: heartbeats, stream commands, stats
//! 4. On disconnect: mark receiver offline, clean up active streams

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use chrono::Utc;
use futures::SinkExt;
use futures::stream::StreamExt;
use tokio::sync::mpsc;

use strata_common::auth;
use strata_protocol::{
    Envelope, PROTOCOL_VERSION, ReceiverAuthLoginPayload, ReceiverAuthLoginResponsePayload,
    ReceiverControlMessage, ReceiverMessage,
};

use crate::state::{AppState, ReceiverHandle};

/// Axum handler — upgrades HTTP to WebSocket.
pub async fn handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

async fn handle_socket(state: AppState, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Wait for the first message — must be auth.login
    let (receiver_id, owner_id, hostname) = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => match authenticate(&state, &text).await {
            Ok((rid, owner_id, hostname, response_json)) => {
                if ws_tx
                    .send(Message::Text(response_json.into()))
                    .await
                    .is_err()
                {
                    return;
                }
                (rid, owner_id, hostname)
            }
            Err(err_json) => {
                let _ = ws_tx.send(Message::Text(err_json.into())).await;
                return;
            }
        },
        _ => return,
    };

    tracing::info!(receiver_id = %receiver_id, "receiver connected");

    // Create channel for sending messages to this receiver. Note: the
    // receiver's own outbound channel to its control-plane WS write task
    // (strata-receiver's `main.rs`) uses 128, not this value — an
    // unexplained mismatch, flagged here rather than silently unified (E9).
    const RECEIVER_COMMAND_CHANNEL_CAPACITY: usize = 64;
    let (tx, mut rx) = mpsc::channel::<String>(RECEIVER_COMMAND_CHANNEL_CAPACITY);

    // Register in shared state
    state
        .receivers()
        .insert(receiver_id.clone(), ReceiverHandle { tx, hostname });

    // Mark online in DB
    let _ = sqlx::query("UPDATE receivers SET online = TRUE, last_seen_at = $1 WHERE id = $2")
        .bind(Utc::now())
        .bind(&receiver_id)
        .execute(state.pool())
        .await;

    // Bidirectional message loop
    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_receiver_message(&state, &receiver_id, &owner_id, &text).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }

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

    // ── Cleanup ─────────────────────────────────────────────────
    state.receivers().remove(&receiver_id);
    state.receiver_status().remove(&receiver_id);

    // Mark offline, reset active_streams
    let _ = sqlx::query(
        "UPDATE receivers SET online = FALSE, active_streams = 0, last_seen_at = $1 WHERE id = $2",
    )
    .bind(Utc::now())
    .bind(&receiver_id)
    .execute(state.pool())
    .await;

    // A WS drop is "unobserved", not "dead" — the receiver's pipelines keep
    // running through a blip. Streams are left for heartbeat reconciliation
    // (or the sweeper via the sender side) instead of being orphan-marked
    // here; see stream_state.rs.
    tracing::info!(receiver_id = %receiver_id, "receiver disconnected");
}

/// Authenticate the receiver from the first message.
async fn authenticate(
    state: &AppState,
    raw: &str,
) -> Result<(String, String, Option<String>, String), String> {
    let envelope: Envelope =
        serde_json::from_str(raw).map_err(|e| error_response(&format!("invalid message: {e}")))?;

    if envelope.proto_version != PROTOCOL_VERSION {
        tracing::warn!(
            receiver_proto = envelope.proto_version,
            ours = PROTOCOL_VERSION,
            "receiver speaks a different protocol version"
        );
    }

    let payload: ReceiverAuthLoginPayload = match envelope.parse_message::<ReceiverMessage>() {
        Ok(ReceiverMessage::AuthLogin(p)) => p,
        Ok(_) => return Err(error_response("first message must be auth.login")),
        Err(e) => return Err(error_response(&format!("invalid auth.login message: {e}"))),
    };

    if let Some(ref token) = payload.enrollment_token {
        return authenticate_enrollment(state, token, &payload).await;
    }

    Err(error_response(
        "no enrollment_token provided for receiver auth",
    ))
}

/// Authenticate via enrollment token.
async fn authenticate_enrollment(
    state: &AppState,
    token: &str,
    payload: &ReceiverAuthLoginPayload,
) -> Result<(String, String, Option<String>, String), String> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, owner_id, enrollment_token FROM receivers WHERE enrollment_token IS NOT NULL",
    )
    .fetch_all(state.pool())
    .await
    .map_err(|e| error_response(&format!("db error: {e}")))?;

    let normalized = strata_common::ids::normalize_enrollment_token(token);
    for (receiver_id, owner_id, token_hash) in &rows {
        if let Ok(true) = auth::verify_password(&normalized, token_hash) {
            // Update receiver record with capacity info
            let _ = sqlx::query(
                "UPDATE receivers SET enrolled = TRUE, hostname = $1, bind_host = $2, \
                 link_ports = $3, max_streams = $4, region = $5, online = TRUE, last_seen_at = $6 \
                 WHERE id = $7",
            )
            .bind(&payload.hostname)
            .bind(&payload.bind_host)
            .bind(
                payload
                    .link_ports
                    .iter()
                    .map(|&p| p as i32)
                    .collect::<Vec<i32>>(),
            )
            .bind(payload.max_streams as i32)
            .bind(&payload.region)
            .bind(Utc::now())
            .bind(receiver_id)
            .execute(state.pool())
            .await;

            // Issue session JWT
            let now = Utc::now().timestamp();
            let claims = auth::Claims {
                sub: receiver_id.clone(),
                iss: "strata-control".into(),
                exp: now + auth::SESSION_TOKEN_TTL_SECS,
                iat: now,
                role: "receiver".into(),
                owner: Some(owner_id.clone()),
            };
            let session_token = state
                .jwt()
                .create_token(&claims)
                .map_err(|e| error_response(&format!("JWT error: {e}")))?;

            let response = ReceiverAuthLoginResponsePayload {
                success: true,
                receiver_id: Some(receiver_id.clone()),
                session_token: Some(session_token),
                error: None,
            };

            let envelope =
                Envelope::from_message(&ReceiverControlMessage::AuthLoginResponse(response))
                    .unwrap();
            let json = serde_json::to_string(&envelope).unwrap();

            tracing::info!(
                receiver_id = %receiver_id,
                hostname = %payload.hostname,
                region = ?payload.region,
                max_streams = payload.max_streams,
                "receiver enrolled"
            );

            return Ok((
                receiver_id.clone(),
                owner_id.clone(),
                Some(payload.hostname.clone()),
                json,
            ));
        }
    }

    Err(error_response("invalid enrollment token"))
}

/// Handle an incoming message from an authenticated receiver.
async fn handle_receiver_message(state: &AppState, receiver_id: &str, owner_id: &str, raw: &str) {
    let envelope: Envelope = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(receiver_id = %receiver_id, "invalid message from receiver: {e}");
            return;
        }
    };

    let msg: ReceiverMessage = match envelope.parse_message() {
        Ok(m) => m,
        Err(_) => {
            tracing::debug!(
                receiver_id = %receiver_id,
                msg_type = %envelope.msg_type,
                "unhandled receiver message type"
            );
            return;
        }
    };

    match msg {
        ReceiverMessage::AuthLogin(_) => {
            tracing::debug!(receiver_id = %receiver_id, "duplicate auth.login ignored");
        }
        ReceiverMessage::Status(payload) => {
            let _ = sqlx::query(
                "UPDATE receivers SET last_seen_at = $1, active_streams = $2 WHERE id = $3",
            )
            .bind(Utc::now())
            .bind(payload.active_streams as i32)
            .bind(receiver_id)
            .execute(state.pool())
            .await;

            crate::stream_state::reconcile_receiver(
                state,
                receiver_id,
                owner_id,
                &payload.running_streams,
            )
            .await;

            state
                .receiver_status()
                .insert(receiver_id.to_string(), payload);
        }
        ReceiverMessage::StreamStats(payload) => {
            // Forward receiver stats to dashboard
            // We could create a dedicated dashboard event for this later
            tracing::trace!(
                receiver_id = %receiver_id,
                stream_id = %payload.stream_id,
                links = payload.links.len(),
                "receiver stream stats"
            );
        }
        ReceiverMessage::StreamEnded(payload) => {
            tracing::info!(
                receiver_id = %receiver_id,
                stream_id = %payload.stream_id,
                reason = ?payload.reason,
                "receiver stream ended"
            );

            // Only act if the stream is still assigned to this receiver.
            let assigned: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM streams WHERE id = $1 AND receiver_id = $2)",
            )
            .bind(&payload.stream_id)
            .bind(receiver_id)
            .fetch_one(state.pool())
            .await
            .unwrap_or(false);
            if assigned {
                let _ = crate::stream_state::transition(
                    state.pool(),
                    &payload.stream_id,
                    strata_protocol::models::StreamState::Ended,
                    None,
                )
                .await;
            }

            state.live_streams().remove(&payload.stream_id);

            // Decrement active_streams
            let _ = sqlx::query(
                "UPDATE receivers SET active_streams = GREATEST(active_streams - 1, 0) WHERE id = $1",
            )
            .bind(receiver_id)
            .execute(state.pool())
            .await;
        }
    }
}

fn error_response(msg: &str) -> String {
    let response = ReceiverAuthLoginResponsePayload {
        success: false,
        receiver_id: None,
        session_token: None,
        error: Some(msg.to_string()),
    };
    let envelope =
        Envelope::from_message(&ReceiverControlMessage::AuthLoginResponse(response)).unwrap();
    serde_json::to_string(&envelope).unwrap()
}
