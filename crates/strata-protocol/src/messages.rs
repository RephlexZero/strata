//! Direction enums — the authoritative list of every message type on each
//! WebSocket leg.
//!
//! Hubs and daemons parse an [`crate::Envelope`] into the enum for their leg
//! ([`crate::Envelope::parse_message`]) and match exhaustively; senders build
//! envelopes from enum values ([`crate::Envelope::from_message`]). The serde
//! tag on each variant is the wire `type` string — it exists nowhere else.

use serde::{Deserialize, Serialize};

use crate::payloads::*;

// ── Agent → Control Plane ───────────────────────────────────────────

/// Every message a sender agent can send to the control plane.
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

    // ── RPC responses (routed to the pending REST caller by request_id) ──
    #[serde(rename = "config.set.response")]
    ConfigSetResponse(ConfigSetResponsePayload),
    #[serde(rename = "config.update.response")]
    ConfigUpdateResponse(ConfigUpdateResponsePayload),
    #[serde(rename = "test.run.response")]
    TestRunResponse(TestRunResponsePayload),
    #[serde(rename = "interfaces.scan.response")]
    InterfacesScanResponse(InterfacesScanResponsePayload),
    #[serde(rename = "interface.command.response")]
    InterfaceCommandResponse(InterfaceCommandResponsePayload),
    #[serde(rename = "files.list.response")]
    FilesListResponse(FilesListResponsePayload),
    #[serde(rename = "diagnostics.network.response")]
    NetworkToolResponse(NetworkToolResponsePayload),
    #[serde(rename = "diagnostics.pcap.response")]
    PcapCaptureResponse(PcapCaptureResponsePayload),
    #[serde(rename = "logs.get.response")]
    LogsResponse(LogsResponsePayload),
    #[serde(rename = "power.command.response")]
    PowerCommandResponse(PowerCommandResponsePayload),
    #[serde(rename = "tls.status.response")]
    TlsStatusResponse(TlsStatusResponsePayload),
    #[serde(rename = "tls.renew.response")]
    TlsRenewResponse(TlsRenewResponsePayload),
    #[serde(rename = "config.export.response")]
    ConfigExportResponse(ConfigExportResponsePayload),
    #[serde(rename = "config.import.response")]
    ConfigImportResponse(ConfigImportResponsePayload),
    #[serde(rename = "updates.check.response")]
    UpdatesCheckResponse(UpdatesCheckResponsePayload),
    #[serde(rename = "updates.install.response")]
    UpdatesInstallResponse(UpdatesInstallResponsePayload),
    #[serde(rename = "stream.destinations.response")]
    StreamDestinationsResponse(StreamDestinationsResponsePayload),
    #[serde(rename = "stream.jitter_buffer.response")]
    JitterBufferResponse(JitterBufferResponsePayload),
}

impl AgentMessage {
    /// Request-correlation ID for RPC response messages; `None` for
    /// telemetry/lifecycle messages (and for `interface.command.response`,
    /// which carries none on the wire — its REST endpoint is fire-and-forget).
    pub fn request_id(&self) -> Option<&str> {
        use AgentMessage::*;
        match self {
            AuthLogin(_) | DeviceStatus(_) | StreamStats(_) | StreamEnded(_)
            | InterfaceCommandResponse(_) => None,
            ConfigSetResponse(p) => Some(&p.request_id),
            ConfigUpdateResponse(p) => p.request_id.as_deref(),
            TestRunResponse(p) => Some(&p.request_id),
            InterfacesScanResponse(p) => Some(&p.request_id),
            FilesListResponse(p) => Some(&p.request_id),
            NetworkToolResponse(p) => Some(&p.request_id),
            PcapCaptureResponse(p) => Some(&p.request_id),
            LogsResponse(p) => Some(&p.request_id),
            PowerCommandResponse(p) => Some(&p.request_id),
            TlsStatusResponse(p) => Some(&p.request_id),
            TlsRenewResponse(p) => Some(&p.request_id),
            ConfigExportResponse(p) => Some(&p.request_id),
            ConfigImportResponse(p) => Some(&p.request_id),
            UpdatesCheckResponse(p) => Some(&p.request_id),
            UpdatesInstallResponse(p) => Some(&p.request_id),
            StreamDestinationsResponse(p) => Some(&p.request_id),
            JitterBufferResponse(p) => Some(&p.request_id),
        }
    }
}

// ── Control Plane → Agent ───────────────────────────────────────────

/// Every message the control plane can send to a sender agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ControlMessage {
    /// Response to agent auth.
    #[serde(rename = "auth.login.response")]
    AuthLoginResponse(AuthLoginResponsePayload),

    /// Start a broadcast.
    #[serde(rename = "stream.start")]
    StreamStart(Box<StreamStartPayload>),

    /// Stop a broadcast.
    #[serde(rename = "stream.stop")]
    StreamStop(StreamStopPayload),

    /// Hot-reload config update.
    #[serde(rename = "config.update")]
    ConfigUpdate(ConfigUpdatePayload),

    /// Switch the active video source on a running pipeline.
    #[serde(rename = "source.switch")]
    SourceSwitch(SourceSwitchPayload),

    /// Manage a network interface on the agent.
    #[serde(rename = "interface.command")]
    InterfaceCommand(InterfaceCommandPayload),

    /// Set receiver/config on the agent.
    #[serde(rename = "config.set")]
    ConfigSet(ConfigSetPayload),

    /// Run a connectivity test.
    #[serde(rename = "test.run")]
    TestRun(TestRunPayload),

    /// Scan for new network interfaces.
    #[serde(rename = "interfaces.scan")]
    InterfacesScan(InterfacesScanPayload),

    /// List files in a directory on the agent.
    #[serde(rename = "files.list")]
    FilesList(FilesListPayload),

    /// Run a network diagnostic tool.
    #[serde(rename = "diagnostics.network")]
    NetworkTool(NetworkToolPayload),

    /// Capture network packets.
    #[serde(rename = "diagnostics.pcap")]
    PcapCapture(PcapCapturePayload),

    /// Fetch device logs.
    #[serde(rename = "logs.get")]
    LogsGet(LogsRequestPayload),

    /// Power command (reboot, shutdown, restart_agent).
    #[serde(rename = "power.command")]
    PowerCommand(PowerCommandPayload),

    /// TLS certificate status query.
    #[serde(rename = "tls.status")]
    TlsStatus(TlsStatusPayload),

    /// TLS certificate renewal.
    #[serde(rename = "tls.renew")]
    TlsRenew(TlsRenewPayload),

    /// Export agent configuration.
    #[serde(rename = "config.export")]
    ConfigExport(ConfigExportPayload),

    /// Import agent configuration.
    #[serde(rename = "config.import")]
    ConfigImport(ConfigImportPayload),

    /// Check for OTA updates.
    #[serde(rename = "updates.check")]
    UpdatesCheck(UpdatesCheckPayload),

    /// Install OTA update.
    #[serde(rename = "updates.install")]
    UpdatesInstall(UpdatesInstallPayload),

    /// Set stream destinations (fan-out).
    #[serde(rename = "stream.destinations")]
    StreamDestinations(StreamDestinationsPayload),

    /// Configure receiver jitter buffer.
    #[serde(rename = "stream.jitter_buffer")]
    JitterBuffer(JitterBufferPayload),
}

impl ControlMessage {
    /// Request-correlation ID for RPC command messages; `None` for
    /// lifecycle/fire-and-forget commands.
    pub fn request_id(&self) -> Option<&str> {
        use ControlMessage::*;
        match self {
            AuthLoginResponse(_) | StreamStart(_) | StreamStop(_) | SourceSwitch(_)
            | InterfaceCommand(_) => None,
            ConfigUpdate(p) => p.request_id.as_deref(),
            ConfigSet(p) => Some(&p.request_id),
            TestRun(p) => Some(&p.request_id),
            InterfacesScan(p) => Some(&p.request_id),
            FilesList(p) => Some(&p.request_id),
            NetworkTool(p) => Some(&p.request_id),
            PcapCapture(p) => Some(&p.request_id),
            LogsGet(p) => Some(&p.request_id),
            PowerCommand(p) => Some(&p.request_id),
            TlsStatus(p) => Some(&p.request_id),
            TlsRenew(p) => Some(&p.request_id),
            ConfigExport(p) => Some(&p.request_id),
            ConfigImport(p) => Some(&p.request_id),
            UpdatesCheck(p) => Some(&p.request_id),
            UpdatesInstall(p) => Some(&p.request_id),
            StreamDestinations(p) => Some(&p.request_id),
            JitterBuffer(p) => Some(&p.request_id),
        }
    }
}

// ── Receiver → Control Plane ────────────────────────────────────────

/// Every message a receiver daemon can send to the control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ReceiverMessage {
    /// Initial authentication with capacity registration.
    #[serde(rename = "auth.login")]
    AuthLogin(ReceiverAuthLoginPayload),

    /// Periodic heartbeat with capacity info.
    #[serde(rename = "receiver.status")]
    Status(ReceiverStatusPayload),

    /// Per-second receiver-side stats for a stream.
    #[serde(rename = "receiver.stream.stats")]
    StreamStats(ReceiverStreamStatsPayload),

    /// Receiver reports a stream has ended.
    #[serde(rename = "receiver.stream.ended")]
    StreamEnded(ReceiverStreamEndedPayload),
}

// ── Control Plane → Receiver ────────────────────────────────────────

/// Every message the control plane can send to a receiver daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ReceiverControlMessage {
    /// Response to receiver auth.
    #[serde(rename = "auth.login.response")]
    AuthLoginResponse(ReceiverAuthLoginResponsePayload),

    /// Start receiving a stream.
    #[serde(rename = "receiver.stream.start")]
    StreamStart(ReceiverStreamStartPayload),

    /// Stop a stream.
    #[serde(rename = "receiver.stream.stop")]
    StreamStop(ReceiverStreamStopPayload),
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

    /// Live stream stats update (sender side).
    #[serde(rename = "stream.stats")]
    StreamStats(StreamStatsPayload),

    /// Stream state changed (started, stopped, failed).
    #[serde(rename = "stream.state")]
    StreamStateChanged {
        stream_id: String,
        sender_id: String,
        state: crate::models::StreamState,
        error: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Envelope;

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
        assert_eq!(parsed.proto_version, crate::PROTOCOL_VERSION);
        let recovered: AuthLoginPayload = parsed.parse_payload().unwrap();
        assert_eq!(recovered.hostname, "test-sender");
    }

    #[test]
    fn envelope_without_proto_version_defaults_to_v1() {
        // Peers that predate versioning send no proto_version field.
        let json = r#"{"id":"x","type":"device.status","ts":"2026-01-01T00:00:00Z","payload":{}}"#;
        let parsed: Envelope = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.proto_version, 1);
    }

    #[test]
    fn envelope_from_message_extracts_tag_and_payload() {
        let msg = AgentMessage::StreamEnded(StreamEndedPayload {
            stream_id: "str_end".into(),
            reason: StreamEndReason::UserStop,
            duration_s: 600,
            total_bytes: 50_000_000,
        });
        let envelope = Envelope::from_message(&msg).unwrap();
        assert_eq!(envelope.msg_type, "stream.ended");
        assert_eq!(envelope.proto_version, crate::PROTOCOL_VERSION);

        // And parse_message round-trips it.
        let recovered: AgentMessage = envelope.parse_message().unwrap();
        match recovered {
            AgentMessage::StreamEnded(p) => assert_eq!(p.total_bytes, 50_000_000),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn envelope_parse_message_rejects_unknown_type() {
        let json = r#"{"id":"x","type":"no.such.message","ts":"2026-01-01T00:00:00Z","payload":{}}"#;
        let envelope: Envelope = serde_json::from_str(json).unwrap();
        assert!(envelope.parse_message::<AgentMessage>().is_err());
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
    fn agent_message_rpc_response_request_id() {
        let msg = AgentMessage::TestRunResponse(TestRunResponsePayload {
            request_id: "req_1".into(),
            cloud_reachable: true,
            cloud_connected: true,
            receiver_reachable: false,
            receiver_url: None,
            enrolled: true,
            control_url: None,
        });
        assert_eq!(msg.request_id(), Some("req_1"));

        let msg = AgentMessage::DeviceStatus(DeviceStatusPayload {
            network_interfaces: vec![],
            media_inputs: vec![],
            stream_state: crate::models::StreamState::Idle,
            cpu_percent: 0.0,
            mem_used_mb: 0,
            uptime_s: 0,
            receiver_url: None,
            running_streams: vec![],
        });
        assert_eq!(msg.request_id(), None);
    }

    #[test]
    fn device_status_running_streams_defaults_empty() {
        // Old agents don't send the field.
        let json = r#"{"network_interfaces":[],"media_inputs":[],"stream_state":"idle","cpu_percent":0.0,"mem_used_mb":0,"uptime_s":0}"#;
        let parsed: DeviceStatusPayload = serde_json::from_str(json).unwrap();
        assert!(parsed.running_streams.is_empty());
    }

    #[test]
    fn stream_end_reason_all_variants() {
        for reason in [
            StreamEndReason::UserStop,
            StreamEndReason::Error,
            StreamEndReason::ControlPlaneStop,
            StreamEndReason::AgentShutdown,
            StreamEndReason::Timeout,
            StreamEndReason::PipelineCrash,
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
    fn control_message_stream_start() {
        let msg = ControlMessage::StreamStart(Box::new(StreamStartPayload {
            stream_id: "str_new".into(),
            source: SourceConfig {
                mode: "test".into(),
                device: None,
                uri: None,
                resolution: Some("1920x1080".into()),
                framerate: Some(30),
                passthrough: None,
            },
            encoder: EncoderConfig {
                bitrate_kbps: 5000,
                tune: Some("zerolatency".into()),
                keyint_max: Some(60),
                codec: Some("h265".into()),
                min_bitrate_kbps: Some(1500),
                max_bitrate_kbps: Some(10000),
            },
            destinations: vec!["dst_yt".into()],
            bonding_config: serde_json::json!({"max_links": 4}),
            psk: Some("secret".into()),
            relay_url: None,
        }));

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("stream.start"));

        let recovered: ControlMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            ControlMessage::StreamStart(ref p) => {
                assert_eq!(p.stream_id, "str_new");
                assert_eq!(p.encoder.bitrate_kbps, 5000);
                assert_eq!(p.source.resolution.as_deref(), Some("1920x1080"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn config_update_request_id_round_trips_through_enum() {
        // The REST layer correlates config.update with its response by
        // request_id — the typed payload must carry it (it used to be
        // injected as a raw JSON patch, which typed dispatch would drop).
        let msg = ControlMessage::ConfigUpdate(ConfigUpdatePayload {
            request_id: Some("req_42".into()),
            scheduler: None,
            encoder: Some(EncoderConfigUpdate {
                bitrate_kbps: Some(2000),
                ..Default::default()
            }),
            fec: None,
        });
        let envelope = Envelope::from_message(&msg).unwrap();
        let recovered: ControlMessage = envelope.parse_message().unwrap();
        match recovered {
            ControlMessage::ConfigUpdate(p) => {
                assert_eq!(p.request_id.as_deref(), Some("req_42"))
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn receiver_message_round_trip() {
        let msg = ReceiverMessage::Status(ReceiverStatusPayload {
            active_streams: 1,
            max_streams: 4,
            cpu_percent: 12.5,
            mem_used_mb: 2048,
            uptime_s: 600,
            running_streams: vec!["str_a".into()],
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("receiver.status"));
        let recovered: ReceiverMessage = serde_json::from_str(&json).unwrap();
        match recovered {
            ReceiverMessage::Status(p) => {
                assert_eq!(p.active_streams, 1);
                assert_eq!(p.running_streams, vec!["str_a".to_string()]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn receiver_control_message_round_trip() {
        let msg = ReceiverControlMessage::StreamStart(ReceiverStreamStartPayload {
            stream_id: "str_r".into(),
            bind_ports: vec![5000, 5002],
            relay_url: None,
            bonding_config: serde_json::Value::Null,
        });
        let envelope = Envelope::from_message(&msg).unwrap();
        assert_eq!(envelope.msg_type, "receiver.stream.start");
        let recovered: ReceiverControlMessage = envelope.parse_message().unwrap();
        match recovered {
            ReceiverControlMessage::StreamStart(p) => assert_eq!(p.bind_ports, vec![5000, 5002]),
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
                running_streams: vec![],
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
            sender_id: "snd_xyz".into(),
            uptime_s: 300,
            encoder_bitrate_kbps: 4500,
            timestamp_ms: 1700000000000,
            links: vec![],
            sender_metrics: None,
            receiver_metrics: None,
        });

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("stream.stats"));
        assert!(json.contains("snd_xyz"));

        let recovered: DashboardEvent = serde_json::from_str(&json).unwrap();
        match recovered {
            DashboardEvent::StreamStats(p) => {
                assert_eq!(p.stream_id, "str_live");
                assert_eq!(p.sender_id, "snd_xyz");
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
            band: None,
            priority: None,
            apn: None,
            sim_pin: None,
            roaming: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: InterfaceCommandPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.interface, "wwan0");
        assert_eq!(parsed.action, "enable");
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
            running_streams: vec![],
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("receiver_url"),
            "receiver_url should be omitted when None"
        );
    }
}
