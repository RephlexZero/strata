//! Data models for the Strata platform.
//!
//! These types represent the database entities and are shared between the
//! control plane (which writes them) and the agent (which receives subsets
//! of them over the WebSocket protocol).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── User ────────────────────────────────────────────────────────────

/// A platform user (admin, operator, or viewer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: UserRole,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    Admin,
    Operator,
    Viewer,
}

impl std::fmt::Display for UserRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserRole::Admin => write!(f, "admin"),
            UserRole::Operator => write!(f, "operator"),
            UserRole::Viewer => write!(f, "viewer"),
        }
    }
}

impl std::str::FromStr for UserRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "admin" => Ok(UserRole::Admin),
            "operator" => Ok(UserRole::Operator),
            "viewer" => Ok(UserRole::Viewer),
            other => Err(format!("unknown role: {other}")),
        }
    }
}

// ── Sender (Device) ─────────────────────────────────────────────────

/// A registered sender device (field unit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sender {
    pub id: String,
    pub owner_id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub device_public_key: Option<String>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Live status of a sender device, reported over WSS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderStatus {
    pub network_interfaces: Vec<NetworkInterface>,
    pub media_inputs: Vec<MediaInput>,
    pub stream_state: StreamState,
    pub cpu_percent: f32,
    pub mem_used_mb: u32,
    pub uptime_s: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    #[serde(rename = "type")]
    pub iface_type: InterfaceType,
    pub state: InterfaceState,
    /// Whether this interface is administratively enabled (user can toggle).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceType {
    Cellular,
    Ethernet,
    Wifi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceState {
    Connected,
    Disconnected,
    Connecting,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInput {
    pub device: String,
    #[serde(rename = "type")]
    pub input_type: MediaInputType,
    pub label: String,
    pub capabilities: Vec<String>,
    pub status: MediaInputStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaInputType {
    V4l2,
    File,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaInputStatus {
    Available,
    InUse,
    Error,
}

// ── Stream ──────────────────────────────────────────────────────────

/// An active or historical broadcast stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub id: String,
    pub sender_id: String,
    pub destination_id: Option<String>,
    pub state: StreamState,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub config_json: Option<String>,
    pub total_bytes: i64,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamState {
    Idle,
    Starting,
    Live,
    Stopping,
    Ended,
    Failed,
}

impl std::fmt::Display for StreamState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamState::Idle => write!(f, "idle"),
            StreamState::Starting => write!(f, "starting"),
            StreamState::Live => write!(f, "live"),
            StreamState::Stopping => write!(f, "stopping"),
            StreamState::Ended => write!(f, "ended"),
            StreamState::Failed => write!(f, "failed"),
        }
    }
}

// ── Destination ─────────────────────────────────────────────────────

/// A configured streaming destination (YouTube, Twitch, SRT endpoint, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    pub id: String,
    pub owner_id: String,
    pub platform: DestinationPlatform,
    pub name: String,
    pub url: String,
    /// Stream key — only present when the caller has permission to see it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_key: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationPlatform {
    Youtube,
    Twitch,
    CustomRtmp,
    Srt,
}

impl std::fmt::Display for DestinationPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DestinationPlatform::Youtube => write!(f, "youtube"),
            DestinationPlatform::Twitch => write!(f, "twitch"),
            DestinationPlatform::CustomRtmp => write!(f, "custom_rtmp"),
            DestinationPlatform::Srt => write!(f, "srt"),
        }
    }
}

// ── Link Stats ──────────────────────────────────────────────────────

/// Per-link statistics from the bonding engine, sent in `stream.stats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkStats {
    pub id: u32,
    pub interface: String,
    pub state: String,
    pub rtt_ms: f64,
    pub loss_rate: f64,
    pub capacity_bps: u64,
    pub sent_bytes: u64,
    pub signal_dbm: Option<i32>,
}
