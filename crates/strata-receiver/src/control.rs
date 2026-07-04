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

use strata_protocol::{
    AuthChallengeResponsePayload, Envelope, ReceiverAuthLoginPayload, ReceiverControlMessage,
    ReceiverMessage, ReceiverStatusPayload, ReceiverStreamEndedPayload, StreamEndReason,
};

use crate::ReceiverState;

/// Send a typed message to the control plane.
async fn send_message(state: &ReceiverState, msg: &ReceiverMessage) {
    match Envelope::from_message(msg).and_then(|e| serde_json::to_string(&e)) {
        Ok(json) => {
            let _ = state.control_tx.send(json).await;
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize message");
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
    const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    // Fraction of the deterministic backoff added as random jitter, so a
    // control-plane restart doesn't make every receiver reconnect in
    // lockstep (a thundering herd against the O(n·argon2) enrollment scan
    // — E9).
    const BACKOFF_JITTER_FRACTION: f64 = 0.2;

    let mut backoff = INITIAL_BACKOFF;

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
                backoff = INITIAL_BACKOFF;
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

        let jittered = backoff.mul_f64(1.0 + rand::random::<f64>() * BACKOFF_JITTER_FRACTION);
        tracing::info!(backoff_s = jittered.as_secs_f64(), "reconnecting");
        tokio::time::sleep(jittered).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
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
    // Enrolled devices authenticate by ed25519 challenge; otherwise enroll
    // with the one-time token, sending the public key so the token is
    // consumed server-side.
    let identity = state.identity.lock().await.clone();
    let enrolled_device_id = identity.device_id.clone();

    let auth_payload = ReceiverAuthLoginPayload {
        enrollment_token: if enrolled_device_id.is_none() {
            enrollment_token.map(|s| s.to_string())
        } else {
            None
        },
        device_id: enrolled_device_id.clone(),
        device_public_key: Some(identity.public_key.clone()),
        receiver_version: env!("CARGO_PKG_VERSION").to_string(),
        hostname: hostname.to_string(),
        region: region.map(|s| s.to_string()),
        bind_host: bind_host.to_string(),
        link_ports: link_ports.to_vec(),
        max_streams,
    };

    let envelope = Envelope::from_message(&ReceiverMessage::AuthLogin(auth_payload))?;
    let json = serde_json::to_string(&envelope)?;
    ws_tx.send(Message::Text(json.into())).await?;

    // Response is either the login result (enrollment) or a challenge to
    // sign (device-key reconnect).
    let auth_response = loop {
        let text = match ws_rx.next().await {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Close(_))) => anyhow::bail!("connection closed during auth"),
            Some(Err(e)) => anyhow::bail!("WebSocket error during auth: {e}"),
            None => anyhow::bail!("connection closed during auth"),
            _ => anyhow::bail!("unexpected message type during auth"),
        };
        let envelope: Envelope = serde_json::from_str(&text)?;
        match envelope.parse_message() {
            Ok(ReceiverControlMessage::AuthChallenge(challenge)) => {
                let Some(ref device_id) = enrolled_device_id else {
                    anyhow::bail!("received auth.challenge without an enrolled identity");
                };
                let signature = strata_common::auth::sign_challenge(
                    &identity.private_key,
                    &challenge.challenge,
                )
                .map_err(|e| anyhow::anyhow!("failed to sign challenge: {e}"))?;
                let response =
                    ReceiverMessage::AuthChallengeResponse(AuthChallengeResponsePayload {
                        device_id: device_id.clone(),
                        signature,
                    });
                let envelope = Envelope::from_message(&response)?;
                ws_tx
                    .send(Message::Text(serde_json::to_string(&envelope)?.into()))
                    .await?;
            }
            Ok(ReceiverControlMessage::AuthLoginResponse(resp)) => break resp,
            _ => anyhow::bail!("unexpected message during auth: {}", envelope.msg_type),
        }
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
        let mut identity = state.identity.lock().await;
        if identity.device_id.as_deref() != Some(&receiver_id) {
            identity.device_id = Some(receiver_id.clone());
            if let Err(e) = identity.save(&state.identity_path) {
                tracing::error!(
                    error = %e,
                    path = %state.identity_path.display(),
                    "FAILED to persist device identity — this device cannot re-authenticate after restart"
                );
            }
        }
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
                let envelope = Envelope::from_message(&ReceiverMessage::Status(status))?;
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
    let running_streams = pipelines.running_ids();
    drop(pipelines);

    let sys = sysinfo::System::new_all();

    ReceiverStatusPayload {
        active_streams: running_streams.len() as u32,
        max_streams: state.max_streams,
        cpu_percent: sys.global_cpu_usage(),
        mem_used_mb: sys.used_memory() / (1024 * 1024),
        uptime_s: sysinfo::System::uptime(),
        running_streams,
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

    let msg: ReceiverControlMessage = match envelope.parse_message() {
        Ok(m) => m,
        Err(_) => {
            tracing::debug!(msg_type = %envelope.msg_type, "unhandled control message");
            return;
        }
    };

    match msg {
        ReceiverControlMessage::AuthLoginResponse(_) | ReceiverControlMessage::AuthChallenge(_) => {
            tracing::debug!("unexpected auth message outside handshake");
        }
        ReceiverControlMessage::StreamStart(payload) => {
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
                send_message(state, &ReceiverMessage::StreamEnded(ended)).await;
            }
        }
        ReceiverControlMessage::StreamStop(payload) => {
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
                send_message(state, &ReceiverMessage::StreamEnded(ended)).await;
            }
        }
    }
}
