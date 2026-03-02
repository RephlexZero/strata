//! WebSocket control channel to the cloud control plane.
//!
//! Handles:
//! - Connection with exponential backoff reconnect
//! - Authentication with capacity registration
//! - Heartbeat (receiver.status every N seconds)
//! - Incoming commands (receiver.stream.start, receiver.stream.stop)
//! - Outgoing messages (receiver.stream.stats, receiver.stream.ended)

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use strata_common::protocol::{
    Envelope, ReceiverAuthLoginPayload, ReceiverAuthLoginResponsePayload, ReceiverStatusPayload,
    ReceiverStreamEndedPayload, ReceiverStreamStartPayload, ReceiverStreamStopPayload,
    StreamEndReason,
};

use crate::ReceiverState;

/// Send a typed envelope to the control plane.
async fn send_envelope(state: &ReceiverState, msg_type: &str, payload: &impl serde::Serialize) {
    let envelope = Envelope::new(msg_type, payload);
    match serde_json::to_string(&envelope) {
        Ok(json) => {
            let _ = state.control_tx.send(json).await;
        }
        Err(e) => {
            tracing::error!(msg_type, error = %e, "failed to serialize envelope");
        }
    }
}

/// Run the control channel loop with reconnect.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    state: Arc<ReceiverState>,
    control_url: &str,
    enrollment_token: Option<&str>,
    hostname: &str,
    bind_host: &str,
    link_ports: Vec<u16>,
    max_streams: u32,
    region: Option<&str>,
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
            bind_host,
            &link_ports,
            max_streams,
            region,
            heartbeat_interval,
            &mut outgoing_rx,
        )
        .await
        {
            Ok(()) => {
                state
                    .control_connected
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                tracing::info!("control connection closed cleanly");
                if *state.shutdown.borrow() {
                    return Ok(());
                }
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                state
                    .control_connected
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(error = %e, "control connection failed");
            }
        }

        if *state.shutdown.borrow() {
            return Ok(());
        }

        tracing::info!(backoff_s = backoff.as_secs(), "reconnecting");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_and_run(
    state: &Arc<ReceiverState>,
    control_url: &str,
    enrollment_token: Option<&str>,
    hostname: &str,
    bind_host: &str,
    link_ports: &[u16],
    max_streams: u32,
    region: Option<&str>,
    heartbeat_interval: u64,
    outgoing_rx: &mut mpsc::Receiver<String>,
) -> anyhow::Result<()> {
    let (ws, _response) = tokio_tungstenite::connect_async(control_url).await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    tracing::info!("WebSocket connected");

    // ── Authenticate with capacity info ─────────────────────────
    let auth_payload = ReceiverAuthLoginPayload {
        enrollment_token: enrollment_token.map(|s| s.to_string()),
        receiver_version: env!("CARGO_PKG_VERSION").to_string(),
        hostname: hostname.to_string(),
        region: region.map(|s| s.to_string()),
        bind_host: bind_host.to_string(),
        link_ports: link_ports.to_vec(),
        max_streams,
    };

    let envelope = Envelope::new("auth.login", &auth_payload);
    let json = serde_json::to_string(&envelope)?;
    ws_tx.send(Message::Text(json.into())).await?;

    // Wait for auth response
    let auth_response = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => {
            let envelope: Envelope = serde_json::from_str(&text)?;
            let resp: ReceiverAuthLoginResponsePayload = envelope.parse_payload()?;
            resp
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

    let receiver_id = auth_response
        .receiver_id
        .ok_or_else(|| anyhow::anyhow!("missing receiver_id in auth response"))?;

    tracing::info!(receiver_id = %receiver_id, "authenticated");

    {
        *state.receiver_id.lock().await = Some(receiver_id.clone());
        *state.session_token.lock().await = auth_response.session_token;
    }
    state
        .control_connected
        .store(true, std::sync::atomic::Ordering::Relaxed);

    // ── Heartbeat + message loop ────────────────────────────────
    let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_interval));
    let mut shutdown = state.shutdown.clone();

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let status = build_heartbeat(state).await;
                let envelope = Envelope::new("receiver.status", &status);
                let json = serde_json::to_string(&envelope)?;
                ws_tx.send(Message::Text(json.into())).await?;
            }

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
                    _ => {}
                }
            }

            msg = outgoing_rx.recv() => {
                if let Some(text) = msg {
                    ws_tx.send(Message::Text(text.into())).await?;
                }
            }

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

async fn build_heartbeat(state: &ReceiverState) -> ReceiverStatusPayload {
    let pipelines = state.pipelines.lock().await;
    let active = pipelines.active_count() as u32;
    drop(pipelines);

    let sys = sysinfo::System::new_all();

    ReceiverStatusPayload {
        active_streams: active,
        max_streams: state.max_streams,
        cpu_percent: sys.global_cpu_usage(),
        mem_used_mb: sys.used_memory() / (1024 * 1024),
        uptime_s: sysinfo::System::uptime(),
    }
}

async fn handle_control_message(state: &ReceiverState, raw: &str) {
    let envelope: Envelope = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("invalid message from control plane: {e}");
            return;
        }
    };

    match envelope.msg_type.as_str() {
        "receiver.stream.start" => match envelope.parse_payload::<ReceiverStreamStartPayload>() {
            Ok(payload) => {
                tracing::info!(
                    stream_id = %payload.stream_id,
                    bind_ports = ?payload.bind_ports,
                    relay_url = ?payload.relay_url,
                    "received receiver.stream.start"
                );

                let mut pipelines = state.pipelines.lock().await;
                if let Err(e) = pipelines.start(
                    &payload.stream_id,
                    &state.bind_host,
                    &payload.bind_ports,
                    payload.relay_url.as_deref(),
                    &payload.bonding_config,
                ) {
                    tracing::error!(error = %e, "failed to start receiver pipeline");
                    let ended = ReceiverStreamEndedPayload {
                        stream_id: payload.stream_id,
                        reason: StreamEndReason::Error,
                        duration_s: 0,
                        total_bytes: 0,
                    };
                    drop(pipelines);
                    send_envelope(state, "receiver.stream.ended", &ended).await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to parse receiver.stream.start payload");
            }
        },
        "receiver.stream.stop" => {
            if let Ok(payload) = envelope.parse_payload::<ReceiverStreamStopPayload>() {
                tracing::info!(stream_id = %payload.stream_id, "received receiver.stream.stop");
                let mut pipelines = state.pipelines.lock().await;
                let stats = pipelines.stop(&payload.stream_id);
                drop(pipelines);

                if let Some(stats) = stats {
                    // Release ports back to pool
                    let mut pool = state.port_pool.lock().await;
                    pool.release(&stats.bind_ports);
                    drop(pool);

                    let ended = ReceiverStreamEndedPayload {
                        stream_id: payload.stream_id,
                        reason: StreamEndReason::ControlPlaneStop,
                        duration_s: stats.duration_s,
                        total_bytes: stats.total_bytes,
                    };
                    send_envelope(state, "receiver.stream.ended", &ended).await;
                }
            }
        }
        other => {
            tracing::debug!(msg_type = %other, "unhandled control message");
        }
    }
}
