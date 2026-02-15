//! WebSocket protocol messages between sender agent and control plane.
//!
//! All messages are JSON-encoded and follow a common envelope format.
//! See docs/platform/02-control-protocol.md for the full specification.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::{LinkStats, MediaInput, NetworkInterface, StreamState};

// ── Envelope ────────────────────────────────────────────────────────

/// The outer envelope for all WebSocket messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Unique message ID (UUIDv7, time-ordered).
    pub id: String,
    /// Message type (dotted namespace, e.g. "device.status").
    #[serde(rename = "type")]
    pub msg_type: String,
    /// ISO 8601 timestamp.
    pub ts: DateTime<Utc>,
    /// Type-specific payload.
    pub payload: serde_json::Value,
}

impl Envelope {
    /// Create a new envelope with a fresh UUIDv7 and current timestamp.
    pub fn new(msg_type: impl Into<String>, payload: impl Serialize) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            msg_type: msg_type.into(),
            ts: Utc::now(),
            payload: serde_json::to_value(payload).expect("payload serialization"),
        }
    }

    /// Parse the payload into a concrete type.
    pub fn parse_payload<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }
}

// ── Agent → Control Plane ───────────────────────────────────────────

/// All message types the agent can send.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum AgentMessage {
    /// Initial authentication after WebSocket connect.
    #[serde(rename = "auth.login")]
    AuthLogin(AuthLoginPayload),

    /// Periodic heartbeat with hardware status (every 10s or on change).
    #[serde(rename = "device.status")]
    DeviceStatus(DeviceStatusPayload),

    /// Per-second stream telemetry while broadcasting.
    #[serde(rename = "stream.stats")]
    StreamStats(StreamStatsPayload),

    /// Stream has ended (user-initiated or error).
    #[serde(rename = "stream.ended")]
    StreamEnded(StreamEndedPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthLoginPayload {
    pub enrollment_token: Option<String>,
    pub device_key: Option<String>,
    pub agent_version: String,
    pub hostname: String,
    pub arch: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStatsPayload {
    pub stream_id: String,
    pub uptime_s: u64,
    pub encoder_bitrate_kbps: u32,
    pub links: Vec<LinkStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEndedPayload {
    pub stream_id: String,
    pub reason: StreamEndReason,
    pub duration_s: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEndReason {
    UserStop,
    Error,
    ControlPlaneStop,
    AgentShutdown,
    Timeout,
}

// ── Control Plane → Agent ───────────────────────────────────────────

/// All message types the control plane can send to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ControlMessage {
    /// Response to agent auth.
    #[serde(rename = "auth.login.response")]
    AuthLoginResponse(AuthLoginResponsePayload),

    /// Start a broadcast.
    #[serde(rename = "stream.start")]
    StreamStart(StreamStartPayload),

    /// Stop a broadcast.
    #[serde(rename = "stream.stop")]
    StreamStop(StreamStopPayload),

    /// Hot-reload config update.
    #[serde(rename = "config.update")]
    ConfigUpdate(ConfigUpdatePayload),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthLoginResponsePayload {
    pub success: bool,
    pub sender_id: Option<String>,
    pub session_token: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStartPayload {
    pub stream_id: String,
    pub source: SourceConfig,
    pub encoder: EncoderConfig,
    pub destinations: Vec<String>,
    pub bonding_config: serde_json::Value,
    pub rist_psk: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    pub mode: String,
    pub device: Option<String>,
    pub uri: Option<String>,
    pub resolution: Option<String>,
    pub framerate: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub bitrate_kbps: u32,
    pub tune: Option<String>,
    pub keyint_max: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStopPayload {
    pub stream_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdatePayload {
    /// Partial config — only fields present are updated.
    pub scheduler: Option<serde_json::Value>,
}

/// Command to manage a network interface on the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceCommandPayload {
    /// The interface name (e.g. "wwan0").
    pub interface: String,
    /// Action: "enable", "disable".
    pub action: String,
}

/// Response to an interface command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceCommandResponsePayload {
    pub success: bool,
    pub interface: String,
    pub action: String,
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

// ── Dashboard WebSocket Events ──────────────────────────────────────

/// Events pushed to dashboard WebSocket subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DashboardEvent {
    /// Sender came online or went offline.
    #[serde(rename = "sender.status")]
    SenderStatus {
        sender_id: String,
        online: bool,
        status: Option<DeviceStatusPayload>,
    },

    /// Live stream stats update.
    #[serde(rename = "stream.stats")]
    StreamStats(StreamStatsPayload),

    /// Stream state changed (started, stopped, failed).
    #[serde(rename = "stream.state")]
    StreamStateChanged {
        stream_id: String,
        sender_id: String,
        state: StreamState,
        error: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let payload = AuthLoginPayload {
            enrollment_token: Some("enr_test123".into()),
            device_key: None,
            agent_version: "0.5.0".into(),
            hostname: "test-sender".into(),
            arch: "x86_64".into(),
        };

        let envelope = Envelope::new("auth.login", &payload);
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: Envelope = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.msg_type, "auth.login");
        let recovered: AuthLoginPayload = parsed.parse_payload().unwrap();
        assert_eq!(recovered.hostname, "test-sender");
    }

    #[test]
    fn envelope_has_valid_uuid_and_timestamp() {
        let envelope = Envelope::new("test", serde_json::json!({}));
        // UUIDv7 is 36 chars with dashes
        assert_eq!(envelope.id.len(), 36);
        assert!(envelope.id.contains('-'));
        // Timestamp should be recent
        let age = chrono::Utc::now() - envelope.ts;
        assert!(age.num_seconds() < 5);
    }

    #[test]
    fn agent_message_tagged_serialization() {
        let msg = AgentMessage::AuthLogin(AuthLoginPayload {
            enrollment_token: Some("enr_abc".into()),
            device_key: None,
            agent_version: "0.5.0".into(),
            hostname: "sender-1".into(),
            arch: "aarch64".into(),
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("auth.login"));
        assert!(json.contains("sender-1"));

        // Round-trip
        let recovered: AgentMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            AgentMessage::AuthLogin(p) => assert_eq!(p.hostname, "sender-1"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_message_device_status() {
        let msg = AgentMessage::DeviceStatus(DeviceStatusPayload {
            network_interfaces: vec![],
            media_inputs: vec![],
            stream_state: crate::models::StreamState::Idle,
            cpu_percent: 25.0,
            mem_used_mb: 512,
            uptime_s: 3600,
            receiver_url: Some("rist://rcv.example.com:5000".into()),
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("device.status"));

        let recovered: AgentMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            AgentMessage::DeviceStatus(p) => {
                assert!((p.cpu_percent - 25.0).abs() < f32::EPSILON);
                assert_eq!(p.mem_used_mb, 512);
                assert_eq!(p.receiver_url.unwrap(), "rist://rcv.example.com:5000");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_message_stream_stats() {
        let msg = AgentMessage::StreamStats(StreamStatsPayload {
            stream_id: "str_test".into(),
            uptime_s: 120,
            encoder_bitrate_kbps: 5000,
            links: vec![crate::models::LinkStats {
                id: 1,
                interface: "wwan0".into(),
                state: "connected".into(),
                rtt_ms: 15.0,
                loss_rate: 0.005,
                capacity_bps: 10_000_000,
                sent_bytes: 500_000,
                signal_dbm: Some(-65),
            }],
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("stream.stats"));

        let recovered: AgentMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            AgentMessage::StreamStats(p) => {
                assert_eq!(p.stream_id, "str_test");
                assert_eq!(p.links.len(), 1);
                assert_eq!(p.links[0].interface, "wwan0");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_message_stream_ended() {
        let msg = AgentMessage::StreamEnded(StreamEndedPayload {
            stream_id: "str_end".into(),
            reason: StreamEndReason::UserStop,
            duration_s: 600,
            total_bytes: 50_000_000,
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("stream.ended"));

        let recovered: AgentMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            AgentMessage::StreamEnded(p) => {
                assert_eq!(p.reason, StreamEndReason::UserStop);
                assert_eq!(p.total_bytes, 50_000_000);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn stream_end_reason_all_variants() {
        for reason in [
            StreamEndReason::UserStop,
            StreamEndReason::Error,
            StreamEndReason::ControlPlaneStop,
            StreamEndReason::AgentShutdown,
            StreamEndReason::Timeout,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let parsed: StreamEndReason = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, reason);
        }
    }

    #[test]
    fn control_message_tagged_serialization() {
        let msg = ControlMessage::StreamStop(StreamStopPayload {
            stream_id: "str_test123".into(),
            reason: "user_request".into(),
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("stream.stop"));

        let recovered: ControlMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            ControlMessage::StreamStop(p) => assert_eq!(p.stream_id, "str_test123"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn control_message_auth_login_response() {
        let msg = ControlMessage::AuthLoginResponse(AuthLoginResponsePayload {
            success: true,
            sender_id: Some("snd_abc".into()),
            session_token: Some("tok_xyz".into()),
            error: None,
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("auth.login.response"));

        let recovered: ControlMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            ControlMessage::AuthLoginResponse(p) => {
                assert!(p.success);
                assert_eq!(p.sender_id.unwrap(), "snd_abc");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn control_message_stream_start() {
        let msg = ControlMessage::StreamStart(StreamStartPayload {
            stream_id: "str_new".into(),
            source: SourceConfig {
                mode: "test".into(),
                device: None,
                uri: None,
                resolution: Some("1920x1080".into()),
                framerate: Some(30),
            },
            encoder: EncoderConfig {
                bitrate_kbps: 5000,
                tune: Some("zerolatency".into()),
                keyint_max: Some(60),
            },
            destinations: vec!["dst_yt".into()],
            bonding_config: serde_json::json!({"max_links": 4}),
            rist_psk: Some("secret".into()),
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("stream.start"));

        let recovered: ControlMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            ControlMessage::StreamStart(p) => {
                assert_eq!(p.stream_id, "str_new");
                assert_eq!(p.encoder.bitrate_kbps, 5000);
                assert_eq!(p.source.resolution.unwrap(), "1920x1080");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn control_message_config_update() {
        let msg = ControlMessage::ConfigUpdate(ConfigUpdatePayload {
            scheduler: Some(serde_json::json!({"algo": "weighted_round_robin"})),
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("config.update"));

        let recovered: ControlMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            ControlMessage::ConfigUpdate(p) => {
                assert!(p.scheduler.is_some());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn dashboard_event_serialization() {
        let event = DashboardEvent::StreamStateChanged {
            stream_id: "str_abc".into(),
            sender_id: "snd_xyz".into(),
            state: crate::models::StreamState::Live,
            error: None,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("stream.state"));
        assert!(json.contains("live"));
    }

    #[test]
    fn dashboard_event_sender_status() {
        let event = DashboardEvent::SenderStatus {
            sender_id: "snd_test".into(),
            online: true,
            status: Some(DeviceStatusPayload {
                network_interfaces: vec![],
                media_inputs: vec![],
                stream_state: crate::models::StreamState::Idle,
                cpu_percent: 10.0,
                mem_used_mb: 256,
                uptime_s: 7200,
                receiver_url: None,
            }),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("sender.status"));
        assert!(json.contains("snd_test"));

        let recovered: DashboardEvent = serde_json::from_str(&json).unwrap();
        match recovered {
            DashboardEvent::SenderStatus {
                sender_id,
                online,
                status,
            } => {
                assert_eq!(sender_id, "snd_test");
                assert!(online);
                assert!(status.is_some());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn dashboard_event_stream_stats() {
        let event = DashboardEvent::StreamStats(StreamStatsPayload {
            stream_id: "str_live".into(),
            uptime_s: 300,
            encoder_bitrate_kbps: 4500,
            links: vec![],
        });

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("stream.stats"));

        let recovered: DashboardEvent = serde_json::from_str(&json).unwrap();
        match recovered {
            DashboardEvent::StreamStats(p) => {
                assert_eq!(p.stream_id, "str_live");
                assert_eq!(p.encoder_bitrate_kbps, 4500);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn interface_command_payload_serde() {
        let cmd = InterfaceCommandPayload {
            interface: "wwan0".into(),
            action: "enable".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: InterfaceCommandPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.interface, "wwan0");
        assert_eq!(parsed.action, "enable");
    }

    #[test]
    fn interface_command_response_serde() {
        let resp = InterfaceCommandResponsePayload {
            success: true,
            interface: "wwan0".into(),
            action: "disable".into(),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: InterfaceCommandResponsePayload = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert!(parsed.error.is_none());
    }

    #[test]
    fn config_set_payload_serde() {
        let payload = ConfigSetPayload {
            request_id: "req_123".into(),
            receiver_url: Some("rist://recv.example.com:5000".into()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: ConfigSetPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.request_id, "req_123");
        assert_eq!(parsed.receiver_url.unwrap(), "rist://recv.example.com:5000");
    }

    #[test]
    fn config_set_response_serde() {
        let resp = ConfigSetResponsePayload {
            request_id: "req_456".into(),
            success: true,
            receiver_url: Some("rist://192.168.1.50:5000".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ConfigSetResponsePayload = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
    }

    #[test]
    fn test_run_payload_serde() {
        let payload = TestRunPayload {
            request_id: "req_test".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: TestRunPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.request_id, "req_test");
    }

    #[test]
    fn test_run_response_serde() {
        let resp = TestRunResponsePayload {
            request_id: "req_test".into(),
            cloud_reachable: true,
            cloud_connected: true,
            receiver_reachable: false,
            receiver_url: None,
            enrolled: true,
            control_url: Some("wss://control.example.com/agent/ws".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: TestRunResponsePayload = serde_json::from_str(&json).unwrap();
        assert!(parsed.cloud_reachable);
        assert!(!parsed.receiver_reachable);
        assert!(parsed.enrolled);
    }

    #[test]
    fn interfaces_scan_payload_serde() {
        let payload = InterfacesScanPayload {
            request_id: "req_scan".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: InterfacesScanPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.request_id, "req_scan");
    }

    #[test]
    fn interfaces_scan_response_serde() {
        let resp = InterfacesScanResponsePayload {
            request_id: "req_scan".into(),
            discovered: vec!["wwan1".into(), "wwan2".into()],
            total: 4,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: InterfacesScanResponsePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.discovered.len(), 2);
        assert_eq!(parsed.total, 4);
    }

    #[test]
    fn device_status_receiver_url_omitted_when_none() {
        let status = DeviceStatusPayload {
            network_interfaces: vec![],
            media_inputs: vec![],
            stream_state: crate::models::StreamState::Idle,
            cpu_percent: 0.0,
            mem_used_mb: 0,
            uptime_s: 0,
            receiver_url: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("receiver_url"),
            "receiver_url should be omitted when None"
        );
    }
}
