//! WebSocket handler for dashboard live updates.
//!
//! Endpoint: GET /ws
//!
//! Browser clients connect here to receive real-time events:
//! - Sender status changes (online/offline, hardware state)
//! - Stream stats (per-link bitrate, RTT, loss)
//! - Stream state changes (starting, live, ended, failed)
//!
//! The first message on the socket must be an `auth.login` envelope
//! carrying the user's session JWT (see `strata_common::protocol::
//! DashboardAuthPayload`) — mirroring the agent/receiver WS handshake
//! rather than a `?token=` query param, since tokens in URLs end up in
//! proxy/access logs. Every event delivered afterwards is scoped to the
//! authenticated user's own resources; see `AppState::broadcast_dashboard`.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::SinkExt;
use futures::stream::StreamExt;

use strata_common::protocol::{
    DashboardAuthPayload, DashboardAuthResponsePayload, DashboardEvent, Envelope,
};

use crate::state::AppState;

/// Axum handler — upgrades HTTP to WebSocket.
pub async fn handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

/// Dashboard WebSocket handler.
///
/// Authenticates the connection from its first message, then sends an
/// initial snapshot of the user's own state and subscribes to the
/// broadcast channel for live updates, filtered to that user.
async fn handle_socket(state: AppState, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Wait for the first message — must be auth.login.
    let owner_id = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => match authenticate(&state, &text).await {
            Ok((owner_id, response_json)) => {
                if ws_tx
                    .send(Message::Text(response_json.into()))
                    .await
                    .is_err()
                {
                    return;
                }
                owner_id
            }
            Err(err_json) => {
                let _ = ws_tx.send(Message::Text(err_json.into())).await;
                return;
            }
        },
        _ => return,
    };

    // Subscribe BEFORE building the snapshot so we don't miss events that
    // arrive in between.
    let mut dashboard_rx = state.subscribe_dashboard();

    tracing::debug!(owner_id = %owner_id, "dashboard client connected");

    // ── Initial snapshot ────────────────────────────────────────────
    // Send the current state of the caller's own senders/streams so the UI
    // starts from a correct baseline rather than waiting for the next
    // heartbeat. Scoped to `owner_id` — see the security model in the
    // module doc.
    let mut snapshot: Vec<DashboardEvent> = Vec::new();

    // Online senders + cached device status, scoped to this owner.
    let owned_sender_ids: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM senders WHERE owner_id = $1")
            .bind(&owner_id)
            .fetch_all(state.pool())
            .await
            .unwrap_or_default();
    for (sender_id,) in owned_sender_ids {
        if state.agents().contains_key(&sender_id) {
            let status = state.device_status().get(&sender_id).map(|v| v.clone());
            snapshot.push(DashboardEvent::SenderStatus {
                sender_id,
                online: true,
                status,
            });
        }
    }

    // Active streams for this owner's senders — query the DB for anything
    // in starting/live/stopping.
    if let Ok(rows) = sqlx::query_as::<_, (String, String, String)>(
        "SELECT s.id, s.sender_id, s.state FROM streams s \
         JOIN senders sn ON sn.id = s.sender_id \
         WHERE s.state IN ('starting', 'live', 'stopping') AND sn.owner_id = $1",
    )
    .bind(&owner_id)
    .fetch_all(state.pool())
    .await
    {
        for (stream_id, sender_id, state_str) in rows {
            let stream_state = match state_str.as_str() {
                "starting" => strata_common::models::StreamState::Starting,
                "live" => strata_common::models::StreamState::Live,
                "stopping" => strata_common::models::StreamState::Stopping,
                _ => continue,
            };
            snapshot.push(DashboardEvent::StreamStateChanged {
                stream_id: stream_id.clone(),
                sender_id: sender_id.clone(),
                state: stream_state,
                error: None,
            });

            // Also send the last known stream stats if available
            if let Some(stats) = state.stream_stats().get(&sender_id) {
                snapshot.push(DashboardEvent::StreamStats(stats.clone()));
            }
        }
    }

    for event in snapshot {
        let json = match serde_json::to_string(&event) {
            Ok(j) => j,
            Err(_) => continue,
        };
        if ws_tx.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    // ── Live event loop ─────────────────────────────────────────────

    loop {
        tokio::select! {
            // Forward broadcast events to the browser, scoped to this owner.
            event = dashboard_rx.recv() => {
                match event {
                    Ok((event_owner, event)) => {
                        if event_owner != owner_id {
                            continue;
                        }
                        let json = match serde_json::to_string(&event) {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to serialize dashboard event");
                                continue;
                            }
                        };
                        if ws_tx.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("dashboard client lagged, dropped {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }

            // Handle client messages (subscriptions, pings)
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // Ignore other messages for now
                }
            }
        }
    }

    tracing::debug!(owner_id = %owner_id, "dashboard client disconnected");
}

/// Authenticate the dashboard client from its first message.
/// Returns `Ok((owner_id, response_json))` on success.
async fn authenticate(state: &AppState, raw: &str) -> Result<(String, String), String> {
    let envelope: Envelope =
        serde_json::from_str(raw).map_err(|e| error_response(&format!("invalid message: {e}")))?;

    if envelope.msg_type != "auth.login" {
        return Err(error_response("first message must be auth.login"));
    }

    let payload: DashboardAuthPayload = envelope
        .parse_payload()
        .map_err(|e| error_response(&format!("invalid auth.login payload: {e}")))?;

    let claims = state
        .jwt()
        .verify_token(&payload.token)
        .map_err(|_| error_response("invalid or expired token"))?;

    // A device (sender/receiver) session token carries `owner`; a user
    // session token does not (see api/auth.rs::login). Only user tokens
    // may open the dashboard feed.
    if claims.owner.is_some() {
        return Err(error_response("token is not a user session"));
    }

    let owner_id = claims.sub;
    let response = DashboardAuthResponsePayload {
        success: true,
        error: None,
    };
    let envelope = Envelope::new("auth.login.response", &response);
    let json = serde_json::to_string(&envelope).unwrap();

    Ok((owner_id, json))
}

/// Build a JSON error response string.
fn error_response(msg: &str) -> String {
    let response = DashboardAuthResponsePayload {
        success: false,
        error: Some(msg.to_string()),
    };
    let envelope = Envelope::new("auth.login.response", &response);
    serde_json::to_string(&envelope).unwrap()
}
