//! Pipeline manager — spawns and manages GStreamer sender pipelines.
//!
//! The daemon spawns `strata-pipeline` as a child process for clean isolation.
//!
//! Stats are relayed from strata-pipeline via UDP to 127.0.0.1:9100,
//! where the telemetry module reads and forwards them to the control plane.
//!
//! Hot-swap source switching is supported via a Unix domain socket at
//! `/tmp/strata-pipeline.sock`.

use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use strata_protocol::StreamStartPayload;

/// UDP address where strata-node sends stats JSON.
pub const STATS_LISTEN_ADDR: &str = "127.0.0.1:9100";

/// Unix socket path for pipeline control (hot-swap commands).
pub const CONTROL_SOCK_PATH: &str = "/tmp/strata-pipeline.sock";

const PIPELINE_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
static TEST_PIPELINE_BIN: std::sync::Mutex<Option<std::ffi::OsString>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static TEST_PIPELINE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Pipeline manager state.
pub struct PipelineManager {
    child: Option<Child>,
    stream_id: Option<String>,
    started_at: Option<Instant>,
    total_bytes: u64,
}

/// Stats returned when a pipeline is stopped.
pub struct PipelineStopStats {
    pub duration_s: u64,
    pub total_bytes: u64,
}

/// Send a JSON command string to the pipeline's Unix control socket.
/// Returns `true` if the message was sent successfully.
fn send_to_control_socket(msg: &str) -> bool {
    match std::os::unix::net::UnixStream::connect(CONTROL_SOCK_PATH) {
        Ok(mut stream) => {
            use std::io::Write;
            if let Err(e) = stream.write_all(msg.as_bytes()) {
                tracing::warn!(error = %e, "failed to send command to pipeline control socket");
                false
            } else {
                true
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to connect to pipeline control socket");
            false
        }
    }
}

fn pipeline_binary() -> std::ffi::OsString {
    #[cfg(test)]
    if let Some(bin) = TEST_PIPELINE_BIN.lock().unwrap().clone() {
        return bin;
    }

    std::env::var_os("STRATA_PIPELINE_BIN").unwrap_or_else(|| "strata-pipeline".into())
}

#[cfg(unix)]
fn send_sigint(child: &Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: `child.id()` is the OS PID for this child process. `kill` is used
    // only to deliver SIGINT to that child, and ESRCH is handled by the caller.
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }
}

#[cfg(not(unix))]
fn send_sigint(_child: &Child) {}

fn shutdown_child(child: &mut Child, timeout: Duration) {
    send_sigint(child);

    match wait_with_timeout(child, timeout) {
        Ok(_) => tracing::info!("pipeline exited cleanly"),
        Err(_) => {
            tracing::warn!("pipeline didn't exit cleanly, killing");
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl PipelineManager {
    pub fn new() -> Self {
        Self {
            child: None,
            stream_id: None,
            started_at: None,
            total_bytes: 0,
        }
    }

    /// Check if a pipeline is currently running.
    ///
    /// Also checks the actual child process — if it has exited but we
    /// haven't cleaned up yet, returns `false`.
    pub fn is_running(&mut self) -> bool {
        if self.stream_id.is_some() {
            if let Some(ref mut child) = self.child {
                // Check if the child process is still alive
                match child.try_wait() {
                    Ok(Some(_)) => {
                        // Child has exited but we haven't cleaned up yet
                        return false;
                    }
                    Ok(None) => {} // Still running
                    Err(_) => {}   // Error checking — assume still running
                }
            } else {
                // No child process — not really running
                return false;
            }
        }
        self.stream_id.is_some()
    }

    /// Quick check if a stream ID is set (does not poll the child process).
    /// Use this when you only need a guard and don't hold &mut self.
    pub fn has_stream(&self) -> bool {
        self.stream_id.is_some()
    }

    /// Current stream ID, if any.
    #[allow(dead_code)]
    pub fn stream_id(&self) -> Option<&str> {
        self.stream_id.as_deref()
    }

    /// Start a sender pipeline.
    pub fn start(&mut self, payload: StreamStartPayload) -> anyhow::Result<()> {
        if self.is_running() {
            anyhow::bail!(
                "pipeline already running (stream {})",
                self.stream_id.as_deref().unwrap_or("?")
            );
        }

        tracing::info!(
            stream_id = %payload.stream_id,
            source_mode = %payload.source.mode,
            bitrate_kbps = payload.encoder.bitrate_kbps,
            "starting pipeline"
        );

        // Spawn strata-pipeline
        let child = spawn_pipeline(&payload)?;
        self.child = Some(child);
        self.stream_id = Some(payload.stream_id);
        self.started_at = Some(Instant::now());
        self.total_bytes = 0;

        Ok(())
    }

    /// Stop the current pipeline.
    pub fn stop(&mut self) -> PipelineStopStats {
        self.stop_with_timeout(PIPELINE_STOP_TIMEOUT)
    }

    fn stop_with_timeout(&mut self, timeout: Duration) -> PipelineStopStats {
        let duration_s = self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);

        if let Some(mut child) = self.child.take() {
            shutdown_child(&mut child, timeout);
        }

        let stats = PipelineStopStats {
            duration_s,
            total_bytes: self.total_bytes,
        };

        self.stream_id = None;
        self.started_at = None;
        self.total_bytes = 0;

        tracing::info!(duration_s = stats.duration_s, "pipeline stopped");
        stats
    }

    /// Switch the active video source on a running pipeline.
    ///
    /// Sends a JSON command to the strata-node's control socket.
    /// The command is fire-and-forget — errors are logged but not propagated.
    pub fn switch_source(
        &self,
        mode: &str,
        device: Option<&str>,
        uri: Option<&str>,
        pattern: Option<&str>,
    ) {
        if !self.has_stream() {
            tracing::warn!("cannot switch source: no pipeline running");
            return;
        }

        let mut cmd = serde_json::json!({
            "cmd": "switch_source",
            "mode": mode,
        });
        if let Some(d) = device {
            cmd["device"] = serde_json::json!(d);
        }
        if let Some(u) = uri {
            cmd["uri"] = serde_json::json!(u);
        }
        if let Some(p) = pattern {
            cmd["pattern"] = serde_json::json!(p);
        }

        // Connect to the control socket and send the command
        let msg = format!("{}\n", cmd);
        if send_to_control_socket(&msg) {
            tracing::info!(mode, "source switch command sent");
        }
    }

    /// Tell the running pipeline to enable or disable a bonding link
    /// associated with the given OS interface name.
    ///
    /// This sends a `toggle_link` command over the control socket so
    /// strata-node can add or remove the link at runtime without
    /// affecting OS-level connectivity.
    pub fn toggle_link(&self, iface: &str, enabled: bool) {
        if !self.has_stream() {
            tracing::debug!(iface, enabled, "no pipeline running, skip toggle_link");
            return;
        }

        let cmd = serde_json::json!({
            "cmd": "toggle_link",
            "interface": iface,
            "enabled": enabled,
        });

        let msg = format!("{}\n", cmd);
        if send_to_control_socket(&msg) {
            tracing::info!(iface, enabled, "toggle_link command sent");
        }
    }

    /// Send an arbitrary JSON command to the running strata-node process.
    ///
    /// Returns `true` if the command was sent successfully.
    pub fn send_command(&self, cmd: &serde_json::Value) -> bool {
        if !self.has_stream() {
            tracing::debug!("no pipeline running, skip send_command");
            return false;
        }
        let msg = format!("{}\n", cmd);
        send_to_control_socket(&msg)
    }

    /// Check if the child process has exited unexpectedly.
    ///
    /// Returns `Some(info)` if the child exited (crash or normal EOS),
    /// along with stream info for reporting. Cleans up internal state.
    pub fn check_child_exit(&mut self) -> Option<ChildExitInfo> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                let stream_id = self.stream_id.take().unwrap_or_default();
                let duration_s = self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                let total_bytes = self.total_bytes;

                // Clean up
                self.child = None;
                self.started_at = None;
                self.total_bytes = 0;

                Some(ChildExitInfo {
                    stream_id,
                    exit_status: status,
                    duration_s,
                    total_bytes,
                })
            }
            Ok(None) => None, // Still running
            Err(e) => {
                tracing::warn!(error = %e, "error checking child process status");
                None
            }
        }
    }

    /// Update accumulated bytes (called from telemetry).
    pub fn add_bytes(&mut self, bytes: u64) {
        self.total_bytes += bytes;
    }

    /// Get elapsed seconds since pipeline started, if running.
    pub fn elapsed_s(&self) -> u64 {
        self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }
}

/// Spawn the `strata-pipeline` binary as a child process.
fn spawn_pipeline(payload: &StreamStartPayload) -> anyhow::Result<Child> {
    let bin = pipeline_binary();
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("sender");

    // Source flags
    match payload.source.mode.as_str() {
        "v4l2" => {
            cmd.arg("--source").arg("v4l2");
            if let Some(ref device) = payload.source.device {
                cmd.arg("--device").arg(device);
            }
        }
        "uri" => {
            cmd.arg("--source").arg("uri");
            if let Some(ref uri) = payload.source.uri {
                cmd.arg("--uri").arg(uri);
            }
        }
        _ => {
            cmd.arg("--source").arg("test");
        }
    }

    // Encoder
    cmd.arg("--bitrate")
        .arg(payload.encoder.bitrate_kbps.to_string());

    // Passthrough mode — bypass encoder for file sources
    if payload.source.passthrough.unwrap_or(false) {
        cmd.arg("--passthrough");
    }

    // Codec (default h265)
    if let Some(ref codec) = payload.encoder.codec {
        cmd.arg("--codec").arg(codec);
    }

    // Bitrate envelope for adaptation
    if let Some(min) = payload.encoder.min_bitrate_kbps {
        cmd.arg("--min-bitrate").arg(min.to_string());
    }
    if let Some(max) = payload.encoder.max_bitrate_kbps {
        cmd.arg("--max-bitrate").arg(max.to_string());
    }

    // Framerate (from source config, default 30)
    if let Some(fps) = payload.source.framerate {
        cmd.arg("--framerate").arg(fps.to_string());
    }

    // Resolution (from source config, default 1280x720)
    if let Some(ref res) = payload.source.resolution {
        cmd.arg("--resolution").arg(res);
    }

    // Always add audio for RTMP compatibility
    cmd.arg("--audio");

    // Stats relay — always relay stats to the agent's telemetry listener
    cmd.arg("--stats-dest").arg(STATS_LISTEN_ADDR);

    // Control socket for hot-swap
    cmd.arg("--control").arg(CONTROL_SOCK_PATH);

    // Destinations
    if !payload.destinations.is_empty() {
        let dest_str = payload.destinations.join(",");
        cmd.arg("--dest").arg(&dest_str);
    }

    // Write bonding config (+ per-link interface bindings) to a temp file.
    // Every destination shares the receiver's host, so the pipeline's
    // `ip route get` fallback resolves all links onto the default route —
    // the bonded links must be pinned to distinct interfaces here, via the
    // [[links]] section the pipeline gives priority to.
    let mut config_tbl = match toml::Value::try_from(&payload.bonding_config) {
        Ok(toml::Value::Table(t)) => t,
        _ => toml::value::Table::new(),
    };
    if !config_tbl.contains_key("links") && !payload.destinations.is_empty() {
        let mut ifaces: Vec<String> = crate::hardware::scan_network_interfaces()
            .into_iter()
            .filter(|i| i.state == strata_protocol::models::InterfaceState::Connected)
            .map(|i| i.name)
            .collect();
        ifaces.sort();
        if ifaces.len() < payload.destinations.len() {
            tracing::warn!(
                links = payload.destinations.len(),
                connected = ifaces.len(),
                "fewer connected interfaces than links; unpinned links fall back to route lookup"
            );
        }
        let links: Vec<toml::Value> = payload
            .destinations
            .iter()
            .zip(ifaces.iter())
            .map(|(uri, iface)| {
                let mut t = toml::value::Table::new();
                t.insert("uri".into(), toml::Value::String(uri.clone()));
                t.insert("interface".into(), toml::Value::String(iface.clone()));
                toml::Value::Table(t)
            })
            .collect();
        if !links.is_empty() {
            tracing::info!(links = ?links, "pinned links to interfaces");
            config_tbl.insert("links".into(), toml::Value::Array(links));
        }
    }
    if !config_tbl.is_empty() {
        let config_path = format!("/tmp/strata-stream-{}.toml", payload.stream_id);
        match toml::to_string_pretty(&toml::Value::Table(config_tbl)) {
            Ok(toml_str) => {
                if let Err(e) = std::fs::write(&config_path, &toml_str) {
                    tracing::warn!(error = %e, path = %config_path, "failed to write bonding config");
                } else {
                    cmd.arg("--config").arg(&config_path);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize bonding config");
            }
        }
    }

    tracing::info!(cmd = ?cmd, "spawning strata-node");
    let child = cmd.spawn()?;
    Ok(child)
}

/// Info returned when a child process exits unexpectedly.
pub struct ChildExitInfo {
    pub stream_id: String,
    pub exit_status: ExitStatus,
    pub duration_s: u64,
    pub total_bytes: u64,
}

/// Wait for a child process with a timeout.
fn wait_with_timeout(child: &mut Child, timeout: std::time::Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(_status) => return Ok(()),
            None => {
                if Instant::now() >= deadline {
                    anyhow::bail!("timeout waiting for child process");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use strata_protocol::{EncoderConfig, SourceConfig};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    struct TestPipelineBinGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for TestPipelineBinGuard {
        fn drop(&mut self) {
            *TEST_PIPELINE_BIN.lock().unwrap() = None;
        }
    }

    struct TestPipelineScript {
        dir: PathBuf,
        script: PathBuf,
        marker: PathBuf,
        pidfile: PathBuf,
    }

    impl TestPipelineScript {
        fn new(mode: &str) -> Self {
            let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "strata-sender-pipeline-{}-{}",
                std::process::id(),
                unique
            ));
            fs::create_dir_all(&dir).unwrap();

            let script = dir.join("fake-pipeline.sh");
            let marker = dir.join("events.log");
            let pidfile = dir.join("pid");
            let behavior = match mode {
                "graceful" => "trap 'echo sigint >> \"$marker\"; exit 0' INT",
                "stubborn" => "trap 'echo sigint >> \"$marker\"' INT",
                other => panic!("unknown test pipeline mode: {other}"),
            };
            let body = format!(
                "#!/usr/bin/env bash\nset -eu\nmarker='{marker}'\npidfile='{pidfile}'\necho $$ > \"$pidfile\"\necho started > \"$marker\"\n{behavior}\nwhile :; do\n  read -r -t 1 _ || true\ndone\n",
                marker = marker.display(),
                pidfile = pidfile.display(),
                behavior = behavior,
            );
            fs::write(&script, body).unwrap();

            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();

            Self {
                dir,
                script,
                marker,
                pidfile,
            }
        }

        fn wait_for_marker(&self, needle: &str) {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if self.marker_contents().contains(needle) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for marker {needle}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn marker_contents(&self) -> String {
            fs::read_to_string(&self.marker).unwrap_or_default()
        }

        fn pid(&self) -> i32 {
            let pid = fs::read_to_string(&self.pidfile).unwrap();
            pid.trim().parse().unwrap()
        }
    }

    impl Drop for TestPipelineScript {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn set_test_pipeline_bin(path: &Path) -> TestPipelineBinGuard {
        let lock = TEST_PIPELINE_LOCK.lock().unwrap();
        *TEST_PIPELINE_BIN.lock().unwrap() = Some(path.as_os_str().to_os_string());
        TestPipelineBinGuard { _lock: lock }
    }

    fn sample_payload() -> StreamStartPayload {
        StreamStartPayload {
            stream_id: "test-stream".into(),
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
        }
    }

    #[cfg(unix)]
    fn process_is_alive(pid: i32) -> bool {
        // SAFETY: signal 0 probes process existence without delivering a signal.
        unsafe {
            if libc::kill(pid, 0) == 0 {
                return true;
            }
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[test]
    fn stop_sends_sigint_and_clears_state() {
        let script = TestPipelineScript::new("graceful");
        let _guard = set_test_pipeline_bin(&script.script);
        let mut manager = PipelineManager::new();

        manager.start(sample_payload()).unwrap();
        script.wait_for_marker("started");
        let pid = script.pid();

        let stats = manager.stop_with_timeout(Duration::from_millis(500));

        assert_eq!(stats.total_bytes, 0);
        assert!(!manager.has_stream());
        assert!(script.marker_contents().contains("sigint"));
        assert!(!process_is_alive(pid));
    }

    #[test]
    fn stop_kills_unresponsive_child_after_timeout() {
        let script = TestPipelineScript::new("stubborn");
        let _guard = set_test_pipeline_bin(&script.script);
        let mut manager = PipelineManager::new();

        manager.start(sample_payload()).unwrap();
        script.wait_for_marker("started");
        let pid = script.pid();

        manager.stop_with_timeout(Duration::from_millis(200));

        assert!(!manager.has_stream());
        assert!(script.marker_contents().contains("sigint"));
        assert!(!process_is_alive(pid));
    }
}
