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

#[derive(Debug, Clone, Deserialize)]
pub struct SenderStatusResponse {
    pub sender_id: String,
    pub online: bool,
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
    pub destination_id: Option<String>,
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
    SenderStatus { sender_id: String, online: bool },

    #[serde(rename = "stream.stats")]
    StreamStats {
        stream_id: String,
        uptime_s: u64,
        encoder_bitrate_kbps: u32,
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
    pub signal_dbm: Option<i32>,
}

// ── API Error Response ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorResponse {
    pub error: String,
}
