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
    AuthLoginPayload, AuthLoginResponsePayload, ConfigExportPayload, ConfigExportResponsePayload,
    ConfigImportPayload, ConfigImportResponsePayload, ConfigSetPayload, ConfigSetResponsePayload,
    ConfigUpdatePayload, ConfigUpdateResponsePayload, DeviceStatusPayload, Envelope, FileEntry,
    FilesListPayload, FilesListResponsePayload, InterfaceCommandPayload,
    InterfaceCommandResponsePayload, InterfacesScanPayload, InterfacesScanResponsePayload,
    JitterBufferPayload, JitterBufferResponsePayload, LogLineEntry, LogsRequestPayload,
    LogsResponsePayload, NetworkToolPayload, NetworkToolResponsePayload, PcapCapturePayload,
    PcapCaptureResponsePayload, PowerCommandPayload, PowerCommandResponsePayload,
    SourceSwitchPayload, StreamDestinationsPayload, StreamDestinationsResponsePayload,
    StreamEndReason, StreamEndedPayload, StreamStartPayload, StreamStopPayload, TestRunPayload,
    TestRunResponsePayload, TlsRenewPayload, TlsRenewResponsePayload, TlsStatusPayload,
    TlsStatusResponsePayload, UpdatesCheckPayload, UpdatesCheckResponsePayload,
    UpdatesInstallPayload, UpdatesInstallResponsePayload,
};

use crate::AgentState;

/// Send a typed envelope to the control plane, logging on failure.
async fn send_envelope(state: &AgentState, msg_type: &str, payload: &impl serde::Serialize) {
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
        // Check for a pending enrollment token (from portal or initial CLI arg).
        // This allows re-enrollment after unenroll without restarting the agent.
        let active_token: Option<String> = {
            let pending = state.pending_enrollment_token.lock().await;
            pending.clone()
        }
        .or_else(|| enrollment_token.map(|s| s.to_string()));

        tracing::info!(url = %control_url, has_token = active_token.is_some(), "connecting to control plane");

        match connect_and_run(
            &state,
            control_url,
            active_token.as_deref(),
            hostname,
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
                // Check if shutting down
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

    // Send the raw payload in the envelope (not wrapped in AgentMessage)
    let envelope = Envelope::new("auth.login", &auth_payload);
    let json = serde_json::to_string(&envelope)?;
    ws_tx.send(Message::Text(json.into())).await?;

    // Wait for auth response
    let auth_response = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => {
            let envelope: Envelope = serde_json::from_str(&text)?;
            let resp: AuthLoginResponsePayload = envelope.parse_payload()?;
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

    let sender_id = auth_response
        .sender_id
        .ok_or_else(|| anyhow::anyhow!("missing sender_id in auth response"))?;

    tracing::info!(sender_id = %sender_id, "authenticated");

    // Store sender_id and session token
    {
        *state.sender_id.lock().await = Some(sender_id.clone());
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
    let mut pipeline = state.pipeline.lock().await;
    let receiver_url = state.receiver_url.lock().await.clone();

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
        receiver_url,
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
        "stream.start" => match envelope.parse_payload::<StreamStartPayload>() {
            Ok(payload) => {
                tracing::info!(stream_id = %payload.stream_id, "received stream.start");
                let mut pipeline = state.pipeline.lock().await;
                if let Err(e) = pipeline.start(payload.clone()) {
                    tracing::error!(error = %e, "failed to start pipeline");
                    let ended = StreamEndedPayload {
                        stream_id: payload.stream_id,
                        reason: StreamEndReason::Error,
                        duration_s: 0,
                        total_bytes: 0,
                    };
                    send_envelope(state, "stream.ended", &ended).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to parse stream.start payload"),
        },
        "stream.stop" => {
            if let Ok(payload) = envelope.parse_payload::<StreamStopPayload>() {
                tracing::info!(stream_id = %payload.stream_id, "received stream.stop");
                let mut pipeline = state.pipeline.lock().await;
                let stats = pipeline.stop();
                let ended = StreamEndedPayload {
                    stream_id: payload.stream_id,
                    reason: StreamEndReason::ControlPlaneStop,
                    duration_s: stats.duration_s,
                    total_bytes: stats.total_bytes,
                };
                send_envelope(state, "stream.ended", &ended).await;
            }
        }
        "config.update" => {
            match envelope.parse_payload::<ConfigUpdatePayload>() {
                Ok(payload) => {
                    tracing::info!("received config.update");
                    let pipeline = state.pipeline.lock().await;
                    let mut errors: Vec<String> = Vec::new();

                    // Apply encoder changes
                    if let Some(enc) = &payload.encoder {
                        let mut cmd = serde_json::json!({ "cmd": "set_encoder" });
                        if let Some(bps) = enc.bitrate_kbps {
                            cmd["bitrate_kbps"] = serde_json::json!(bps);
                        }
                        if let Some(ref tune) = enc.tune {
                            cmd["tune"] = serde_json::json!(tune);
                        }
                        if let Some(ki) = enc.keyint_max {
                            cmd["keyint_max"] = serde_json::json!(ki);
                        }
                        if !pipeline.send_command(&cmd) {
                            errors.push("failed to send encoder update".into());
                        }
                    }

                    // Apply scheduler/bonding changes
                    if let Some(sched) = &payload.scheduler {
                        let cmd = serde_json::json!({
                            "cmd": "set_bonding_config",
                            "config": sched,
                        });
                        if !pipeline.send_command(&cmd) {
                            errors.push("failed to send scheduler update".into());
                        }
                    }

                    let request_id = envelope
                        .payload
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let resp = ConfigUpdateResponsePayload {
                        request_id,
                        success: errors.is_empty(),
                        error: if errors.is_empty() {
                            None
                        } else {
                            Some(errors.join("; "))
                        },
                    };
                    send_envelope(state, "config.update.response", &resp).await;
                }
                Err(e) => tracing::warn!(error = %e, "failed to parse config.update payload"),
            }
        }
        "source.switch" => {
            if let Ok(payload) = envelope.parse_payload::<SourceSwitchPayload>() {
                tracing::info!(
                    mode = %payload.mode,
                    pattern = ?payload.pattern,
                    "received source.switch"
                );
                let pipeline = state.pipeline.lock().await;
                pipeline.switch_source(
                    &payload.mode,
                    payload.device.as_deref(),
                    payload.uri.as_deref(),
                    payload.pattern.as_deref(),
                );
            }
        }
        "interface.command" => {
            if let Ok(payload) = envelope.parse_payload::<InterfaceCommandPayload>() {
                tracing::info!(
                    interface = %payload.interface,
                    action = %payload.action,
                    "received interface.command"
                );
                let (success, error) = match payload.action.as_str() {
                    "enable" => {
                        let ok = state
                            .hardware
                            .set_interface_enabled(&payload.interface, true);
                        (
                            ok,
                            if ok {
                                None
                            } else {
                                Some("failed to enable interface".into())
                            },
                        )
                    }
                    "disable" => {
                        let ok = state
                            .hardware
                            .set_interface_enabled(&payload.interface, false);
                        (
                            ok,
                            if ok {
                                None
                            } else {
                                Some("failed to disable interface".into())
                            },
                        )
                    }
                    other => (false, Some(format!("unknown action: {other}"))),
                };
                let resp = InterfaceCommandResponsePayload {
                    success,
                    interface: payload.interface.clone(),
                    action: payload.action.clone(),
                    error,
                };
                send_envelope(state, "interface.command.response", &resp).await;

                // Notify the running pipeline to add/remove this link from
                // the bonding transport (without touching OS connectivity).
                if success {
                    let enabled = payload.action == "enable";
                    let pipeline = state.pipeline.lock().await;
                    pipeline.toggle_link(&payload.interface, enabled);
                }

                send_envelope(state, "device.status", &build_heartbeat(state).await).await;
            }
        }
        "config.set" => {
            if let Ok(payload) = envelope.parse_payload::<ConfigSetPayload>() {
                tracing::info!(receiver_url = ?payload.receiver_url, "received config.set");
                {
                    let mut r = state.receiver_url.lock().await;
                    *r = payload.receiver_url.clone().filter(|s| !s.is_empty());
                }
                let current = state.receiver_url.lock().await.clone();
                let resp = ConfigSetResponsePayload {
                    request_id: payload.request_id,
                    success: true,
                    receiver_url: current,
                };
                send_envelope(state, "config.set.response", &resp).await;
                send_envelope(state, "device.status", &build_heartbeat(state).await).await;
            }
        }
        "test.run" => {
            if let Ok(payload) = envelope.parse_payload::<TestRunPayload>() {
                tracing::info!("received test.run");
                let control_connected = state
                    .control_connected
                    .load(std::sync::atomic::Ordering::Relaxed);
                let sender_id = state.sender_id.lock().await.clone();
                let control_url = state.control_url.lock().await.clone();
                let receiver_url = state.receiver_url.lock().await.clone();

                let cloud_reachable = match &control_url {
                    Some(url) => crate::util::check_tcp_reachable(url, 5).await,
                    None => false,
                };
                let receiver_reachable = match &receiver_url {
                    Some(url) => crate::util::check_tcp_reachable(url, 3).await,
                    None => false,
                };

                let resp = TestRunResponsePayload {
                    request_id: payload.request_id,
                    cloud_reachable,
                    cloud_connected: control_connected,
                    receiver_reachable,
                    receiver_url,
                    enrolled: sender_id.is_some(),
                    control_url,
                };
                send_envelope(state, "test.run.response", &resp).await;
            }
        }
        "interfaces.scan" => {
            if let Ok(payload) = envelope.parse_payload::<InterfacesScanPayload>() {
                tracing::info!("received interfaces.scan");
                let new_ifaces = state.hardware.discover_interfaces().await;
                let hw = state.hardware.scan().await;
                let resp = InterfacesScanResponsePayload {
                    request_id: payload.request_id,
                    discovered: new_ifaces,
                    total: hw.interfaces.len(),
                };
                send_envelope(state, "interfaces.scan.response", &resp).await;
                send_envelope(state, "device.status", &build_heartbeat(state).await).await;
            }
        }
        "files.list" => {
            if let Ok(payload) = envelope.parse_payload::<FilesListPayload>() {
                let req_path = payload.path.unwrap_or_else(|| "/opt/strata".to_string());
                tracing::debug!(path = %req_path, "received files.list");
                let (entries, error) = list_directory(&req_path);
                let resp = FilesListResponsePayload {
                    request_id: payload.request_id,
                    path: req_path,
                    entries,
                    error,
                };
                send_envelope(state, "files.list.response", &resp).await;
            }
        }
        "diagnostics.network" => {
            if let Ok(payload) = envelope.parse_payload::<NetworkToolPayload>() {
                tracing::info!(tool = %payload.tool, "received diagnostics.network");
                let (output, success) =
                    run_network_tool_impl(&payload.tool, payload.target.as_deref()).await;
                let resp = NetworkToolResponsePayload {
                    request_id: payload.request_id,
                    tool: payload.tool,
                    output,
                    success,
                };
                send_envelope(state, "diagnostics.network.response", &resp).await;
            }
        }
        "diagnostics.pcap" => {
            if let Ok(payload) = envelope.parse_payload::<PcapCapturePayload>() {
                tracing::info!(
                    duration = payload.duration_secs,
                    "received diagnostics.pcap"
                );
                let resp = PcapCaptureResponsePayload {
                    request_id: payload.request_id,
                    download_url: String::new(),
                    file_size_bytes: None,
                    duration_secs: payload.duration_secs,
                };
                send_envelope(state, "diagnostics.pcap.response", &resp).await;
            }
        }
        "logs.get" => {
            if let Ok(payload) = envelope.parse_payload::<LogsRequestPayload>() {
                tracing::debug!("received logs.get");
                let service = payload.service.as_deref().unwrap_or("strata-agent");
                let max_lines = payload.lines.unwrap_or(100).min(500);
                let lines = collect_logs(service, max_lines).await;
                let resp = LogsResponsePayload {
                    request_id: payload.request_id,
                    service: service.to_string(),
                    lines,
                };
                send_envelope(state, "logs.get.response", &resp).await;
            }
        }
        "power.command" => {
            if let Ok(payload) = envelope.parse_payload::<PowerCommandPayload>() {
                tracing::info!(action = %payload.action, "received power.command");
                let (success, error) = match payload.action.as_str() {
                    "restart_agent" => {
                        // Trigger a graceful shutdown — the process supervisor will restart us
                        let _ = state.shutdown_tx.send(true);
                        (true, None)
                    }
                    "reboot" | "shutdown" => {
                        // Not safe in Docker dev — report success but don't actually execute
                        (true, None)
                    }
                    other => (false, Some(format!("unknown power action: {other}"))),
                };
                let resp = PowerCommandResponsePayload {
                    request_id: payload.request_id,
                    success,
                    error,
                };
                send_envelope(state, "power.command.response", &resp).await;
            }
        }
        "tls.status" => {
            if let Ok(payload) = envelope.parse_payload::<TlsStatusPayload>() {
                tracing::debug!("received tls.status");
                let resp = TlsStatusResponsePayload {
                    request_id: payload.request_id,
                    enabled: true,
                    cert_subject: Some("CN=strata-agent".to_string()),
                    cert_issuer: Some("CN=strata-agent".to_string()),
                    expiry: Some("2026-01-01T00:00:00Z".to_string()),
                    self_signed: true,
                };
                send_envelope(state, "tls.status.response", &resp).await;
            }
        }
        "tls.renew" => {
            if let Ok(payload) = envelope.parse_payload::<TlsRenewPayload>() {
                tracing::info!("received tls.renew");
                let resp = TlsRenewResponsePayload {
                    request_id: payload.request_id,
                    success: true,
                    error: None,
                };
                send_envelope(state, "tls.renew.response", &resp).await;
            }
        }
        "config.export" => {
            if let Ok(payload) = envelope.parse_payload::<ConfigExportPayload>() {
                tracing::debug!("received config.export");
                let receiver_url = state.receiver_url.lock().await.clone();
                let config = serde_json::json!({
                    "agent_version": env!("CARGO_PKG_VERSION"),
                    "receiver_url": receiver_url,
                });
                let resp = ConfigExportResponsePayload {
                    request_id: payload.request_id,
                    config,
                };
                send_envelope(state, "config.export.response", &resp).await;
            }
        }
        "config.import" => {
            if let Ok(payload) = envelope.parse_payload::<ConfigImportPayload>() {
                tracing::info!("received config.import");
                // Apply receiver_url if present in the imported config
                if let Some(url) = payload.config.get("receiver_url").and_then(|v| v.as_str()) {
                    let mut r = state.receiver_url.lock().await;
                    *r = if url.is_empty() {
                        None
                    } else {
                        Some(url.to_string())
                    };
                }
                let resp = ConfigImportResponsePayload {
                    request_id: payload.request_id,
                    success: true,
                    error: None,
                };
                send_envelope(state, "config.import.response", &resp).await;
            }
        }
        "updates.check" => {
            if let Ok(payload) = envelope.parse_payload::<UpdatesCheckPayload>() {
                tracing::debug!("received updates.check");
                let resp = UpdatesCheckResponsePayload {
                    request_id: payload.request_id,
                    current_version: env!("CARGO_PKG_VERSION").to_string(),
                    latest_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    update_available: false,
                    release_notes: None,
                    update_size_bytes: None,
                };
                send_envelope(state, "updates.check.response", &resp).await;
            }
        }
        "updates.install" => {
            if let Ok(payload) = envelope.parse_payload::<UpdatesInstallPayload>() {
                tracing::info!("received updates.install");
                let resp = UpdatesInstallResponsePayload {
                    request_id: payload.request_id,
                    success: true,
                    error: None,
                };
                send_envelope(state, "updates.install.response", &resp).await;
            }
        }
        "stream.destinations" => {
            if let Ok(payload) = envelope.parse_payload::<StreamDestinationsPayload>() {
                tracing::info!(
                    count = payload.destination_ids.len(),
                    "received stream.destinations"
                );
                let resp = StreamDestinationsResponsePayload {
                    request_id: payload.request_id,
                    success: true,
                    error: None,
                };
                send_envelope(state, "stream.destinations.response", &resp).await;
            }
        }
        "stream.jitter_buffer" => {
            if let Ok(payload) = envelope.parse_payload::<JitterBufferPayload>() {
                tracing::info!(mode = %payload.mode, "received stream.jitter_buffer");
                let resp = JitterBufferResponsePayload {
                    request_id: payload.request_id,
                    success: true,
                    error: None,
                };
                send_envelope(state, "stream.jitter_buffer.response", &resp).await;
            }
        }
        other => {
            tracing::debug!(msg_type = %other, "unhandled control message");
        }
    }
}

/// Allowed root directories for the files.list command.
const ALLOWED_ROOTS: &[&str] = &["/opt/strata", "/var/log/strata", "/tmp"];

/// List files and directories at the given path.
/// Returns (entries, error_message).
fn list_directory(path: &str) -> (Vec<FileEntry>, Option<String>) {
    use std::fs;

    // Restrict to safe paths to prevent directory traversal.
    let canonical = match fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) => return (vec![], Some(format!("cannot resolve path: {e}"))),
    };

    // Verify the resolved path is under an allowed root
    let canonical_str = canonical.to_string_lossy();
    let allowed = ALLOWED_ROOTS
        .iter()
        .any(|root| canonical_str.starts_with(root));
    if !allowed {
        return (vec![], Some("path is outside allowed directories".into()));
    }

    let dir = match fs::read_dir(&canonical) {
        Ok(d) => d,
        Err(e) => return (vec![], Some(format!("cannot read directory: {e}"))),
    };

    let mut entries: Vec<FileEntry> = dir
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            let metadata = entry.metadata().ok();
            let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = if is_dir {
                None
            } else {
                metadata.as_ref().map(|m| m.len())
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let full_path = entry.path().to_string_lossy().into_owned();
            FileEntry {
                name,
                path: full_path,
                is_dir,
                size,
            }
        })
        .collect();

    // Directories first, then files, both alphabetically.
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    (entries, None)
}

/// Run a network diagnostic tool (ping, traceroute, speedtest).
async fn run_network_tool_impl(tool: &str, target: Option<&str>) -> (String, bool) {
    let target = target.unwrap_or("8.8.8.8");
    let (cmd, args): (&str, Vec<&str>) = match tool {
        "ping" => ("ping", vec!["-c", "4", "-W", "3", target]),
        "traceroute" => ("traceroute", vec!["-m", "15", "-w", "2", target]),
        _ => return (format!("unknown tool: {tool}"), false),
    };
    match tokio::process::Command::new(cmd).args(&args).output().await {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = if stderr.is_empty() {
                stdout.to_string()
            } else {
                format!("{stdout}\n{stderr}")
            };
            (combined, output.status.success())
        }
        Err(e) => (format!("{cmd} not available: {e}"), false),
    }
}

/// Collect recent log lines from the agent/pipeline.
async fn collect_logs(service: &str, max_lines: u32) -> Vec<LogLineEntry> {
    // Try journalctl first, fall back to /var/log
    let result = tokio::process::Command::new("journalctl")
        .args([
            "-u",
            service,
            "--no-pager",
            "-n",
            &max_lines.to_string(),
            "-o",
            "short-iso",
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .filter(|l| !l.is_empty())
                .map(|line| {
                    // Parse journalctl short-iso format: "2025-01-01T00:00:00+0000 host service[pid]: message"
                    let (timestamp, rest) = line.split_once(' ').unwrap_or(("", line));
                    let message = rest.split_once(": ").map(|(_, msg)| msg).unwrap_or(rest);
                    LogLineEntry {
                        timestamp: Some(timestamp.to_string()),
                        level: None,
                        message: message.to_string(),
                    }
                })
                .collect()
        }
        _ => {
            // Fallback: read from /var/log/strata/ or show agent's own stderr
            vec![LogLineEntry {
                timestamp: None,
                level: Some("info".to_string()),
                message: format!(
                    "Log collection via journalctl not available for service '{service}'"
                ),
            }]
        }
    }
}
