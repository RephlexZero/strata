//! WebSocket handler for dashboard live updates.
//!
//! Endpoint: GET /ws
//!
//! Browser clients connect here to receive real-time events:
//! - Sender status changes (online/offline, hardware state)
//! - Stream stats (per-link bitrate, RTT, loss)
//! - Stream state changes (starting, live, ended, failed)

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::stream::StreamExt;
use futures::SinkExt;

use strata_common::protocol::DashboardEvent;

use crate::state::AppState;

/// Axum handler — upgrades HTTP to WebSocket.
pub async fn handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

/// Dashboard WebSocket handler.
///
/// Sends an initial snapshot of all known state then subscribes to the
/// broadcast channel for live updates.
async fn handle_socket(state: AppState, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Subscribe BEFORE building the snapshot so we don't miss events that
    // arrive in between.
    let mut dashboard_rx = state.subscribe_dashboard();

    tracing::debug!("dashboard client connected");

    // ── Initial snapshot ────────────────────────────────────────────
    // Send the current state of every connected sender so the UI starts
    // from a correct baseline rather than waiting for the next heartbeat.
    let mut snapshot: Vec<DashboardEvent> = Vec::new();

    // Online senders + cached device status
    for entry in state.agents().iter() {
        let sender_id = entry.key().clone();
        let status = state.device_status().get(&sender_id).map(|v| v.clone());
        snapshot.push(DashboardEvent::SenderStatus {
            sender_id,
            online: true,
            status,
        });
    }

    // Active streams — query the DB for anything in starting/live/stopping
    if let Ok(rows) = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, sender_id, state FROM streams WHERE state IN ('starting', 'live', 'stopping')",
    )
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
            // Forward broadcast events to the browser
            event = dashboard_rx.recv() => {
                match event {
                    Ok(event) => {
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

    tracing::debug!("dashboard client disconnected");
}
