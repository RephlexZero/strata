//! REST API request/response types shared by the control plane (which
//! serves them) and the dashboard (which consumes them).
//!
//! Types the dashboard never touches (e.g. the receivers admin API) stay
//! local to `strata-control` — this module only holds shapes that cross
//! the server/browser boundary, so a field change breaks both sides at
//! compile time instead of silently breaking the UI.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{MediaInput, NetworkInterface, StreamState};

// ── Auth ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub user_id: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub user_id: String,
    pub role: String,
}

/// Error body returned by every failing endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorResponse {
    pub error: String,
}

// ── Senders ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderSummary {
    pub id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub online: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderDetail {
    pub id: String,
    pub owner_id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub enrolled: bool,
    pub online: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSenderRequest {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSenderResponse {
    pub sender_id: String,
    pub enrollment_token: String,
}

/// Response of `GET /api/senders/:id/status` — the last cached
/// `device.status` heartbeat plus identity/online flags. Everything is
/// optional because a sender that has never sent a heartbeat has no
/// cached status.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SenderFullStatus {
    pub sender_id: Option<String>,
    pub online: Option<bool>,
    pub network_interfaces: Option<Vec<NetworkInterface>>,
    pub media_inputs: Option<Vec<MediaInput>>,
    pub stream_state: Option<StreamState>,
    pub cpu_percent: Option<f32>,
    pub mem_used_mb: Option<u32>,
    pub uptime_s: Option<u64>,
    pub receiver_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnenrollResponse {
    pub sender_id: String,
    pub enrollment_token: String,
    pub message: String,
}

// ── Streams ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSummary {
    pub id: String,
    pub sender_id: String,
    pub state: String,
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// Machine-readable end cause ("pipeline_crash", "control_plane_stop",
    /// "reconciled", …). None while active or from older control planes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Stream this one replaced (stop→start within the lineage window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarted_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamDetail {
    pub id: String,
    pub sender_id: String,
    pub destination_id: Option<String>,
    pub state: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub config_json: Option<String>,
    pub total_bytes: i64,
    pub error_message: Option<String>,
    /// Machine-readable end cause ("pipeline_crash", "control_plane_stop",
    /// "reconciled", …). None while active or from older control planes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// Stream this one replaced (stop→start within the lineage window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarted_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartStreamRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::SourceConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<crate::EncoderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartStreamResponse {
    pub stream_id: String,
    pub state: String,
}

// ── Destinations ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestinationSummary {
    pub id: String,
    pub platform: String,
    pub name: String,
    pub url: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDestinationRequest {
    pub platform: String,
    pub name: String,
    pub url: String,
    pub stream_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDestinationResponse {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateDestinationRequest {
    pub name: Option<String>,
    pub url: Option<String>,
    pub stream_key: Option<String>,
}

// ── Alerting ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub metric: String,
    pub condition: String,
    pub threshold: f64,
    #[serde(default)]
    pub enabled: bool,
}
