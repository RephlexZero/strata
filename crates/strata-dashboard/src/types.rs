//! API response types — mirrors the server-side response structs.
//!
//! strata-common is not wasm-compatible (argon2/ed25519 deps), so we
//! duplicate the subset of types the dashboard needs here.

use serde::{Deserialize, Serialize};

// ── Auth ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub user_id: String,
    pub role: String,
}

// ── Senders ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct SenderSummary {
    pub id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub online: bool,
    pub last_seen_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SenderDetail {
    pub id: String,
    pub owner_id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub enrolled: bool,
    pub online: bool,
    pub last_seen_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateSenderRequest {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateSenderResponse {
    pub sender_id: String,
    pub enrollment_token: String,
}

/// Full sender status including hardware data (from agent heartbeat).
#[derive(Debug, Clone, Deserialize)]
pub struct SenderFullStatus {
    pub sender_id: Option<String>,
    pub online: Option<bool>,
    pub network_interfaces: Option<Vec<NetworkInterface>>,
    pub media_inputs: Option<Vec<MediaInput>>,
    pub stream_state: Option<String>,
    pub cpu_percent: Option<f32>,
    pub mem_used_mb: Option<u32>,
    pub uptime_s: Option<u64>,
    pub receiver_url: Option<String>,
}

/// Network interface status.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct NetworkInterface {
    pub name: String,
    #[serde(rename = "type", alias = "iface_type")]
    pub iface_type: String,
    pub state: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub ip: Option<String>,
    pub carrier: Option<String>,
    pub signal_dbm: Option<i32>,
    pub technology: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Media input.
#[derive(Debug, Clone, Deserialize)]
pub struct MediaInput {
    pub device: String,
    #[serde(rename = "type", alias = "input_type")]
    pub input_type: String,
    pub label: String,
    pub capabilities: Vec<String>,
    pub status: String,
}

/// Response from unenrolling a sender.
#[derive(Debug, Clone, Deserialize)]
pub struct UnenrollResponse {
    pub sender_id: String,
    pub enrollment_token: String,
    pub message: String,
}

/// Response from setting sender config.
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigSetResponse {
    pub request_id: Option<String>,
    pub success: bool,
    pub receiver_url: Option<String>,
}

/// Connectivity test result.
#[derive(Debug, Clone, Deserialize)]
pub struct TestRunResponse {
    pub cloud_reachable: bool,
    pub cloud_connected: bool,
    pub receiver_reachable: bool,
    pub receiver_url: Option<String>,
    pub enrolled: bool,
    pub control_url: Option<String>,
}

/// Interface scan result.
#[derive(Debug, Clone, Deserialize)]
pub struct InterfaceScanResponse {
    pub discovered: Vec<String>,
    pub total: usize,
}

// ── Streams ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct StreamSummary {
    pub id: String,
    pub sender_id: String,
    pub state: String,
    pub started_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamDetail {
    pub id: String,
    pub sender_id: String,
    pub destination_id: Option<String>,
    pub state: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub total_bytes: i64,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartStreamRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder: Option<EncoderConfig>,
}

/// Source configuration for stream start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framerate: Option<u32>,
}

/// Encoder configuration for stream start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub bitrate_kbps: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tune: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyint_max: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_bitrate_kbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bitrate_kbps: Option<u32>,
}

/// A smart-default bitrate envelope for a given video profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoProfile {
    pub min_kbps: u32,
    pub default_kbps: u32,
    pub max_kbps: u32,
}

/// Compute smart-default bitrate envelope from resolution + framerate + codec.
/// Client-side mirror of strata_common::profiles::lookup_profile.
pub fn lookup_profile(
    resolution: Option<&str>,
    framerate: Option<u32>,
    codec: Option<&str>,
) -> VideoProfile {
    let res = resolution.unwrap_or("1920x1080");
    let fps = framerate.unwrap_or(30);
    let codec = codec.unwrap_or("h265");

    let height = res
        .split('x')
        .nth(1)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1080);

    let hfr = fps > 30;

    let (min, default, max) = match (height, hfr) {
        (0..=540, false) => (800, 1500, 3000),
        (0..=540, true) => (1000, 2000, 4000),
        (541..=720, false) => (1500, 3000, 4000),
        (541..=720, true) => (2000, 4000, 6000),
        (721..=1080, false) => (3000, 5000, 6000),
        (721..=1080, true) => (4000, 7000, 10000),
        (1081..=1440, false) => (6000, 10000, 13000),
        (1081..=1440, true) => (8000, 14000, 20000),
        (1441..=2160, false) => (10000, 20000, 30000),
        (1441..=2160, true) => (13000, 27000, 40000),
        _ => (13000, 27000, 40000),
    };

    let scale = if codec == "h264" { 1.5 } else { 1.0 };

    VideoProfile {
        min_kbps: (min as f64 * scale) as u32,
        default_kbps: (default as f64 * scale) as u32,
        max_kbps: (max as f64 * scale) as u32,
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartStreamResponse {
    pub stream_id: String,
    pub state: String,
}

// ── Destinations ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct DestinationSummary {
    pub id: String,
    pub platform: String,
    pub name: String,
    pub url: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateDestinationRequest {
    pub platform: String,
    pub name: String,
    pub url: String,
    pub stream_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateDestinationResponse {
    pub id: String,
}

// ── Dashboard WebSocket Events ──────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DashboardEvent {
    #[serde(rename = "sender.status")]
    SenderStatus {
        sender_id: String,
        online: bool,
        #[serde(default)]
        status: Option<SenderFullStatus>,
    },

    #[serde(rename = "stream.stats")]
    StreamStats {
        stream_id: String,
        #[serde(default)]
        sender_id: String,
        uptime_s: u64,
        encoder_bitrate_kbps: u32,
        #[serde(default)]
        timestamp_ms: u64,
        links: Vec<LinkStats>,
    },

    #[serde(rename = "stream.state")]
    StreamStateChanged {
        stream_id: String,
        sender_id: String,
        state: String,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct LinkStats {
    pub id: u32,
    pub interface: String,
    pub state: String,
    pub rtt_ms: f64,
    pub loss_rate: f64,
    pub capacity_bps: u64,
    pub sent_bytes: u64,
    /// Current observed throughput in bits per second.
    #[serde(default)]
    pub observed_bps: u64,
    pub signal_dbm: Option<i32>,
    /// Link technology kind (e.g. "ethernet", "cellular").
    #[serde(default)]
    pub link_kind: Option<String>,
}

// ── Stream Config Update (Hot Reconfig) ─────────────────────────────

/// Request body for `POST /api/senders/:id/stream/config`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StreamConfigUpdateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder: Option<EncoderConfigUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduler: Option<serde_json::Value>,
}

/// Partial encoder update — only set fields are applied.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EncoderConfigUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tune: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyint_max: Option<u32>,
}

/// Source switch request — sent to POST /api/senders/:id/source.
#[derive(Debug, Clone, Serialize)]
pub struct SourceSwitchRequest {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

// ── API Error Response ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorResponse {
    pub error: String,
}
// ── File Browser ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileBrowserResponse {
    pub path: String,
    pub entries: Vec<FileEntry>,
    pub error: Option<String>,
}
