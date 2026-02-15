//! Pipeline manager — spawns and manages GStreamer sender pipelines.
//!
//! The agent spawns `integration_node` as a child process for clean isolation.
//! In simulation mode, it runs a lightweight fake pipeline that generates
//! synthetic stats without actual media processing.

use std::process::Child;
use std::time::Instant;

use strata_common::protocol::StreamStartPayload;

/// Pipeline manager state.
pub struct PipelineManager {
    simulate: bool,
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

impl PipelineManager {
    pub fn new(simulate: bool) -> Self {
        Self {
            simulate,
            child: None,
            stream_id: None,
            started_at: None,
            total_bytes: 0,
        }
    }

    /// Check if a pipeline is currently running.
    pub fn is_running(&self) -> bool {
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
            simulate = self.simulate,
            "starting pipeline"
        );

        if self.simulate {
            // Simulation mode — no actual child process
            self.stream_id = Some(payload.stream_id);
            self.started_at = Some(Instant::now());
            self.total_bytes = 0;
            tracing::info!("simulated pipeline started");
        } else {
            // Real mode — spawn integration_node
            let child = spawn_integration_node(&payload)?;
            self.child = Some(child);
            self.stream_id = Some(payload.stream_id);
            self.started_at = Some(Instant::now());
            self.total_bytes = 0;
        }

        Ok(())
    }

    /// Stop the current pipeline.
    pub fn stop(&mut self) -> PipelineStopStats {
        let duration_s = self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);

        if let Some(mut child) = self.child.take() {
            // Send SIGINT for graceful EOS shutdown
            #[cfg(unix)]
            {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGINT);
                }
            }

            // Wait up to 5 seconds for clean exit
            match wait_with_timeout(&mut child, std::time::Duration::from_secs(5)) {
                Ok(_) => tracing::info!("pipeline exited cleanly"),
                Err(_) => {
                    tracing::warn!("pipeline didn't exit cleanly, killing");
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
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

    /// Update accumulated bytes (called from telemetry).
    pub fn add_bytes(&mut self, bytes: u64) {
        self.total_bytes += bytes;
    }

    /// Get elapsed seconds since pipeline started, if running.
    pub fn elapsed_s(&self) -> u64 {
        self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }
}

/// Spawn the `integration_node` binary as a child process.
fn spawn_integration_node(payload: &StreamStartPayload) -> anyhow::Result<Child> {
    let mut cmd = std::process::Command::new("integration_node");
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

    // Framerate (from source config, default 30)
    if let Some(fps) = payload.source.framerate {
        cmd.arg("--framerate").arg(fps.to_string());
    }

    // Always add audio for RTMP compatibility
    cmd.arg("--audio");

    // Destinations (RIST URLs)
    if !payload.destinations.is_empty() {
        let dest_str = payload.destinations.join(",");
        cmd.arg("--dest").arg(&dest_str);
    }

    // Write bonding config to temp file if non-empty
    if !payload.bonding_config.is_null() {
        let config_path = format!("/tmp/strata-stream-{}.toml", payload.stream_id);
        if let Ok(toml_str) = toml::to_string_pretty(&payload.bonding_config) {
            let _ = std::fs::write(&config_path, &toml_str);
            cmd.arg("--config").arg(&config_path);
        }
    }

    tracing::info!(cmd = ?cmd, "spawning integration_node");
    let child = cmd.spawn()?;
    Ok(child)
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
