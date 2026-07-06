//! Payload structs for every WebSocket message.
//!
//! Each payload appears as a variant of one of the direction enums in
//! [`crate::messages`] — that enum is the authoritative list of message
//! types; these are just the bodies.

use serde::{Deserialize, Serialize};

use crate::models::{LinkStats, MediaInput, NetworkInterface, StreamState};

// ── Agent → Control Plane ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthLoginPayload {
    /// One-time composite enrollment token (`<device_id>.<SECRET>`).
    /// Present only on first enrollment; consumed server-side.
    pub enrollment_token: Option<String>,
    /// Device id for reconnects — triggers an ed25519 challenge handshake
    /// against the public key stored at enrollment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// The device's ed25519 public key (base64). Sent at enrollment so the
    /// token can be single-use; reconnects authenticate by signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_public_key: Option<String>,
    pub agent_version: String,
    pub hostname: String,
    pub arch: String,
}

/// Server → device: prove possession of the enrolled private key by
/// signing this base64 nonce.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthChallengePayload {
    pub challenge: String,
}

/// Device → server: base64 ed25519 signature over the challenge string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthChallengeResponsePayload {
    pub device_id: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStatusPayload {
    pub network_interfaces: Vec<NetworkInterface>,
    pub media_inputs: Vec<MediaInput>,
    pub stream_state: StreamState,
    pub cpu_percent: f32,
    pub mem_used_mb: u32,
    pub uptime_s: u64,
    /// Current receiver URL (if configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_url: Option<String>,
    /// IDs of streams whose media pipeline is currently running on this
    /// device. The control plane reconciles its DB against this on every
    /// heartbeat — a WS drop alone never marks a stream dead.
    #[serde(default)]
    pub running_streams: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStatsPayload {
    pub stream_id: String,
    /// Sender that produced these stats (set by control plane).
    #[serde(default)]
    pub sender_id: String,
    pub uptime_s: u64,
    pub encoder_bitrate_kbps: u32,
    /// Epoch milliseconds when these stats were captured.
    #[serde(default)]
    pub timestamp_ms: u64,
    pub links: Vec<LinkStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_metrics: Option<crate::models::TransportSenderMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_metrics: Option<crate::models::TransportReceiverMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEndedPayload {
    pub stream_id: String,
    pub reason: StreamEndReason,
    pub duration_s: u64,
    pub total_bytes: u64,
    /// Human-readable detail for error/crash ends (e.g. "pipeline exit code 1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEndReason {
    UserStop,
    Error,
    ControlPlaneStop,
    AgentShutdown,
    Timeout,
    PipelineCrash,
}

// ── Control Plane → Agent ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthLoginResponsePayload {
    pub success: bool,
    pub sender_id: Option<String>,
    pub error: Option<String>,
}

/// First message a dashboard (browser) WebSocket client must send on
/// `/ws` to authenticate — mirrors the agent/receiver `auth.login`
/// handshake rather than a `?token=` query param, since tokens in URLs
/// end up in proxy/access logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardAuthPayload {
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardAuthResponsePayload {
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStartPayload {
    pub stream_id: String,
    pub source: SourceConfig,
    pub encoder: EncoderConfig,
    pub destinations: Vec<String>,
    pub bonding_config: serde_json::Value,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "rist_psk",
        alias = "strata_psk"
    )]
    pub psk: Option<String>,
    /// RTMP/RTMPS relay URL — when set, the sender tees its encoded
    /// output and pushes a parallel FLV stream to this URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    pub mode: String,
    pub device: Option<String>,
    pub uri: Option<String>,
    pub resolution: Option<String>,
    pub framerate: Option<u32>,
    /// Skip encoding — remux file source directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub bitrate_kbps: u32,
    pub tune: Option<String>,
    pub keyint_max: Option<u32>,
    /// Video codec: "h264" or "h265". Defaults to "h265".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Minimum bitrate for adaptation envelope (kbps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_bitrate_kbps: Option<u32>,
    /// Maximum bitrate for adaptation envelope (kbps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate_kbps: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStopPayload {
    pub stream_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdatePayload {
    /// Request-correlation ID — echoed back in `config.update.response`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Partial config — only fields present are updated.
    pub scheduler: Option<serde_json::Value>,
    /// Encoder parameters to hot-update.
    pub encoder: Option<EncoderConfigUpdate>,
    // No `fec` field: the former FecConfigUpdate (layer switch, BLEST
    // threshold) never had a producer — its dashboard knobs were deleted as
    // placebo in E1 — and FEC overhead is closed-loop adaptive; a manual pin
    // would fight the adaptation loop (see wiki/Control-Loop-Map.md).
}

/// Partial encoder config for hot-update (all fields optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EncoderConfigUpdate {
    pub bitrate_kbps: Option<u32>,
    pub tune: Option<String>,
    pub keyint_max: Option<u32>,
}

/// Response to config.update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdateResponsePayload {
    pub request_id: Option<String>,
    pub success: bool,
    pub error: Option<String>,
}

/// Command to switch the active video source on a running pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSwitchPayload {
    /// Request-correlation ID — echoed back in `source.switch.response`.
    /// Optional for wire compatibility with older control planes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Source mode: "test", "v4l2", "uri".
    pub mode: String,
    /// V4L2 device path (used when mode = "v4l2").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Media URI (used when mode = "uri").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Test pattern name (used when mode = "test").
    /// E.g. "smpte", "ball", "snow", "black".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// Command to manage a network interface on the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceCommandPayload {
    /// Request-correlation ID — echoed back in `interface.command.response`.
    /// Optional for wire compatibility with older control planes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The interface name (e.g. "wwan0").
    pub interface: String,
    /// Action: "enable", "disable", "lock_band", "set_priority".
    pub action: String,
    /// Band to lock to (only used when action = "lock_band").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<String>,
    /// Priority to set (only used when action = "set_priority").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u32>,
    /// APN to set (only used when action = "set_apn").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apn: Option<String>,
    /// SIM PIN to set (only used when action = "set_apn").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_pin: Option<String>,
    /// Roaming toggle (only used when action = "set_apn").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roaming: Option<bool>,
}

/// Response to an interface command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceCommandResponsePayload {
    /// Echoed request-correlation ID (None from older agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub success: bool,
    pub interface: String,
    pub action: String,
    pub error: Option<String>,
}

/// Response to source.switch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSwitchResponsePayload {
    /// Echoed request-correlation ID (None from older agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub success: bool,
    /// The mode that was requested ("test", "v4l2", "uri").
    pub mode: String,
    pub error: Option<String>,
}

/// Set receiver/config on the agent (proxied from control plane).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSetPayload {
    pub request_id: String,
    pub receiver_url: Option<String>,
}

/// Response to config.set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSetResponsePayload {
    pub request_id: String,
    pub success: bool,
    pub receiver_url: Option<String>,
}

/// Request the agent to run a connectivity test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunPayload {
    pub request_id: String,
}

/// Connectivity test results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunResponsePayload {
    pub request_id: String,
    pub cloud_reachable: bool,
    pub cloud_connected: bool,
    pub receiver_reachable: bool,
    pub receiver_url: Option<String>,
    pub enrolled: bool,
    pub control_url: Option<String>,
}

/// Request the agent to scan for new network interfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfacesScanPayload {
    pub request_id: String,
}

/// Interface scan results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfacesScanResponsePayload {
    pub request_id: String,
    pub discovered: Vec<String>,
    pub total: usize,
}

/// Request agent to list files in a directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesListPayload {
    pub request_id: String,
    /// Absolute path to list; defaults to a sensible root if absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// A single entry returned by `files.list.response`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Response to a `files.list` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesListResponsePayload {
    pub request_id: String,
    pub path: String,
    pub entries: Vec<FileEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Diagnostics Protocol ────────────────────────────────────────────

/// Run a network diagnostic tool (ping, traceroute, speedtest).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkToolPayload {
    pub request_id: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkToolResponsePayload {
    pub request_id: String,
    pub tool: String,
    pub output: String,
    pub success: bool,
}

/// Capture network packets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PcapCapturePayload {
    pub request_id: String,
    pub duration_secs: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PcapCaptureResponsePayload {
    pub request_id: String,
    pub download_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size_bytes: Option<u64>,
    pub duration_secs: u32,
}

/// Fetch device logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsRequestPayload {
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLineEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsResponsePayload {
    pub request_id: String,
    pub service: String,
    pub lines: Vec<LogLineEntry>,
}

/// Send a power command (reboot, shutdown, restart_agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerCommandPayload {
    pub request_id: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerCommandResponsePayload {
    pub request_id: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// TLS certificate status query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsStatusPayload {
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsStatusResponsePayload {
    pub request_id: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert_subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert_issuer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry: Option<String>,
    pub self_signed: bool,
}

/// TLS certificate renewal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsRenewPayload {
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsRenewResponsePayload {
    pub request_id: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Export agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigExportPayload {
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigExportResponsePayload {
    pub request_id: String,
    pub config: serde_json::Value,
}

/// Import agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigImportPayload {
    pub request_id: String,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigImportResponsePayload {
    pub request_id: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Check for OTA updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatesCheckPayload {
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatesCheckResponsePayload {
    pub request_id: String,
    pub current_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    pub update_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_size_bytes: Option<u64>,
}

/// Install OTA update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatesInstallPayload {
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatesInstallResponsePayload {
    pub request_id: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Set stream destinations (fan-out).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamDestinationsPayload {
    pub request_id: String,
    pub destination_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamDestinationsResponsePayload {
    pub request_id: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Configure receiver jitter buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitterBufferPayload {
    pub request_id: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub static_ms: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitterBufferResponsePayload {
    pub request_id: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Receiver → Control Plane ────────────────────────────────────────

/// Auth payload sent by a receiver daemon when connecting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverAuthLoginPayload {
    /// One-time composite enrollment token (`<device_id>.<SECRET>`).
    pub enrollment_token: Option<String>,
    /// Device id for reconnects (ed25519 challenge handshake).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// The device's ed25519 public key (base64), sent at enrollment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_public_key: Option<String>,
    pub receiver_version: String,
    pub hostname: String,
    pub region: Option<String>,
    /// Public IP or hostname the receiver is reachable at.
    pub bind_host: String,
    /// UDP ports available for incoming bonded streams.
    pub link_ports: Vec<u16>,
    /// Maximum concurrent streams this receiver can handle.
    pub max_streams: u32,
}

/// Auth response sent to a receiver daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverAuthLoginResponsePayload {
    pub success: bool,
    pub receiver_id: Option<String>,
    pub error: Option<String>,
}

/// Control plane asks the receiver to start receiving a stream. The
/// receiver owns its port pool: it allocates `link_count` UDP ports and
/// answers with `receiver.stream.started` carrying the actual ports (E6) —
/// two concurrent streams can no longer collide on one receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverStreamStartPayload {
    /// Request-correlation ID — echoed back in `receiver.stream.started`.
    pub request_id: String,
    pub stream_id: String,
    /// How many link ports the stream needs (one per sender link).
    pub link_count: u32,
    /// Optional RTMP/HLS relay URL.
    pub relay_url: Option<String>,
    /// Optional bonding config (scheduler params, etc).
    #[serde(default)]
    pub bonding_config: serde_json::Value,
}

/// Receiver's answer to `receiver.stream.start`: the allocated ports, or
/// why allocation/spawn failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverStreamStartedPayload {
    pub request_id: String,
    pub stream_id: String,
    pub success: bool,
    /// The UDP ports actually bound for this stream (empty on failure).
    #[serde(default)]
    pub bind_ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Control plane tells the receiver to stop a stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverStreamStopPayload {
    pub stream_id: String,
    pub reason: String,
}

/// Receiver reports a stream has ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverStreamEndedPayload {
    pub stream_id: String,
    pub reason: StreamEndReason,
    pub duration_s: u64,
    pub total_bytes: u64,
    /// Human-readable detail for error/crash ends (e.g. "pipeline exit code 1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Receiver reports per-second stats for a stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverStreamStatsPayload {
    pub stream_id: String,
    pub receiver_id: String,
    pub uptime_s: u64,
    pub timestamp_ms: u64,
    pub links: Vec<crate::models::LinkStats>,
    /// HLS egress health (None for non-HLS relays or older pipelines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<crate::models::EgressStats>,
}

/// Receiver heartbeat with capacity info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverStatusPayload {
    pub active_streams: u32,
    pub max_streams: u32,
    pub cpu_percent: f32,
    pub mem_used_mb: u64,
    pub uptime_s: u64,
    /// IDs of streams whose pipeline is currently running on this receiver.
    /// The control plane reconciles against this on every heartbeat (same
    /// contract as `DeviceStatusPayload::running_streams`).
    #[serde(default)]
    pub running_streams: Vec<String>,
}
