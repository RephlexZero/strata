//! Daemon lifecycle tests — drive the real `strata-sender` binary against a
//! fake control-plane WebSocket server speaking the strata-protocol envelope
//! contract (the inverse of strata-control's api_integration suite, which
//! fakes devices against a real server).
//!
//! The pipeline child is a fake shell script (same pattern as the
//! `PipelineManager` unit tests), injected via `STRATA_PIPELINE_BIN`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use strata_common::identity::DeviceIdentity;
use strata_protocol::models::StreamState;
use strata_protocol::{
    AgentMessage, AuthChallengePayload, AuthLoginPayload, AuthLoginResponsePayload,
    ConfigUpdatePayload, ControlMessage, EncoderConfig, EncoderConfigUpdate, Envelope,
    SourceConfig, StreamEndReason, StreamStartPayload,
};

/// The daemon binds fixed host resources (stats UDP 127.0.0.1:9100, the
/// pipeline control socket below) — run one daemon at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// Mirrors `pipeline::CONTROL_SOCK_PATH` in the daemon.
const CONTROL_SOCK_PATH: &str = "/tmp/strata-pipeline.sock";

const RECV_TIMEOUT: Duration = Duration::from_secs(30);

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn lock_serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

// ── Test scaffolding ─────────────────────────────────────────────────

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(tag: &str) -> Self {
        let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "strata-sender-lifecycle-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct FakePipelineScript {
    script: PathBuf,
    marker: PathBuf,
    pidfile: PathBuf,
}

impl FakePipelineScript {
    fn new(dir: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("fake-pipeline.sh");
        let marker = dir.join("events.log");
        let pidfile = dir.join("pid");
        let body = format!(
            "#!/usr/bin/env bash\nset -eu\nmarker='{marker}'\npidfile='{pidfile}'\necho $$ > \"$pidfile\"\necho started >> \"$marker\"\ntrap 'echo sigint >> \"$marker\"; exit 0' INT\nwhile :; do\n  read -r -t 1 _ || sleep 0.2\ndone\n",
            marker = marker.display(),
            pidfile = pidfile.display(),
        );
        std::fs::write(&script, body).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        Self {
            script,
            marker,
            pidfile,
        }
    }

    async fn wait_started(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let started = std::fs::read_to_string(&self.marker)
                .map(|s| s.contains("started"))
                .unwrap_or(false);
            let pid_ok = std::fs::read_to_string(&self.pidfile)
                .map(|s| s.trim().parse::<i32>().is_ok())
                .unwrap_or(false);
            if started && pid_ok {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for fake pipeline to start"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn pid(&self) -> i32 {
        std::fs::read_to_string(&self.pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

/// The daemon under test, killed when the test ends.
struct Daemon {
    _child: tokio::process::Child,
}

impl Daemon {
    fn spawn(
        control_url: &str,
        identity_file: &Path,
        enrollment_token: Option<&str>,
        pipeline_bin: Option<&Path>,
    ) -> Self {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_strata-sender"));
        cmd.arg("--control-url")
            .arg(control_url)
            .arg("--identity-file")
            .arg(identity_file)
            .arg("--portal-addr")
            .arg("127.0.0.1:0")
            .arg("--heartbeat-interval")
            .arg("1")
            .env("RUST_LOG", "warn")
            .kill_on_drop(true);
        if let Some(token) = enrollment_token {
            cmd.arg("--enrollment-token").arg(token);
        }
        if let Some(bin) = pipeline_bin {
            cmd.env("STRATA_PIPELINE_BIN", bin);
        }
        Self {
            _child: cmd.spawn().expect("failed to spawn strata-sender"),
        }
    }
}

// ── Fake control plane ───────────────────────────────────────────────

struct Conn {
    ws: WebSocketStream<TcpStream>,
}

impl Conn {
    async fn accept(listener: &TcpListener) -> Self {
        let (stream, _) = tokio::time::timeout(RECV_TIMEOUT, listener.accept())
            .await
            .expect("timed out waiting for the daemon to connect")
            .unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        Self { ws }
    }

    async fn recv_agent(&mut self) -> AgentMessage {
        loop {
            let msg = tokio::time::timeout(RECV_TIMEOUT, self.ws.next())
                .await
                .expect("timed out waiting for an agent message")
                .expect("daemon closed the connection")
                .expect("websocket error");
            match msg {
                Message::Text(text) => {
                    let envelope: Envelope =
                        serde_json::from_str(&text).expect("invalid envelope from daemon");
                    return envelope.parse_message().unwrap_or_else(|e| {
                        panic!("unparseable agent message {}: {e}", envelope.msg_type)
                    });
                }
                Message::Close(_) => panic!("daemon closed the connection"),
                _ => continue,
            }
        }
    }

    async fn send_control(&mut self, msg: &ControlMessage) {
        let envelope = Envelope::from_message(msg).unwrap();
        self.ws
            .send(Message::Text(
                serde_json::to_string(&envelope).unwrap().into(),
            ))
            .await
            .unwrap();
    }

    /// Skip unrelated traffic (heartbeats, stats) until `f` matches.
    async fn wait_for<T>(&mut self, what: &str, f: impl Fn(AgentMessage) -> Option<T>) -> T {
        let deadline = tokio::time::Instant::now() + RECV_TIMEOUT;
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            if let Some(v) = f(self.recv_agent().await) {
                return v;
            }
        }
    }

    async fn expect_auth_login(&mut self) -> AuthLoginPayload {
        match self.recv_agent().await {
            AgentMessage::AuthLogin(p) => p,
            other => panic!("expected auth.login as the first message, got {other:?}"),
        }
    }

    /// Enrollment leg of the server contract: token + public key in,
    /// auth.login.response out. Returns the public key the daemon enrolled.
    async fn enroll(&mut self, expected_token: &str, sender_id: &str) -> String {
        let login = self.expect_auth_login().await;
        assert_eq!(login.enrollment_token.as_deref(), Some(expected_token));
        assert_eq!(login.device_id, None, "fresh device must not claim an id");
        let public_key = login
            .device_public_key
            .expect("enrollment must carry the device public key");

        self.send_control(&ControlMessage::AuthLoginResponse(
            AuthLoginResponsePayload {
                success: true,
                sender_id: Some(sender_id.to_string()),
                error: None,
            },
        ))
        .await;
        public_key
    }

    /// Reconnect leg: device_id in, ed25519 challenge out, signature
    /// verified against the enrolled public key.
    async fn challenge_auth(&mut self, device_id: &str, public_key: &str) {
        let login = self.expect_auth_login().await;
        assert_eq!(login.device_id.as_deref(), Some(device_id));
        assert_eq!(
            login.enrollment_token, None,
            "enrolled device must not resend a token"
        );

        let challenge = strata_common::auth::generate_challenge();
        self.send_control(&ControlMessage::AuthChallenge(AuthChallengePayload {
            challenge: challenge.clone(),
        }))
        .await;

        let resp = match self.recv_agent().await {
            AgentMessage::AuthChallengeResponse(p) => p,
            other => panic!("expected auth.challenge.response, got {other:?}"),
        };
        assert_eq!(resp.device_id, device_id);
        assert!(
            strata_common::auth::verify_challenge(public_key, &challenge, &resp.signature).unwrap(),
            "challenge signature must verify against the enrolled public key"
        );

        self.send_control(&ControlMessage::AuthLoginResponse(
            AuthLoginResponsePayload {
                success: true,
                sender_id: Some(device_id.to_string()),
                error: None,
            },
        ))
        .await;
    }
}

fn stream_start(stream_id: &str) -> ControlMessage {
    ControlMessage::StreamStart(Box::new(StreamStartPayload {
        stream_id: stream_id.into(),
        source: SourceConfig {
            mode: "test".into(),
            device: None,
            uri: None,
            resolution: None,
            framerate: None,
            passthrough: None,
        },
        encoder: EncoderConfig {
            bitrate_kbps: 1_000,
            tune: None,
            keyint_max: None,
            codec: None,
            min_bitrate_kbps: None,
            max_bitrate_kbps: None,
        },
        destinations: Vec::new(),
        bonding_config: serde_json::Value::Null,
        psk: None,
        relay_url: None,
    }))
}

fn config_update(request_id: &str, bitrate_kbps: u32, tune: Option<&str>) -> ControlMessage {
    ControlMessage::ConfigUpdate(ConfigUpdatePayload {
        request_id: Some(request_id.into()),
        scheduler: None,
        encoder: Some(EncoderConfigUpdate {
            bitrate_kbps: Some(bitrate_kbps),
            tune: tune.map(Into::into),
            keyint_max: None,
        }),
    })
}

// ── Tests ────────────────────────────────────────────────────────────

/// Enroll flow: `--enrollment-token` → auth.login with token + public key →
/// success response persists the assigned device id into the identity file,
/// and the daemon enters its heartbeat loop.
#[tokio::test]
async fn enroll_persists_identity_and_starts_heartbeating() {
    let _serial = lock_serial();
    let dir = TestDir::new("enroll");
    let identity_path = dir.path.join("identity.json");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/agent/ws", listener.local_addr().unwrap());
    let _daemon = Daemon::spawn(&url, &identity_path, Some("snd_e2e.SECRET42"), None);

    let mut conn = Conn::accept(&listener).await;
    let public_key = conn.enroll("snd_e2e.SECRET42", "snd_e2e").await;

    // The first heartbeat proves the daemon accepted the login response and
    // entered its main loop; identity persistence happens before that.
    let status = conn
        .wait_for("device.status heartbeat", |m| match m {
            AgentMessage::DeviceStatus(p) => Some(p),
            _ => None,
        })
        .await;
    assert!(matches!(status.stream_state, StreamState::Idle));
    assert!(status.running_streams.is_empty());

    let identity = DeviceIdentity::load_or_generate(&identity_path).unwrap();
    assert_eq!(identity.device_id.as_deref(), Some("snd_e2e"));
    assert_eq!(
        identity.public_key, public_key,
        "persisted keypair must be the one sent at enrollment"
    );
}

/// Reconnect flow: with a persisted identity (no token), the daemon answers
/// the server's ed25519 challenge with a valid signature and resumes.
#[tokio::test]
async fn reconnect_answers_ed25519_challenge() {
    let _serial = lock_serial();
    let dir = TestDir::new("reconnect");
    let identity_path = dir.path.join("identity.json");

    let (private_key, public_key) = strata_common::auth::generate_device_keypair();
    DeviceIdentity {
        device_id: Some("snd_reconnect".into()),
        private_key,
        public_key: public_key.clone(),
    }
    .save(&identity_path)
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/agent/ws", listener.local_addr().unwrap());
    let _daemon = Daemon::spawn(&url, &identity_path, None, None);

    let mut conn = Conn::accept(&listener).await;
    conn.challenge_auth("snd_reconnect", &public_key).await;

    // Post-auth heartbeat proves the handshake fully completed.
    conn.wait_for("device.status heartbeat", |m| match m {
        AgentMessage::DeviceStatus(p) => Some(p),
        _ => None,
    })
    .await;
}

/// stream.start spawns the pipeline child and heartbeats/stats report it
/// live; killing the child yields stream.ended(pipeline_crash) and the
/// heartbeat drops the stream.
#[tokio::test]
async fn stream_start_runs_pipeline_and_crash_is_reported() {
    let _serial = lock_serial();
    let dir = TestDir::new("crash");
    let identity_path = dir.path.join("identity.json");
    let script = FakePipelineScript::new(&dir.path);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/agent/ws", listener.local_addr().unwrap());
    let _daemon = Daemon::spawn(
        &url,
        &identity_path,
        Some("snd_crash.SECRET42"),
        Some(&script.script),
    );

    let mut conn = Conn::accept(&listener).await;
    conn.enroll("snd_crash.SECRET42", "snd_crash").await;

    conn.send_control(&stream_start("str_lifecycle")).await;
    script.wait_started().await;

    // Heartbeat reflects the running stream…
    let status = conn
        .wait_for("live device.status", |m| match m {
            AgentMessage::DeviceStatus(p) if p.running_streams == ["str_lifecycle"] => Some(p),
            _ => None,
        })
        .await;
    assert!(matches!(status.stream_state, StreamState::Live));

    // …and telemetry emits stream.stats for it.
    conn.wait_for("stream.stats", |m| match m {
        AgentMessage::StreamStats(p) if p.stream_id == "str_lifecycle" => Some(()),
        _ => None,
    })
    .await;

    // Kill the pipeline child out from under the daemon.
    let pid = script.pid();
    // SAFETY: pid is the fake pipeline script this test spawned via the daemon.
    unsafe {
        assert_eq!(libc::kill(pid, libc::SIGKILL), 0);
    }

    let ended = conn
        .wait_for("stream.ended", |m| match m {
            AgentMessage::StreamEnded(p) => Some(p),
            _ => None,
        })
        .await;
    assert_eq!(ended.stream_id, "str_lifecycle");
    assert_eq!(ended.reason, StreamEndReason::PipelineCrash);

    // Heartbeat running_streams reflects reality again.
    let status = conn
        .wait_for("idle device.status", |m| match m {
            AgentMessage::DeviceStatus(p) if p.running_streams.is_empty() => Some(p),
            _ => None,
        })
        .await;
    assert!(matches!(status.stream_state, StreamState::Idle));
}

/// config.update with encoder fields is forwarded as a `set_encoder` command
/// on the pipeline control socket — and reported as a failure when the
/// socket isn't there.
#[tokio::test]
async fn config_update_forwards_encoder_or_reports_failure() {
    let _serial = lock_serial();
    let dir = TestDir::new("config");
    let identity_path = dir.path.join("identity.json");
    let script = FakePipelineScript::new(&dir.path);

    // Stale socket from an earlier crashed run would break the bind below.
    let _ = std::fs::remove_file(CONTROL_SOCK_PATH);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/agent/ws", listener.local_addr().unwrap());
    let _daemon = Daemon::spawn(
        &url,
        &identity_path,
        Some("snd_cfg.SECRET42"),
        Some(&script.script),
    );

    let mut conn = Conn::accept(&listener).await;
    conn.enroll("snd_cfg.SECRET42", "snd_cfg").await;

    // A pipeline must be running for the daemon to attempt the socket.
    conn.send_control(&stream_start("str_cfg")).await;
    script.wait_started().await;
    conn.wait_for("live device.status", |m| match m {
        AgentMessage::DeviceStatus(p) if p.running_streams == ["str_cfg"] => Some(()),
        _ => None,
    })
    .await;

    // No control socket listening → the daemon must report failure honestly.
    conn.send_control(&config_update("cfg-fail", 1_500, None))
        .await;
    let resp = conn
        .wait_for("config.update.response (failure)", |m| match m {
            AgentMessage::ConfigUpdateResponse(p)
                if p.request_id.as_deref() == Some("cfg-fail") =>
            {
                Some(p)
            }
            _ => None,
        })
        .await;
    assert!(!resp.success);
    assert!(resp.error.unwrap().contains("encoder"));

    // Now stand in for strata-pipeline's control socket.
    let sock_listener = tokio::net::UnixListener::bind(CONTROL_SOCK_PATH).unwrap();
    conn.send_control(&config_update("cfg-ok", 2_500, Some("zerolatency")))
        .await;

    let (mut sock, _) = tokio::time::timeout(RECV_TIMEOUT, sock_listener.accept())
        .await
        .expect("timed out waiting for the control-socket command")
        .unwrap();
    let mut raw = String::new();
    tokio::time::timeout(RECV_TIMEOUT, sock.read_to_string(&mut raw))
        .await
        .expect("timed out reading the control-socket command")
        .unwrap();
    let cmd: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(cmd["cmd"], "set_encoder");
    assert_eq!(cmd["bitrate_kbps"], 2_500);
    assert_eq!(cmd["tune"], "zerolatency");

    let resp = conn
        .wait_for("config.update.response (success)", |m| match m {
            AgentMessage::ConfigUpdateResponse(p) if p.request_id.as_deref() == Some("cfg-ok") => {
                Some(p)
            }
            _ => None,
        })
        .await;
    assert!(resp.success, "error: {:?}", resp.error);

    drop(sock_listener);
    let _ = std::fs::remove_file(CONTROL_SOCK_PATH);
}
