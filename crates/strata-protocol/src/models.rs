//! Data models for the Strata platform.
//!
//! These types represent the database entities and are shared between the
//! control plane (which writes them) and the agent (which receives subsets
//! of them over the WebSocket protocol).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── User ────────────────────────────────────────────────────────────

/// A platform user (operator or viewer).
///
/// There is no admin role — every user owns exactly their own data,
/// isolated by `owner_id` on every table.  No user can see or modify
/// another user's resources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    #[serde(skip)]
    pub password_hash: String,
    pub role: UserRole,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    Operator,
    Viewer,
}

impl std::fmt::Display for UserRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserRole::Operator => write!(f, "operator"),
            UserRole::Viewer => write!(f, "viewer"),
        }
    }
}

impl std::str::FromStr for UserRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
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
    pub cell_id: Option<String>,
    pub band: Option<String>,
    pub data_cap_mb: Option<u64>,
    pub data_used_mb: Option<u64>,
    /// Link priority (1 = highest, 100 = lowest). Default is 1.
    #[serde(default = "default_priority")]
    pub priority: u32,
    pub apn: Option<String>,
    #[serde(skip_serializing)]
    pub sim_pin: Option<String>,
    #[serde(default)]
    pub roaming: bool,
}

fn default_true() -> bool {
    true
}

fn default_priority() -> u32 {
    1
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

// ── Transport Stats ─────────────────────────────────────────────────

/// Sender-side transport protocol statistics, suitable for Prometheus export.
///
/// Mirrors `strata_transport::stats::SenderStats` but lives in strata-common
/// so the metrics renderer doesn't need to depend on strata-transport.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportSenderMetrics {
    /// Total packets sent (including retransmissions).
    pub packets_sent: u64,
    /// Total original payload bytes sent.
    pub bytes_sent: u64,
    /// Packets acknowledged by receiver.
    pub packets_acked: u64,
    /// NACK-triggered retransmissions.
    pub retransmissions: u64,
    /// Packets expired from send buffer without ACK.
    pub packets_expired: u64,
    /// FEC repair packets sent.
    pub fec_repairs_sent: u64,
    /// Last measured RTT in microseconds.
    pub last_rtt_us: u64,
}

/// Receiver-side transport protocol statistics, suitable for Prometheus export.
///
/// Mirrors `strata_transport::stats::ReceiverStats` but lives in strata-common
/// so the metrics renderer doesn't need to depend on strata-transport.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportReceiverMetrics {
    /// Total packets received (including duplicates and late).
    pub packets_received: u64,
    /// Total payload bytes received.
    pub bytes_received: u64,
    /// Packets delivered to the application (unique + in-order).
    pub packets_delivered: u64,
    /// Duplicate packets received.
    pub duplicates: u64,
    /// Packets received after playout deadline.
    pub late_packets: u64,
    /// Packets recovered via FEC decoding.
    pub fec_recoveries: u64,
    /// NACKs sent to request retransmission.
    pub nacks_sent: u64,
    /// Highest contiguous sequence number delivered.
    pub highest_delivered_seq: u64,
    /// Current jitter buffer depth in packets.
    pub jitter_buffer_depth: u32,
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
    /// Current observed throughput in bits per second.
    #[serde(default)]
    pub observed_bps: u64,
    pub signal_dbm: Option<i32>,
    pub rsrp: Option<f32>,
    pub rsrq: Option<f32>,
    pub sinr: Option<f32>,
    pub cqi: Option<u8>,
    /// Link technology kind (e.g. "ethernet", "cellular", "wifi").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_kind: Option<String>,
    /// BBRv3 estimated bottleneck bandwidth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btlbw_bps: Option<u64>,
    /// BBRv3 estimated minimum RTT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtprop_ms: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Enum Serde Round-Trips ──────────────────────────────────

    #[test]
    fn user_role_serde_round_trip() {
        for role in [UserRole::Operator, UserRole::Viewer] {
            let json = serde_json::to_string(&role).unwrap();
            let parsed: UserRole = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, role);
        }
    }

    #[test]
    fn user_role_from_str() {
        assert_eq!("operator".parse::<UserRole>().unwrap(), UserRole::Operator);
        assert_eq!("viewer".parse::<UserRole>().unwrap(), UserRole::Viewer);
        assert!("admin".parse::<UserRole>().is_err());
        assert!("invalid".parse::<UserRole>().is_err());
    }

    #[test]
    fn user_role_display() {
        assert_eq!(UserRole::Operator.to_string(), "operator");
        assert_eq!(UserRole::Viewer.to_string(), "viewer");
    }

    #[test]
    fn interface_type_serde_round_trip() {
        for t in [
            InterfaceType::Cellular,
            InterfaceType::Ethernet,
            InterfaceType::Wifi,
        ] {
            let json = serde_json::to_string(&t).unwrap();
            let parsed: InterfaceType = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, t);
        }
    }

    #[test]
    fn interface_state_serde_round_trip() {
        for s in [
            InterfaceState::Connected,
            InterfaceState::Disconnected,
            InterfaceState::Connecting,
            InterfaceState::Error,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let parsed: InterfaceState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, s);
        }
    }

    #[test]
    fn media_input_type_serde_round_trip() {
        for t in [
            MediaInputType::V4l2,
            MediaInputType::File,
            MediaInputType::Test,
        ] {
            let json = serde_json::to_string(&t).unwrap();
            let parsed: MediaInputType = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, t);
        }
    }

    #[test]
    fn media_input_status_serde_round_trip() {
        for s in [
            MediaInputStatus::Available,
            MediaInputStatus::InUse,
            MediaInputStatus::Error,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let parsed: MediaInputStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, s);
        }
    }

    #[test]
    fn stream_state_serde_round_trip() {
        for s in [
            StreamState::Idle,
            StreamState::Starting,
            StreamState::Live,
            StreamState::Stopping,
            StreamState::Ended,
            StreamState::Failed,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let parsed: StreamState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, s);
            // Also test Display
            let display = s.to_string();
            assert!(!display.is_empty());
        }
    }

    #[test]
    fn destination_platform_serde_round_trip() {
        for p in [
            DestinationPlatform::Youtube,
            DestinationPlatform::Twitch,
            DestinationPlatform::CustomRtmp,
            DestinationPlatform::Srt,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let parsed: DestinationPlatform = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, p);
            assert_eq!(p.to_string(), json.trim_matches('"'));
        }
    }

    // ── Struct Serde Round-Trips ────────────────────────────────

    #[test]
    fn network_interface_serde() {
        let iface = NetworkInterface {
            name: "wwan0".into(),
            iface_type: InterfaceType::Cellular,
            state: InterfaceState::Connected,
            enabled: true,
            ip: Some("10.0.0.1".into()),
            carrier: Some("T-Mobile".into()),
            signal_dbm: Some(-67),
            technology: Some("5G".into()),
            cell_id: None,
            band: None,
            data_cap_mb: None,
            data_used_mb: None,
            priority: 1,
            apn: None,
            sim_pin: None,
            roaming: false,
        };
        let json = serde_json::to_string(&iface).unwrap();
        let parsed: NetworkInterface = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "wwan0");
        assert_eq!(parsed.iface_type, InterfaceType::Cellular);
        assert_eq!(parsed.state, InterfaceState::Connected);
        assert!(parsed.enabled);
        assert_eq!(parsed.signal_dbm, Some(-67));
    }

    #[test]
    fn network_interface_enabled_default() {
        // Verify `enabled` defaults to true when missing from JSON
        let json = r#"{"name":"eth0","type":"ethernet","state":"connected"}"#;
        let parsed: NetworkInterface = serde_json::from_str(json).unwrap();
        assert!(parsed.enabled, "enabled should default to true");
    }

    #[test]
    fn media_input_serde() {
        let input = MediaInput {
            device: "/dev/video0".into(),
            input_type: MediaInputType::V4l2,
            label: "USB Camera".into(),
            capabilities: vec!["1080p30".into(), "720p60".into()],
            status: MediaInputStatus::Available,
        };
        let json = serde_json::to_string(&input).unwrap();
        let parsed: MediaInput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.device, "/dev/video0");
        assert_eq!(parsed.capabilities.len(), 2);
    }

    #[test]
    fn link_stats_serde() {
        let stats = LinkStats {
            id: 1,
            interface: "wwan0".into(),
            state: "connected".into(),
            rtt_ms: 23.5,
            loss_rate: 0.01,
            capacity_bps: 15_000_000,
            sent_bytes: 1_048_576,
            observed_bps: 8_000_000,
            signal_dbm: Some(-72),
            link_kind: Some("cellular".into()),
            rsrp: None,
            rsrq: None,
            sinr: None,
            cqi: None,
            btlbw_bps: Some(12_000_000),
            rtprop_ms: Some(20.0),
        };
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: LinkStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 1);
        assert!((parsed.rtt_ms - 23.5).abs() < f64::EPSILON);
        assert_eq!(parsed.capacity_bps, 15_000_000);
        assert_eq!(parsed.observed_bps, 8_000_000);
        assert_eq!(parsed.link_kind.as_deref(), Some("cellular"));
        assert_eq!(parsed.btlbw_bps, Some(12_000_000));
        assert_eq!(parsed.rtprop_ms, Some(20.0));
    }

    #[test]
    fn link_stats_backward_compat() {
        // Old JSON without observed_bps or link_kind should still parse
        let json = r#"{"id":0,"interface":"eth0","state":"Live","rtt_ms":10.0,"loss_rate":0.0,"capacity_bps":10000000,"sent_bytes":0,"signal_dbm":null}"#;
        let parsed: LinkStats = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.observed_bps, 0);
        assert!(parsed.link_kind.is_none());
    }

    #[test]
    fn transport_sender_metrics_serde() {
        let stats = TransportSenderMetrics {
            packets_sent: 50_000,
            bytes_sent: 70_000_000,
            packets_acked: 49_500,
            retransmissions: 100,
            packets_expired: 10,
            fec_repairs_sent: 500,
            last_rtt_us: 30_000,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: TransportSenderMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.packets_sent, 50_000);
        assert_eq!(parsed.fec_repairs_sent, 500);
        assert_eq!(parsed.last_rtt_us, 30_000);
    }

    #[test]
    fn transport_sender_metrics_default() {
        let stats = TransportSenderMetrics::default();
        assert_eq!(stats.packets_sent, 0);
        assert_eq!(stats.retransmissions, 0);
    }

    #[test]
    fn transport_receiver_metrics_serde() {
        let stats = TransportReceiverMetrics {
            packets_received: 60_000,
            bytes_received: 80_000_000,
            packets_delivered: 55_000,
            duplicates: 2_000,
            late_packets: 500,
            fec_recoveries: 300,
            nacks_sent: 150,
            highest_delivered_seq: 54_999,
            jitter_buffer_depth: 8,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: TransportReceiverMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.packets_received, 60_000);
        assert_eq!(parsed.fec_recoveries, 300);
        assert_eq!(parsed.jitter_buffer_depth, 8);
    }

    #[test]
    fn transport_receiver_metrics_default() {
        let stats = TransportReceiverMetrics::default();
        assert_eq!(stats.packets_received, 0);
        assert_eq!(stats.jitter_buffer_depth, 0);
    }

    #[test]
    fn sender_status_serde() {
        let status = SenderStatus {
            network_interfaces: vec![NetworkInterface {
                name: "eth0".into(),
                iface_type: InterfaceType::Ethernet,
                state: InterfaceState::Connected,
                enabled: true,
                ip: Some("192.168.1.100".into()),
                carrier: None,
                signal_dbm: None,
                technology: None,
                cell_id: None,
                band: None,
                data_cap_mb: None,
                data_used_mb: None,
                priority: 1,
                apn: None,
                sim_pin: None,
                roaming: false,
            }],
            media_inputs: vec![],
            stream_state: StreamState::Live,
            cpu_percent: 42.5,
            mem_used_mb: 1024,
            uptime_s: 86400,
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: SenderStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.network_interfaces.len(), 1);
        assert_eq!(parsed.stream_state, StreamState::Live);
        assert!((parsed.cpu_percent - 42.5).abs() < f32::EPSILON);
    }

    #[test]
    fn destination_stream_key_skipped_when_none() {
        let dest = Destination {
            id: "dst_test".into(),
            owner_id: "usr_test".into(),
            platform: DestinationPlatform::Youtube,
            name: "My Channel".into(),
            url: "rtmp://example.com/live".into(),
            stream_key: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&dest).unwrap();
        assert!(
            !json.contains("stream_key"),
            "stream_key should be skipped when None"
        );
    }

    #[test]
    fn destination_stream_key_present_when_some() {
        let dest = Destination {
            id: "dst_test".into(),
            owner_id: "usr_test".into(),
            platform: DestinationPlatform::Twitch,
            name: "My Stream".into(),
            url: "rtmp://live.twitch.tv/app".into(),
            stream_key: Some("live_secret_key".into()),
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&dest).unwrap();
        assert!(json.contains("stream_key"));
        assert!(json.contains("live_secret_key"));
    }

    #[test]
    fn user_password_hash_not_serialized() {
        let user = User {
            id: "usr_test".into(),
            email: "test@test.com".into(),
            password_hash: "secret_hash_value".into(),
            role: UserRole::Operator,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&user).unwrap();
        assert!(
            !json.contains("secret_hash_value"),
            "password_hash should be skipped in serialization"
        );
    }

    #[test]
    fn stream_serde_round_trip() {
        let stream = Stream {
            id: "str_test".into(),
            sender_id: "snd_abc".into(),
            destination_id: Some("dst_xyz".into()),
            state: StreamState::Live,
            started_at: Some(chrono::Utc::now()),
            ended_at: None,
            config_json: Some(r#"{"bitrate":5000}"#.into()),
            total_bytes: 1_000_000,
            error_message: None,
        };
        let json = serde_json::to_string(&stream).unwrap();
        let parsed: Stream = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "str_test");
        assert_eq!(parsed.state, StreamState::Live);
        assert_eq!(parsed.total_bytes, 1_000_000);
    }
}
