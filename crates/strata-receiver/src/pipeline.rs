//! Pipeline registry — manages multiple concurrent receiver pipelines.
//!
//! Unlike the sender (which runs one pipeline at a time), the receiver
//! can run multiple pipelines simultaneously, one per assigned stream.

use std::collections::HashMap;
use std::process::{Child, ExitStatus};
use std::time::Instant;

/// UDP base address for stats relay. Each pipeline gets a unique port
/// starting from this base: 9200, 9201, 9202, ...
pub const STATS_LISTEN_BASE: u16 = 9200;

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

impl PipelineRegistry {
    pub fn new() -> Self {
        Self {
            pipelines: HashMap::new(),
            next_stats_port: STATS_LISTEN_BASE,
        }
    }

    /// Number of currently running pipelines.
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
        let mut entry = self.pipelines.remove(stream_id)?;
        let duration_s = entry.started_at.elapsed().as_secs();

        // Send SIGINT for graceful EOS shutdown
        #[cfg(unix)]
        {
            let pid = entry.child.id() as libc::pid_t;
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
        }

        // Wait up to 5 seconds for clean exit
        match wait_with_timeout(&mut entry.child, std::time::Duration::from_secs(5)) {
            Ok(_) => tracing::info!(stream_id = %stream_id, "pipeline exited cleanly"),
            Err(_) => {
                tracing::warn!(stream_id = %stream_id, "pipeline didn't exit cleanly, killing");
                let _ = entry.child.kill();
                let _ = entry.child.wait();
            }
        }

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
    let bin =
        std::env::var("STRATA_PIPELINE_BIN").unwrap_or_else(|_| "strata-pipeline".to_string());
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
