//! Daemon lifecycle tests — drive the real `strata-receiver` binary against a
//! fake control-plane WebSocket server speaking the strata-protocol envelope
//! contract (the receiver-side twin of strata-sender's daemon_lifecycle
//! suite; same harness pattern).
//!
//! The pipeline child is a fake shell script (same pattern as the sender
//! suite), injected via `STRATA_PIPELINE_BIN`.

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use strata_common::identity::DeviceIdentity;
use strata_protocol::{
    AuthChallengePayload, Envelope, ReceiverAuthLoginResponsePayload, ReceiverControlMessage,
    ReceiverMessage, ReceiverStreamStartPayload, ReceiverStreamStopPayload, StreamEndReason,
};

/// The daemon binds fixed host resources (per-stream stats UDP listeners
/// from port 9200 up) — run one daemon at a time.
static SERIAL: Mutex<()> = Mutex::const_new(());

const RECV_TIMEOUT: Duration = Duration::from_secs(30);

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

async fn lock_serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

// ── Test scaffolding ─────────────────────────────────────────────────

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(tag: &str) -> Self {
        let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "strata-receiver-lifecycle-{tag}-{}-{unique}",
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
    argsfile: PathBuf,
}

impl FakePipelineScript {
    fn new(dir: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("fake-pipeline.sh");
        let marker = dir.join("events.log");
        let pidfile = dir.join("pid");
        let argsfile = dir.join("args");
        let body = format!(
            "#!/usr/bin/env bash\nset -eu\nmarker='{marker}'\npidfile='{pidfile}'\nargsfile='{argsfile}'\necho \"$@\" > \"$argsfile\"\necho $$ > \"$pidfile\"\necho started >> \"$marker\"\ntrap 'echo sigint >> \"$marker\"; exit 0' INT\nwhile :; do\n  read -r -t 1 _ || sleep 0.2\ndone\n",
            marker = marker.display(),
            pidfile = pidfile.display(),
            argsfile = argsfile.display(),
        );
        std::fs::write(&script, body).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        Self {
            script,
            marker,
            pidfile,
            argsfile,
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

    /// The argv the daemon spawned the pipeline with (whitespace-joined).
    fn args(&self) -> String {
        std::fs::read_to_string(&self.argsfile).unwrap_or_default()
    }

    /// Extract the value following `--stats-dest` from the spawn argv.
    fn stats_dest(&self) -> String {
        let args = self.args();
        let mut it = args.split_whitespace();
        while let Some(a) = it.next() {
            if a == "--stats-dest" {
                return it.next().expect("--stats-dest without a value").to_string();
            }
        }
        panic!("--stats-dest not in pipeline argv: {args}");
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
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_strata-receiver"));
        cmd.arg("--control-url")
            .arg(control_url)
            .arg("--identity-file")
            .arg(identity_file)
            .arg("--bind-host")
            .arg("127.0.0.1")
            .arg("--link-ports")
            .arg("6000,6002,6004,6006")
            .arg("--max-streams")
            .arg("2")
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
            _child: cmd.spawn().expect("failed to spawn strata-receiver"),
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

    async fn recv_receiver(&mut self) -> ReceiverMessage {
        loop {
            let msg = tokio::time::timeout(RECV_TIMEOUT, self.ws.next())
                .await
                .expect("timed out waiting for a receiver message")
                .expect("daemon closed the connection")
                .expect("websocket error");
            match msg {
                Message::Text(text) => {
                    let envelope: Envelope =
                        serde_json::from_str(&text).expect("invalid envelope from daemon");
                    return envelope.parse_message().unwrap_or_else(|e| {
                        panic!("unparseable receiver message {}: {e}", envelope.msg_type)
                    });
                }
                Message::Close(_) => panic!("daemon closed the connection"),
                _ => continue,
            }
        }
    }

    async fn send_control(&mut self, msg: &ReceiverControlMessage) {
        let envelope = Envelope::from_message(msg).unwrap();
        self.ws
            .send(Message::Text(
                serde_json::to_string(&envelope).unwrap().into(),
            ))
            .await
            .unwrap();
    }

    /// Skip unrelated traffic (heartbeats, stats) until `f` matches.
    async fn wait_for<T>(&mut self, what: &str, f: impl Fn(ReceiverMessage) -> Option<T>) -> T {
        let deadline = tokio::time::Instant::now() + RECV_TIMEOUT;
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            if let Some(v) = f(self.recv_receiver().await) {
                return v;
            }
        }
    }

    /// Enrollment leg of the server contract: token + public key + capacity
    /// registration in, auth.login.response out. Returns the enrolled key.
    async fn enroll(&mut self, expected_token: &str, receiver_id: &str) -> String {
        let login = match self.recv_receiver().await {
            ReceiverMessage::AuthLogin(p) => p,
            other => panic!("expected auth.login as the first message, got {other:?}"),
        };
        assert_eq!(login.enrollment_token.as_deref(), Some(expected_token));
        assert_eq!(login.device_id, None, "fresh device must not claim an id");
        assert_eq!(login.bind_host, "127.0.0.1");
        assert_eq!(login.link_ports, vec![6000, 6002, 6004, 6006]);
        let public_key = login
            .device_public_key
            .expect("enrollment must carry the device public key");

        self.send_control(&ReceiverControlMessage::AuthLoginResponse(
            ReceiverAuthLoginResponsePayload {
                success: true,
                receiver_id: Some(receiver_id.to_string()),
                error: None,
            },
        ))
        .await;
        public_key
    }

    /// Reconnect leg: device_id in, ed25519 challenge out, signature
    /// verified against the enrolled public key.
    async fn challenge_auth(&mut self, device_id: &str, public_key: &str) {
        let login = match self.recv_receiver().await {
            ReceiverMessage::AuthLogin(p) => p,
            other => panic!("expected auth.login as the first message, got {other:?}"),
        };
        assert_eq!(login.device_id.as_deref(), Some(device_id));
        assert_eq!(
            login.enrollment_token, None,
            "enrolled device must not resend a token"
        );

        let challenge = strata_common::auth::generate_challenge();
        self.send_control(&ReceiverControlMessage::AuthChallenge(
            AuthChallengePayload {
                challenge: challenge.clone(),
            },
        ))
        .await;

        let resp = match self.recv_receiver().await {
            ReceiverMessage::AuthChallengeResponse(p) => p,
            other => panic!("expected auth.challenge.response, got {other:?}"),
        };
        assert_eq!(resp.device_id, device_id);
        assert!(
            strata_common::auth::verify_challenge(public_key, &challenge, &resp.signature).unwrap(),
            "challenge signature must verify against the enrolled public key"
        );

        self.send_control(&ReceiverControlMessage::AuthLoginResponse(
            ReceiverAuthLoginResponsePayload {
                success: true,
                receiver_id: Some(device_id.to_string()),
                error: None,
            },
        ))
        .await;
    }
}

fn stream_start(request_id: &str, stream_id: &str, link_count: u32) -> ReceiverControlMessage {
    ReceiverControlMessage::StreamStart(ReceiverStreamStartPayload {
        request_id: request_id.into(),
        stream_id: stream_id.into(),
        link_count,
        relay_url: None,
        bonding_config: serde_json::Value::Null,
    })
}

// ── Tests ────────────────────────────────────────────────────────────

/// Enroll flow: `--enrollment-token` → auth.login with token, public key
/// and the capacity registration (bind_host + port pool) → success response
/// persists the assigned device id, and the daemon starts heartbeating.
#[tokio::test]
async fn enroll_persists_identity_and_starts_heartbeating() {
    let _serial = lock_serial().await;
    let dir = TestDir::new("enroll");
    let identity_path = dir.path.join("identity.json");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/receiver/ws", listener.local_addr().unwrap());
    let _daemon = Daemon::spawn(&url, &identity_path, Some("rcv_e2e.SECRET42"), None);

    let mut conn = Conn::accept(&listener).await;
    let public_key = conn.enroll("rcv_e2e.SECRET42", "rcv_e2e").await;

    let status = conn
        .wait_for("receiver.status heartbeat", |m| match m {
            ReceiverMessage::Status(p) => Some(p),
            _ => None,
        })
        .await;
    assert_eq!(status.active_streams, 0);
    assert_eq!(status.max_streams, 2);
    assert!(status.running_streams.is_empty());

    let identity = DeviceIdentity::load_or_generate(&identity_path).unwrap();
    assert_eq!(identity.device_id.as_deref(), Some("rcv_e2e"));
    assert_eq!(
        identity.public_key, public_key,
        "persisted keypair must be the one sent at enrollment"
    );
}

/// Reconnect flow: with a persisted identity (no token), the daemon answers
/// the server's ed25519 challenge with a valid signature and resumes.
#[tokio::test]
async fn reconnect_answers_ed25519_challenge() {
    let _serial = lock_serial().await;
    let dir = TestDir::new("reconnect");
    let identity_path = dir.path.join("identity.json");

    let (private_key, public_key) = strata_common::auth::generate_device_keypair();
    DeviceIdentity {
        device_id: Some("rcv_reconnect".into()),
        private_key,
        public_key: public_key.clone(),
    }
    .save(&identity_path)
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/receiver/ws", listener.local_addr().unwrap());
    let _daemon = Daemon::spawn(&url, &identity_path, None, None);

    let mut conn = Conn::accept(&listener).await;
    conn.challenge_auth("rcv_reconnect", &public_key).await;

    conn.wait_for("receiver.status heartbeat", |m| match m {
        ReceiverMessage::Status(p) => Some(p),
        _ => None,
    })
    .await;
}

/// receiver.stream.start allocates ports from the daemon's own pool, acks
/// them back, and spawns the pipeline; killing the child yields
/// receiver.stream.ended(pipeline_crash) and the heartbeat drops the stream.
#[tokio::test]
async fn stream_start_allocates_ports_and_crash_is_reported() {
    let _serial = lock_serial().await;
    let dir = TestDir::new("crash");
    let identity_path = dir.path.join("identity.json");
    let script = FakePipelineScript::new(&dir.path);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/receiver/ws", listener.local_addr().unwrap());
    let _daemon = Daemon::spawn(
        &url,
        &identity_path,
        Some("rcv_crash.SECRET42"),
        Some(&script.script),
    );

    let mut conn = Conn::accept(&listener).await;
    conn.enroll("rcv_crash.SECRET42", "rcv_crash").await;

    conn.send_control(&stream_start("req-1", "str_lifecycle", 2))
        .await;

    let started = conn
        .wait_for("receiver.stream.started", |m| match m {
            ReceiverMessage::StreamStarted(p) => Some(p),
            _ => None,
        })
        .await;
    assert_eq!(started.request_id, "req-1");
    assert_eq!(started.stream_id, "str_lifecycle");
    assert!(started.success, "error: {:?}", started.error);
    assert_eq!(
        started.bind_ports,
        vec![6000, 6002],
        "two links must get the first two pool ports"
    );

    script.wait_started().await;
    let args = script.args();
    assert!(
        args.starts_with("receiver "),
        "pipeline must be spawned in receiver mode: {args}"
    );
    assert!(
        args.contains("--bind 127.0.0.1:6000,127.0.0.1:6002"),
        "pipeline must bind the acked ports: {args}"
    );
    assert!(
        args.contains("--stats-dest"),
        "stats relay flag missing: {args}"
    );

    // Heartbeat reflects the running stream…
    conn.wait_for("receiver.status with the stream", |m| match m {
        ReceiverMessage::Status(p) if p.running_streams == ["str_lifecycle"] => Some(()),
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
        .wait_for("receiver.stream.ended", |m| match m {
            ReceiverMessage::StreamEnded(p) => Some(p),
            _ => None,
        })
        .await;
    assert_eq!(ended.stream_id, "str_lifecycle");
    assert_eq!(ended.reason, StreamEndReason::PipelineCrash);

    // Heartbeat running_streams reflects reality again.
    conn.wait_for("idle receiver.status", |m| match m {
        ReceiverMessage::Status(p) if p.running_streams.is_empty() => Some(()),
        _ => None,
    })
    .await;
}

/// The telemetry chain: JSON sent to the pipeline's `--stats-dest` port is
/// parsed and forwarded as receiver.stream.stats with per-link stats and
/// the HLS egress heartbeat intact.
#[tokio::test]
async fn stats_relay_forwards_links_and_egress() {
    let _serial = lock_serial().await;
    let dir = TestDir::new("stats");
    let identity_path = dir.path.join("identity.json");
    let script = FakePipelineScript::new(&dir.path);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/receiver/ws", listener.local_addr().unwrap());
    let _daemon = Daemon::spawn(
        &url,
        &identity_path,
        Some("rcv_stats.SECRET42"),
        Some(&script.script),
    );

    let mut conn = Conn::accept(&listener).await;
    conn.enroll("rcv_stats.SECRET42", "rcv_stats").await;

    conn.send_control(&stream_start("req-stats", "str_stats", 1))
        .await;
    conn.wait_for("receiver.stream.started", |m| match m {
        ReceiverMessage::StreamStarted(p) if p.success => Some(()),
        _ => None,
    })
    .await;
    script.wait_started().await;

    // Stand in for the pipeline's stats relay: send one JSON blob to the
    // --stats-dest address the daemon handed it.
    let stats_dest = script.stats_dest();
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let blob = serde_json::json!({
        "links": [
            {"id": 0, "loss_rate": 0.25, "received_bytes": 1_000_000u64, "observed_bps": 800_000u64},
        ],
        "egress": {"segments_produced": 42, "wd_restarts": 1, "last_segment_age_ms": 900},
        "timestamp_ms": 1u64,
    });
    // The telemetry loop polls once a second; keep sending until it forwards.
    let stats = loop {
        sock.send_to(blob.to_string().as_bytes(), &stats_dest)
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let ReceiverMessage::StreamStats(p) = conn.recv_receiver().await
                    && p.stream_id == "str_stats"
                {
                    break p;
                }
            }
        })
        .await;
        if let Ok(p) = got {
            break p;
        }
    };

    assert_eq!(stats.receiver_id, "rcv_stats");
    assert_eq!(stats.links.len(), 1);
    assert_eq!(stats.links[0].id, 0);
    assert!((stats.links[0].loss_rate - 0.25).abs() < 1e-9);
    assert_eq!(
        stats.links[0].sent_bytes, 1_000_000,
        "receiver-side received bytes"
    );
    assert_eq!(stats.links[0].observed_bps, 800_000);
    let egress = stats
        .egress
        .expect("egress heartbeat must survive the relay");
    assert_eq!(egress.segments_produced, 42);
    assert_eq!(egress.wd_restarts, 1);
    assert_eq!(egress.last_segment_age_ms, 900);
}

/// receiver.stream.stop tears the pipeline down, releases the ports back to
/// the pool (proven by restarting on the same ports), and reports
/// receiver.stream.ended(control_plane_stop).
#[tokio::test]
async fn stream_stop_releases_ports_and_reports_ended() {
    let _serial = lock_serial().await;
    let dir = TestDir::new("stop");
    let identity_path = dir.path.join("identity.json");
    let script = FakePipelineScript::new(&dir.path);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/receiver/ws", listener.local_addr().unwrap());
    let _daemon = Daemon::spawn(
        &url,
        &identity_path,
        Some("rcv_stop.SECRET42"),
        Some(&script.script),
    );

    let mut conn = Conn::accept(&listener).await;
    conn.enroll("rcv_stop.SECRET42", "rcv_stop").await;

    conn.send_control(&stream_start("req-a", "str_a", 2)).await;
    let first = conn
        .wait_for("first receiver.stream.started", |m| match m {
            ReceiverMessage::StreamStarted(p) if p.request_id == "req-a" => Some(p),
            _ => None,
        })
        .await;
    assert!(first.success);
    script.wait_started().await;

    conn.send_control(&ReceiverControlMessage::StreamStop(
        ReceiverStreamStopPayload {
            stream_id: "str_a".into(),
            reason: "test".into(),
        },
    ))
    .await;

    let ended = conn
        .wait_for("receiver.stream.ended", |m| match m {
            ReceiverMessage::StreamEnded(p) => Some(p),
            _ => None,
        })
        .await;
    assert_eq!(ended.stream_id, "str_a");
    assert_eq!(ended.reason, StreamEndReason::ControlPlaneStop);

    // Ports must be back in the pool: a second stream gets them again.
    conn.send_control(&stream_start("req-b", "str_b", 2)).await;
    let second = conn
        .wait_for("second receiver.stream.started", |m| match m {
            ReceiverMessage::StreamStarted(p) if p.request_id == "req-b" => Some(p),
            _ => None,
        })
        .await;
    assert!(second.success, "error: {:?}", second.error);
    assert_eq!(
        second.bind_ports, first.bind_ports,
        "released ports must be reusable"
    );
}
