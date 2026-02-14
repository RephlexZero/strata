//! WebSocket control channel to the cloud control plane.
//!
//! Handles:
//! - Connection with exponential backoff reconnect
//! - Authentication (enrollment token or device key)
//! - Heartbeat (device.status every N seconds)
//! - Incoming commands (stream.start, stream.stop, config.update)
//! - Outgoing messages (stream.stats, stream.ended)

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use strata_common::models::StreamState;
use strata_common::protocol::{
    AgentMessage, AuthLoginPayload, ControlMessage, DeviceStatusPayload, Envelope, StreamEndReason,
    StreamEndedPayload,
};

use crate::AgentState;

/// Run the control channel loop — connects, authenticates, then runs the
/// bidirectional message loop. Reconnects on failure with exponential backoff.
pub async fn run(
    state: Arc<AgentState>,
    control_url: &str,
    enrollment_token: Option<&str>,
    hostname: &str,
    heartbeat_interval: u64,
    mut outgoing_rx: mpsc::Receiver<String>,
) -> anyhow::Result<()> {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        tracing::info!(url = %control_url, "connecting to control plane");

        match connect_and_run(
            &state,
            control_url,
            enrollment_token,
            hostname,
            heartbeat_interval,
            &mut outgoing_rx,
        )
        .await
        {
            Ok(()) => {
                tracing::info!("control connection closed cleanly");
                // Check if shutting down
                if *state.shutdown.borrow() {
                    return Ok(());
                }
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(error = %e, "control connection failed");
            }
        }

        // Check shutdown before reconnecting
        if *state.shutdown.borrow() {
            return Ok(());
        }

        tracing::info!(backoff_s = backoff.as_secs(), "reconnecting");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn connect_and_run(
    state: &Arc<AgentState>,
    control_url: &str,
    enrollment_token: Option<&str>,
    hostname: &str,
    heartbeat_interval: u64,
    outgoing_rx: &mut mpsc::Receiver<String>,
) -> anyhow::Result<()> {
    // Connect
    let (ws, _response) = tokio_tungstenite::connect_async(control_url).await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    tracing::info!("WebSocket connected");

    // ── Authenticate ────────────────────────────────────────────
    let auth_payload = AuthLoginPayload {
        enrollment_token: enrollment_token.map(|s| s.to_string()),
        device_key: None, // TODO: use saved device key for re-auth
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        hostname: hostname.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    };

    let auth_msg = AgentMessage::AuthLogin(auth_payload);
    let envelope = Envelope::new("auth.login", &auth_msg);
    let json = serde_json::to_string(&envelope)?;
    ws_tx.send(Message::Text(json.into())).await?;

    // Wait for auth response
    let auth_response = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => {
            let envelope: Envelope = serde_json::from_str(&text)?;
            let resp: ControlMessage = envelope.parse_payload()?;
            match resp {
                ControlMessage::AuthLoginResponse(r) => r,
                _ => anyhow::bail!("unexpected response to auth.login"),
            }
        }
        Some(Ok(Message::Close(_))) => anyhow::bail!("connection closed during auth"),
        Some(Err(e)) => anyhow::bail!("WebSocket error during auth: {e}"),
        None => anyhow::bail!("connection closed during auth"),
        _ => anyhow::bail!("unexpected message type during auth"),
    };

    if !auth_response.success {
        let err = auth_response.error.unwrap_or_default();
        anyhow::bail!("authentication failed: {err}");
    }

    let sender_id = auth_response
        .sender_id
        .ok_or_else(|| anyhow::anyhow!("missing sender_id in auth response"))?;

    tracing::info!(sender_id = %sender_id, "authenticated");

    // Store sender_id and session token
    {
        *state.sender_id.lock().await = Some(sender_id.clone());
        *state.session_token.lock().await = auth_response.session_token;
    }

    // ── Heartbeat + message loop ────────────────────────────────
    let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_interval));
    let mut shutdown = state.shutdown.clone();

    loop {
        tokio::select! {
            // Heartbeat tick
            _ = heartbeat.tick() => {
                let status = build_heartbeat(state).await;
                let envelope = Envelope::new("device.status", &status);
                let json = serde_json::to_string(&envelope)?;
                ws_tx.send(Message::Text(json.into())).await?;
            }

            // Incoming messages from control plane
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_control_message(state, &text).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("control plane closed connection");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "WebSocket read error");
                        break;
                    }
                    _ => {} // Ping/Pong handled by tungstenite
                }
            }

            // Outgoing messages (stats, stream.ended, etc.)
            msg = outgoing_rx.recv() => {
                if let Some(text) = msg {
                    ws_tx.send(Message::Text(text.into())).await?;
                }
            }

            // Shutdown signal
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("shutdown signal received, closing WebSocket");
                    let _ = ws_tx.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Build a device.status heartbeat payload.
async fn build_heartbeat(state: &AgentState) -> DeviceStatusPayload {
    let hw = state.hardware.scan().await;
    let pipeline = state.pipeline.lock().await;

    DeviceStatusPayload {
        network_interfaces: hw.interfaces,
        media_inputs: hw.inputs,
        stream_state: if pipeline.is_running() {
            StreamState::Live
        } else {
            StreamState::Idle
        },
        cpu_percent: hw.cpu_percent,
        mem_used_mb: hw.mem_used_mb,
        uptime_s: hw.uptime_s,
    }
}

/// Handle an incoming control message from the control plane.
async fn handle_control_message(state: &AgentState, raw: &str) {
    let envelope: Envelope = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("invalid message from control plane: {e}");
            return;
        }
    };

    match envelope.msg_type.as_str() {
        "stream.start" => {
            if let Ok(ControlMessage::StreamStart(payload)) =
                envelope.parse_payload::<ControlMessage>()
            {
                tracing::info!(stream_id = %payload.stream_id, "received stream.start");
                let mut pipeline = state.pipeline.lock().await;
                if let Err(e) = pipeline.start(payload.clone()) {
                    tracing::error!(error = %e, "failed to start pipeline");
                    // Send stream.ended with error
                    let ended = StreamEndedPayload {
                        stream_id: payload.stream_id,
                        reason: StreamEndReason::Error,
                        duration_s: 0,
                        total_bytes: 0,
                    };
                    let envelope = Envelope::new("stream.ended", &ended);
                    let json = serde_json::to_string(&envelope).unwrap();
                    let _ = state.control_tx.send(json).await;
                }
            }
        }
        "stream.stop" => {
            if let Ok(ControlMessage::StreamStop(payload)) =
                envelope.parse_payload::<ControlMessage>()
            {
                tracing::info!(stream_id = %payload.stream_id, "received stream.stop");
                let mut pipeline = state.pipeline.lock().await;
                let stats = pipeline.stop();
                // Send stream.ended
                let ended = StreamEndedPayload {
                    stream_id: payload.stream_id,
                    reason: StreamEndReason::ControlPlaneStop,
                    duration_s: stats.duration_s,
                    total_bytes: stats.total_bytes,
                };
                let envelope = Envelope::new("stream.ended", &ended);
                let json = serde_json::to_string(&envelope).unwrap();
                let _ = state.control_tx.send(json).await;
            }
        }
        "config.update" => {
            tracing::info!("received config.update (hot-reload not yet implemented)");
        }
        other => {
            tracing::debug!(msg_type = %other, "unhandled control message");
        }
    }
}
