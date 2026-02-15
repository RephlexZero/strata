//! API response types for the sender portal.
//!
//! Mirrors the agent's REST API response types for WASM deserialization.

use serde::{Deserialize, Serialize};

// ── Status ──────────────────────────────────────────────────────────

/// Full device status from GET /api/status.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceStatus {
    pub sender_id: Option<String>,
    pub enrolled: bool,
    pub cloud_connected: bool,
    pub simulate: bool,
    pub streaming: bool,
    pub stream_id: Option<String>,
    pub uptime_s: u64,
    pub cpu_percent: f32,
    pub mem_used_mb: u32,
    pub interfaces: Vec<NetworkInterface>,
    pub inputs: Vec<MediaInput>,
    pub receiver_url: Option<String>,
}

/// Network interface.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    #[serde(rename = "type")]
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
    #[serde(rename = "type")]
    pub input_type: String,
    pub label: String,
    pub capabilities: Vec<String>,
    pub status: String,
}

// ── Enrollment ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct EnrollRequest {
    pub enrollment_token: String,
    pub control_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnrollResponse {
    pub status: String,
    pub message: Option<String>,
    pub sender_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnenrollResponse {
    pub status: String,
    pub message: Option<String>,
}

// ── Config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigResponse {
    pub control_url: Option<String>,
    pub receiver_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigUpdate {
    pub receiver_url: Option<String>,
    pub control_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigSaveResponse {
    pub status: String,
    pub receiver_url: Option<String>,
    pub control_url: Option<String>,
}

// ── Interface management ────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct InterfaceToggleResponse {
    pub interface: String,
    pub enabled: bool,
    pub success: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InterfaceScanResponse {
    pub discovered: Vec<String>,
    pub total: usize,
    pub interfaces: Vec<NetworkInterface>,
}

// ── Connectivity test ───────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct TestResult {
    pub cloud_reachable: bool,
    pub cloud_connected: bool,
    pub receiver_reachable: bool,
    pub receiver_url: Option<String>,
    pub enrolled: bool,
    pub sender_id: Option<String>,
    pub control_url: Option<String>,
}
