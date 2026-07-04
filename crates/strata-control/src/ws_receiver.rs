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
    AuthChallengePayload, Envelope, PROTOCOL_VERSION, ReceiverAuthLoginPayload,
    ReceiverAuthLoginResponsePayload, ReceiverControlMessage, ReceiverMessage,
};

use crate::state::{AppState, ReceiverHandle};

/// Axum handler — upgrades HTTP to WebSocket.
pub async fn handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

async fn handle_socket(state: AppState, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Handshake: enrollment (single message) or challenge/response (two).
    let (receiver_id, owner_id, hostname) =
        match authenticate(&state, &mut ws_tx, &mut ws_rx).await {
            Some(identity) => identity,
            None => return,
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

/// How long the receiver gets to answer an `auth.challenge`.
const CHALLENGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

type WsSink = futures::stream::SplitSink<WebSocket, Message>;
type WsStream = futures::stream::SplitStream<WebSocket>;

/// Run the auth handshake (see ws_agent.rs for the flow — this mirrors it
/// for receivers). Returns `Some((receiver_id, owner_id, hostname))`.
async fn authenticate(
    state: &AppState,
    ws_tx: &mut WsSink,
    ws_rx: &mut WsStream,
) -> Option<(String, String, Option<String>)> {
    let text = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => text,
        _ => return None,
    };

    let payload = match parse_auth_login(&text) {
        Ok(p) => p,
        Err(e) => {
            let _ = ws_tx.send(Message::Text(error_response(&e).into())).await;
            return None;
        }
    };

    let result = if let Some(ref token) = payload.enrollment_token {
        enroll(state, token, &payload).await
    } else if payload.device_id.is_some() {
        challenge_auth(state, ws_tx, ws_rx, &payload).await
    } else {
        Err("no enrollment_token or device_id provided for receiver auth".to_string())
    };

    match result {
        Ok((receiver_id, owner_id, hostname)) => {
            let response = ReceiverAuthLoginResponsePayload {
                success: true,
                receiver_id: Some(receiver_id.clone()),
                error: None,
            };
            let envelope =
                Envelope::from_message(&ReceiverControlMessage::AuthLoginResponse(response))
                    .unwrap();
            let json = serde_json::to_string(&envelope).unwrap();
            if ws_tx.send(Message::Text(json.into())).await.is_err() {
                return None;
            }
            Some((receiver_id, owner_id, hostname))
        }
        Err(msg) => {
            let _ = ws_tx
                .send(Message::Text(error_response(&msg).into()))
                .await;
            None
        }
    }
}

fn parse_auth_login(raw: &str) -> Result<ReceiverAuthLoginPayload, String> {
    let envelope: Envelope =
        serde_json::from_str(raw).map_err(|e| format!("invalid message: {e}"))?;

    if envelope.proto_version != PROTOCOL_VERSION {
        tracing::warn!(
            receiver_proto = envelope.proto_version,
            ours = PROTOCOL_VERSION,
            "receiver speaks a different protocol version"
        );
    }

    match envelope.parse_message::<ReceiverMessage>() {
        Ok(ReceiverMessage::AuthLogin(p)) => Ok(p),
        Ok(_) => Err("first message must be auth.login".into()),
        Err(e) => Err(format!("invalid auth.login message: {e}")),
    }
}

/// Update the receiver row with the capacity info carried on every auth.
async fn record_capacity(
    state: &AppState,
    receiver_id: &str,
    payload: &ReceiverAuthLoginPayload,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE receivers SET hostname = $1, bind_host = $2, link_ports = $3, \
         max_streams = $4, region = $5, online = TRUE, last_seen_at = $6 WHERE id = $7",
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
    .await
    .map_err(|e| format!("db error: {e}"))?;
    Ok(())
}

/// First-time enrollment via one-time composite token — one row lookup,
/// one argon2 verify; token consumed when a device key is bound.
async fn enroll(
    state: &AppState,
    token: &str,
    payload: &ReceiverAuthLoginPayload,
) -> Result<(String, String, Option<String>), String> {
    let Some((receiver_id, secret)) = strata_common::ids::split_enrollment_token(token) else {
        return Err(
            "invalid enrollment token format — expected <device-id>.<token> (re-create the \
             device to get a current-format token)"
                .into(),
        );
    };

    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT owner_id, enrollment_token FROM receivers WHERE id = $1")
            .bind(&receiver_id)
            .fetch_optional(state.pool())
            .await
            .map_err(|e| format!("db error: {e}"))?;

    let Some((owner_id, token_hash)) = row else {
        return Err("invalid enrollment token".into());
    };
    let Some(token_hash) = token_hash else {
        return Err("enrollment token already used — reconnect with the device key".into());
    };

    if !auth::verify_password(&secret, &token_hash).unwrap_or(false) {
        return Err("invalid enrollment token".into());
    }

    if let Some(ref pubkey) = payload.device_public_key {
        sqlx::query(
            "UPDATE receivers SET enrolled = TRUE, device_public_key = $1, \
             enrollment_token = NULL WHERE id = $2",
        )
        .bind(pubkey)
        .bind(&receiver_id)
        .execute(state.pool())
        .await
        .map_err(|e| format!("db error: {e}"))?;
        tracing::info!(receiver_id = %receiver_id, hostname = %payload.hostname, "receiver enrolled (device key bound, token consumed)");
    } else {
        sqlx::query("UPDATE receivers SET enrolled = TRUE WHERE id = $1")
            .bind(&receiver_id)
            .execute(state.pool())
            .await
            .map_err(|e| format!("db error: {e}"))?;
        tracing::warn!(
            receiver_id = %receiver_id,
            "receiver enrolled WITHOUT a device key — enrollment token remains a reusable credential"
        );
    }

    record_capacity(state, &receiver_id, payload).await?;
    Ok((receiver_id, owner_id, Some(payload.hostname.clone())))
}

/// Reconnect auth: nonce challenge signed with the enrolled device key.
async fn challenge_auth(
    state: &AppState,
    ws_tx: &mut WsSink,
    ws_rx: &mut WsStream,
    payload: &ReceiverAuthLoginPayload,
) -> Result<(String, String, Option<String>), String> {
    let device_id = payload.device_id.as_deref().unwrap_or_default().to_string();

    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT owner_id, device_public_key FROM receivers WHERE id = $1 AND enrolled = TRUE",
    )
    .bind(&device_id)
    .fetch_optional(state.pool())
    .await
    .map_err(|e| format!("db error: {e}"))?;

    let Some((owner_id, pubkey)) = row else {
        return Err("unknown or unenrolled device".into());
    };
    let Some(pubkey) = pubkey else {
        return Err("device has no enrolled key — re-enroll with a current receiver".into());
    };

    let challenge = auth::generate_challenge();
    let msg = ReceiverControlMessage::AuthChallenge(AuthChallengePayload {
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
    let response = match envelope.parse_message::<ReceiverMessage>() {
        Ok(ReceiverMessage::AuthChallengeResponse(r)) => r,
        _ => return Err("expected auth.challenge.response".into()),
    };

    if response.device_id != device_id
        || !auth::verify_challenge(&pubkey, &challenge, &response.signature).unwrap_or(false)
    {
        tracing::warn!(receiver_id = %device_id, "device key challenge failed");
        return Err("challenge verification failed".into());
    }

    record_capacity(state, &device_id, payload).await?;

    tracing::info!(receiver_id = %device_id, "receiver authenticated via device key");
    Ok((device_id, owner_id, Some(payload.hostname.clone())))
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
        ReceiverMessage::AuthLogin(_) | ReceiverMessage::AuthChallengeResponse(_) => {
            tracing::debug!(receiver_id = %receiver_id, "auth message outside handshake ignored");
        }
        ReceiverMessage::Status(payload) => {
            // active_streams is display-only: capacity decisions derive from
            // COUNT(*) over the streams table (see api/streams.rs::
            // pick_receiver); this column just mirrors the device's own
            // report for the admin API.
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
        ReceiverMessage::StreamStarted(payload) => {
            // Ack for a pending stream-start request — route it back to the
            // REST caller waiting in api/streams.rs.
            if let Some((_, tx)) = state.pending_requests().remove(&payload.request_id) {
                let _ = tx.send(envelope.payload.clone());
            } else {
                tracing::warn!(
                    receiver_id = %receiver_id,
                    stream_id = %payload.stream_id,
                    "unmatched receiver.stream.started ack"
                );
            }
        }
        ReceiverMessage::StreamStats(payload) => {
            // Receiver-side measurements are the delivered-goodput ground
            // truth — surface them instead of dropping at trace level (E8).
            // Cache for late-joining dashboards (same contract as the
            // sender-side stream_stats cache).
            state
                .receiver_stream_stats()
                .insert(payload.stream_id.clone(), payload.clone());
            state.broadcast_dashboard(
                owner_id,
                strata_protocol::DashboardEvent::ReceiverStreamStats(payload),
            );
        }
        ReceiverMessage::StreamEnded(payload) => {
            tracing::info!(
                receiver_id = %receiver_id,
                stream_id = %payload.stream_id,
                reason = ?payload.reason,
                "receiver stream ended"
            );
            state.receiver_stream_stats().remove(&payload.stream_id);

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
        }
    }
}

fn error_response(msg: &str) -> String {
    let response = ReceiverAuthLoginResponsePayload {
        success: false,
        receiver_id: None,
        error: Some(msg.to_string()),
    };
    let envelope =
        Envelope::from_message(&ReceiverControlMessage::AuthLoginResponse(response)).unwrap();
    serde_json::to_string(&envelope).unwrap()
}
