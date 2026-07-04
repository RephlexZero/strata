//! Pipeline registry — manages multiple concurrent receiver pipelines.
//!
//! Unlike the sender (which runs one pipeline at a time), the receiver
//! can run multiple pipelines simultaneously, one per assigned stream.

use std::collections::HashMap;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

/// UDP base address for stats relay. Each pipeline gets a unique port
/// starting from this base: 9200, 9201, 9202, ...
pub const STATS_LISTEN_BASE: u16 = 9200;

const PIPELINE_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
static TEST_PIPELINE_BIN: std::sync::Mutex<Option<std::ffi::OsString>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static TEST_PIPELINE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Registry of running receiver pipelines, keyed by stream_id.
pub struct PipelineRegistry {
    pipelines: HashMap<String, PipelineEntry>,
    next_stats_port: u16,
}

struct PipelineEntry {
    child: Child,
    bind_ports: Vec<u16>,
    stats_port: u16,
    started_at: Instant,
    total_bytes: u64,
}

pub struct PipelineStopStats {
    pub duration_s: u64,
    pub total_bytes: u64,
    pub bind_ports: Vec<u16>,
}

pub struct ChildExitInfo {
    pub stream_id: String,
    pub exit_status: ExitStatus,
    pub duration_s: u64,
    pub total_bytes: u64,
    pub bind_ports: Vec<u16>,
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

fn shutdown_child(child: &mut Child, timeout: Duration, stream_id: &str) {
    send_sigint(child);

    match wait_with_timeout(child, timeout) {
        Ok(_) => tracing::info!(stream_id = %stream_id, "pipeline exited cleanly"),
        Err(_) => {
            tracing::warn!(stream_id = %stream_id, "pipeline didn't exit cleanly, killing");
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl PipelineRegistry {
    pub fn new() -> Self {
        Self {
            pipelines: HashMap::new(),
            next_stats_port: STATS_LISTEN_BASE,
        }
    }

    /// Number of currently running pipelines.
    /// IDs of all currently-registered pipelines (for heartbeat reporting).
    pub fn running_ids(&self) -> Vec<String> {
        self.pipelines.keys().cloned().collect()
    }

    pub fn active_count(&self) -> usize {
        self.pipelines.len()
    }

    /// Check if a stream is already running.
    pub fn has_stream(&self, stream_id: &str) -> bool {
        self.pipelines.contains_key(stream_id)
    }

    /// Start a receiver pipeline for a stream.
    pub fn start(
        &mut self,
        stream_id: &str,
        bind_host: &str,
        bind_ports: &[u16],
        relay_url: Option<&str>,
        bonding_config: &serde_json::Value,
    ) -> anyhow::Result<()> {
        if self.pipelines.contains_key(stream_id) {
            anyhow::bail!("pipeline already running for stream {stream_id}");
        }

        let stats_port = self.next_stats_port;
        self.next_stats_port += 1;
        let stats_addr = format!("127.0.0.1:{stats_port}");

        tracing::info!(
            stream_id = %stream_id,
            bind_ports = ?bind_ports,
            stats_port = stats_port,
            "starting receiver pipeline"
        );

        let child = spawn_receiver_pipeline(
            stream_id,
            bind_host,
            bind_ports,
            relay_url,
            bonding_config,
            &stats_addr,
        )?;

        self.pipelines.insert(
            stream_id.to_string(),
            PipelineEntry {
                child,
                bind_ports: bind_ports.to_vec(),
                stats_port,
                started_at: Instant::now(),
                total_bytes: 0,
            },
        );

        Ok(())
    }

    /// Stop a specific pipeline by stream_id.
    pub fn stop(&mut self, stream_id: &str) -> Option<PipelineStopStats> {
        self.stop_with_timeout(stream_id, PIPELINE_STOP_TIMEOUT)
    }

    fn stop_with_timeout(
        &mut self,
        stream_id: &str,
        timeout: Duration,
    ) -> Option<PipelineStopStats> {
        let mut entry = self.pipelines.remove(stream_id)?;
        let duration_s = entry.started_at.elapsed().as_secs();

        shutdown_child(&mut entry.child, timeout, stream_id);

        Some(PipelineStopStats {
            duration_s,
            total_bytes: entry.total_bytes,
            bind_ports: entry.bind_ports,
        })
    }

    /// Stop all running pipelines (used during shutdown).
    pub fn stop_all(&mut self) {
        let stream_ids: Vec<String> = self.pipelines.keys().cloned().collect();
        for id in stream_ids {
            self.stop(&id);
        }
    }

    /// Check all child processes for unexpected exits.
    pub fn check_exits(&mut self) -> Vec<ChildExitInfo> {
        let mut exits = Vec::new();
        let mut exited_ids = Vec::new();

        for (stream_id, entry) in &mut self.pipelines {
            match entry.child.try_wait() {
                Ok(Some(status)) => {
                    exits.push(ChildExitInfo {
                        stream_id: stream_id.clone(),
                        exit_status: status,
                        duration_s: entry.started_at.elapsed().as_secs(),
                        total_bytes: entry.total_bytes,
                        bind_ports: entry.bind_ports.clone(),
                    });
                    exited_ids.push(stream_id.clone());
                }
                Ok(None) => {} // Still running
                Err(e) => {
                    tracing::warn!(stream_id = %stream_id, error = %e, "error checking child");
                }
            }
        }

        for id in exited_ids {
            self.pipelines.remove(&id);
        }

        exits
    }

    /// Get the stats listen port for a given stream.
    pub fn stats_port(&self, stream_id: &str) -> Option<u16> {
        self.pipelines.get(stream_id).map(|e| e.stats_port)
    }

    /// Get all active stream IDs and their stats ports.
    pub fn active_streams(&self) -> Vec<(String, u16)> {
        self.pipelines
            .iter()
            .map(|(id, e)| (id.clone(), e.stats_port))
            .collect()
    }
}

/// Spawn `strata-pipeline receiver` as a child process.
fn spawn_receiver_pipeline(
    stream_id: &str,
    bind_host: &str,
    bind_ports: &[u16],
    relay_url: Option<&str>,
    bonding_config: &serde_json::Value,
    stats_addr: &str,
) -> anyhow::Result<Child> {
    let bin = pipeline_binary();
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("receiver");

    // Bind addresses: one per link port
    let bind_str: String = bind_ports
        .iter()
        .map(|p| format!("{bind_host}:{p}"))
        .collect::<Vec<_>>()
        .join(",");
    cmd.arg("--bind").arg(&bind_str);

    // Relay URL (RTMP/HLS output)
    if let Some(url) = relay_url {
        cmd.arg("--relay-url").arg(url);
    }

    // Stats relay
    cmd.arg("--stats-dest").arg(stats_addr);

    // Write bonding config to temp file if non-empty
    if !bonding_config.is_null() {
        let config_path = format!("/tmp/strata-recv-{stream_id}.toml");
        if let Ok(toml_str) = toml::to_string_pretty(bonding_config) {
            if let Err(e) = std::fs::write(&config_path, &toml_str) {
                tracing::warn!(error = %e, path = %config_path, "failed to write bonding config");
            } else {
                cmd.arg("--config").arg(&config_path);
            }
        }
    }

    tracing::info!(cmd = ?cmd, "spawning strata-pipeline receiver");
    let child = cmd.spawn()?;
    Ok(child)
}

fn wait_with_timeout(child: &mut Child, timeout: std::time::Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(_) => return Ok(()),
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
                "strata-receiver-pipeline-{}-{}",
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

    #[cfg(not(unix))]
    fn process_is_alive(_pid: i32) -> bool {
        false
    }

    #[test]
    fn stop_sends_sigint_and_releases_pipeline() {
        let script = TestPipelineScript::new("graceful");
        let _guard = set_test_pipeline_bin(&script.script);
        let mut registry = PipelineRegistry::new();

        registry
            .start(
                "stream-1",
                "127.0.0.1",
                &[5000, 5002],
                None,
                &serde_json::Value::Null,
            )
            .unwrap();
        script.wait_for_marker("started");
        let pid = script.pid();

        let stats = registry
            .stop_with_timeout("stream-1", Duration::from_millis(500))
            .unwrap();

        assert_eq!(stats.bind_ports, vec![5000, 5002]);
        assert_eq!(stats.total_bytes, 0);
        assert_eq!(registry.active_count(), 0);
        assert!(script.marker_contents().contains("sigint"));
        assert!(!process_is_alive(pid));
    }

    #[test]
    fn stop_kills_unresponsive_pipeline_after_timeout() {
        let script = TestPipelineScript::new("stubborn");
        let _guard = set_test_pipeline_bin(&script.script);
        let mut registry = PipelineRegistry::new();

        registry
            .start(
                "stream-1",
                "127.0.0.1",
                &[5000],
                None,
                &serde_json::Value::Null,
            )
            .unwrap();
        script.wait_for_marker("started");
        let pid = script.pid();

        registry.stop_with_timeout("stream-1", Duration::from_millis(200));

        assert_eq!(registry.active_count(), 0);
        assert!(script.marker_contents().contains("sigint"));
        assert!(!process_is_alive(pid));
    }
}
