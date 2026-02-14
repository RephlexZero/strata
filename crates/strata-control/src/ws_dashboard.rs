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

use crate::state::AppState;

/// Axum handler — upgrades HTTP to WebSocket.
pub async fn handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

/// Dashboard WebSocket handler.
///
/// Subscribes to the broadcast channel and pushes every event to the client.
/// No authentication required for now (will add JWT auth later).
async fn handle_socket(state: AppState, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    let mut dashboard_rx = state.subscribe_dashboard();

    tracing::debug!("dashboard client connected");

    loop {
        tokio::select! {
            // Forward broadcast events to the browser
            event = dashboard_rx.recv() => {
                match event {
                    Ok(event) => {
                        let json = match serde_json::to_string(&event) {
                            Ok(j) => j,
                            Err(_) => continue,
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
