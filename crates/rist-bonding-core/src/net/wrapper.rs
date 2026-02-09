use anyhow::Result;
use librist_sys::*;
use std::ffi::CStr;
use std::ffi::CString;
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, trace};

use crate::net::state::LinkStats;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Optional per-link RIST recovery (ARQ) tuning parameters.
///
/// When set, these override the librist defaults parsed from the RIST URL.
#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    pub recovery_maxbitrate: Option<u32>,     // kbps
    pub recovery_rtt_max: Option<u32>,        // ms
    pub recovery_reorder_buffer: Option<u32>, // ms
}

unsafe extern "C" fn log_cb(
    _arg: *mut libc::c_void,
    level: rist_log_level,
    msg: *const libc::c_char,
) -> libc::c_int {
    if !msg.is_null() {
        let message = CStr::from_ptr(msg).to_string_lossy();
        // Route librist log levels to tracing levels
        match level {
            l if l <= rist_log_level_RIST_LOG_ERROR => {
                tracing::error!(target: "librist", "{}", message.trim_end());
            }
            l if l <= rist_log_level_RIST_LOG_WARN => {
                tracing::warn!(target: "librist", "{}", message.trim_end());
            }
            l if l <= rist_log_level_RIST_LOG_NOTICE => {
                tracing::info!(target: "librist", "{}", message.trim_end());
            }
            l if l <= rist_log_level_RIST_LOG_INFO => {
                debug!(target: "librist", "{}", message.trim_end());
            }
            _ => {
                trace!(target: "librist", "{}", message.trim_end());
            }
        }
    }
    0
}

/// librist sender context wrapper.
///
/// Manages the lifecycle of a `rist_ctx` pointer including peer creation,
/// stats callback registration, data transmission, and cleanup on drop.
pub struct RistContext {
    ctx: *mut rist_ctx,
    stats_arg: *mut libc::c_void,
    logging_settings: *mut rist_logging_settings,
}

// SAFETY: librist contexts are internally locked — all sender API functions
// (rist_sender_data_write, rist_peer_create, rist_start, rist_stats_callback_set)
// are documented as thread-safe in the librist API. The ctx pointer is only
// accessed through librist functions which hold internal locks.
// The stats_arg pointer is only read from the stats callback (which librist
// serializes) and cleaned up in Drop.
unsafe impl Send for RistContext {}
unsafe impl Sync for RistContext {}

impl Drop for RistContext {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                rist_destroy(self.ctx);
            }
            if !self.stats_arg.is_null() {
                // Return ownership to Arc so it can drop if count goes to zero
                let _ = Arc::from_raw(self.stats_arg as *const LinkStats);
            }
            // Free logging settings — safe to call after rist_destroy since the
            // context no longer references the logging handle.
            if !self.logging_settings.is_null() {
                rist_logging_settings_free2(&mut self.logging_settings);
            }
        }
    }
}

unsafe extern "C" fn stats_cb(
    arg: *mut libc::c_void,
    stats_container: *const rist_stats,
) -> libc::c_int {
    if arg.is_null() || stats_container.is_null() {
        return 0;
    }

    let stats_ref = &*stats_container;
    // Check type
    if stats_ref.stats_type == rist_stats_type_RIST_STATS_SENDER_PEER {
        let sender_stats = &stats_ref.stats.sender_peer;
        let link_stats = &*(arg as *const LinkStats);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        link_stats.last_stats_ms.store(now_ms, Ordering::Relaxed);

        // Raw updates
        link_stats
            .rtt
            .store(sender_stats.rtt as u64, Ordering::Relaxed);
        link_stats.sent.store(sender_stats.sent, Ordering::Relaxed);
        link_stats
            .retransmitted
            .store(sender_stats.retransmitted, Ordering::Relaxed);

        // EWMA Update
        // We take the lock. Since this runs infrequently (100ms), lock contention is negligible.
        if let Ok(mut ewma) = link_stats.ewma_state.lock() {
            ewma.rtt.update(sender_stats.rtt as f64);

            let sent = sender_stats.sent;
            let rex = sender_stats.retransmitted;

            let delta_sent = sent.saturating_sub(ewma.last_sent);
            let delta_rex = rex.saturating_sub(ewma.last_rex);

            let dt_ms = if ewma.last_stats_ms > 0 {
                now_ms.saturating_sub(ewma.last_stats_ms)
            } else {
                0
            };

            ewma.last_sent = sent;
            ewma.last_rex = rex;
            ewma.last_stats_ms = now_ms;

            // Calculate "Badness" ratio: (Retransmitted) / Sent
            // If sent is 0 (idle), we keep previous estimate or decay?
            // If idle, loss is 0.
            let loss_ratio = if delta_sent > 0 {
                delta_rex as f64 / delta_sent as f64
            } else {
                0.0
            };

            // Update EWMA with current loss ratio (0.0 - 1.0+)
            ewma.loss.update(loss_ratio);

            // Bandwidth update (fallback if librist reports 0)
            let mut bw_bps = sender_stats.bandwidth as f64;
            if bw_bps <= 0.0 && dt_ms > 0 && delta_sent > 0 {
                bw_bps = (delta_sent as f64 * 8.0 * 1000.0) / dt_ms as f64;
            }
            ewma.bandwidth.update(bw_bps);
            link_stats
                .bandwidth
                .store(bw_bps.max(0.0) as u64, Ordering::Relaxed);

            // Update Cached Smooth Values
            // RTT in micros
            let rtt_us = (ewma.rtt.value() * 1000.0) as u64;
            link_stats.smoothed_rtt_us.store(rtt_us, Ordering::Relaxed);

            let bw = ewma.bandwidth.value() as u64;
            link_stats.smoothed_bw_bps.store(bw, Ordering::Relaxed);

            // Loss in permille (0-1000)
            let loss_pm = (ewma.loss.value() * 1000.0) as u64;
            link_stats
                .smoothed_loss_permille
                .store(loss_pm, Ordering::Relaxed);
        }
    }

    // Free the stats container
    rist_stats_free(stats_container);

    0
}

impl RistContext {
    pub fn new(profile: rist_profile) -> Result<Self> {
        let mut ctx: *mut rist_ctx = ptr::null_mut();
        let logging_settings;
        unsafe {
            let mut log_settings: *mut rist_logging_settings = ptr::null_mut();
            rist_logging_set(
                &mut log_settings,
                rist_log_level_RIST_LOG_DEBUG,
                Some(log_cb),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            logging_settings = log_settings;

            let ret = rist_sender_create(&mut ctx, profile, 0, log_settings);

            if ret != 0 {
                // Clean up logging settings on failure
                if !log_settings.is_null() {
                    rist_logging_settings_free2(&mut log_settings);
                }
                return Err(anyhow::anyhow!(
                    "Failed to create RIST sender context: {}",
                    ret
                ));
            }
        }

        Ok(Self {
            ctx,
            stats_arg: ptr::null_mut(),
            logging_settings,
        })
    }

    pub fn register_stats(&mut self, stats: Arc<LinkStats>, interval_ms: i32) -> Result<()> {
        if !self.stats_arg.is_null() {
            return Err(anyhow::anyhow!("Stats callback already registered"));
        }

        let arg_ptr = Arc::into_raw(stats) as *mut libc::c_void;

        unsafe {
            let ret = rist_stats_callback_set(self.ctx, interval_ms, Some(stats_cb), arg_ptr);
            if ret != 0 {
                // Reclaim the Arc to avoid leaking it — registration failed so
                // the callback will never be invoked and Drop won't free it.
                let _ = Arc::from_raw(arg_ptr as *const LinkStats);
                return Err(anyhow::anyhow!("Failed to set stats callback: {}", ret));
            }
        }

        // Only store the pointer after successful registration.
        self.stats_arg = arg_ptr;
        Ok(())
    }

    pub fn start(&self) -> Result<()> {
        unsafe {
            let ret = rist_start(self.ctx);
            if ret != 0 {
                return Err(anyhow::anyhow!("Failed to start RIST context: {}", ret));
            }
        }
        Ok(())
    }

    pub fn peer_add(&self, url: &str, recovery: Option<&RecoveryConfig>) -> Result<()> {
        let c_url = CString::new(url)?;
        unsafe {
            let mut peer_config: *mut rist_peer_config = ptr::null_mut();
            if rist_parse_address2(c_url.as_ptr(), &mut peer_config) != 0 {
                return Err(anyhow::anyhow!("Failed to parse address: {}", url));
            }

            // Apply custom recovery parameters if provided
            if let Some(recovery_cfg) = recovery {
                if let Some(maxbitrate) = recovery_cfg.recovery_maxbitrate {
                    (*peer_config).recovery_maxbitrate = maxbitrate;
                }
                if let Some(rtt_max) = recovery_cfg.recovery_rtt_max {
                    (*peer_config).recovery_rtt_max = rtt_max;
                }
                if let Some(reorder_buffer) = recovery_cfg.recovery_reorder_buffer {
                    (*peer_config).recovery_reorder_buffer = reorder_buffer;
                }
            }

            let mut peer: *mut rist_peer = ptr::null_mut();
            let ret = rist_peer_create(self.ctx, &mut peer, peer_config);

            // We must free the config allocated by parse_address2
            rist_peer_config_free2(&mut peer_config);

            if ret != 0 {
                return Err(anyhow::anyhow!("Failed to create peer: {}", ret));
            }
        }
        Ok(())
    }

    pub fn send_data(&self, data: &[u8]) -> Result<usize> {
        unsafe {
            let mut data_block: rist_data_block = std::mem::zeroed();
            data_block.payload = data.as_ptr() as *const libc::c_void;
            data_block.payload_len = data.len() as _; // Let compiler infer or cast if needed.
                                                      // virt_src_port and dst_port default to 0 via zeroed.

            let ret = rist_sender_data_write(self.ctx, &data_block);
            if ret < 0 {
                return Err(anyhow::anyhow!("Failed to send data: {}", ret));
            }
            Ok(ret as usize)
        }
    }
}

/// librist receiver context wrapper.
///
/// Manages a `rist_ctx` in receiver mode for binding to incoming RIST
/// streams, reading data blocks, and cleaning up on drop.
pub struct RistReceiverContext {
    ctx: *mut rist_ctx,
    logging_settings: *mut rist_logging_settings,
}

// SAFETY: librist receiver contexts are internally locked — rist_receiver_data_read
// and rist_peer_create are documented as thread-safe. The ctx pointer is only
// accessed through librist functions which hold internal locks.
unsafe impl Send for RistReceiverContext {}
unsafe impl Sync for RistReceiverContext {}

impl Drop for RistReceiverContext {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                rist_destroy(self.ctx);
            }
            if !self.logging_settings.is_null() {
                rist_logging_settings_free2(&mut self.logging_settings);
            }
        }
    }
}

impl RistReceiverContext {
    pub fn new(profile: rist_profile) -> Result<Self> {
        let mut ctx: *mut rist_ctx = ptr::null_mut();
        let logging_settings;
        unsafe {
            let mut log_settings: *mut rist_logging_settings = ptr::null_mut();
            rist_logging_set(
                &mut log_settings,
                rist_log_level_RIST_LOG_DEBUG,
                Some(log_cb),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            logging_settings = log_settings;

            let ret = rist_receiver_create(&mut ctx, profile, log_settings);

            if ret != 0 {
                if !log_settings.is_null() {
                    rist_logging_settings_free2(&mut log_settings);
                }
                return Err(anyhow::anyhow!(
                    "Failed to create RIST receiver context: {}",
                    ret
                ));
            }
        }
        Ok(Self {
            ctx,
            logging_settings,
        })
    }

    pub fn start(&self) -> Result<()> {
        unsafe {
            let ret = rist_start(self.ctx);
            if ret != 0 {
                return Err(anyhow::anyhow!("Failed to start RIST receiver: {}", ret));
            }
        }
        Ok(())
    }

    pub fn peer_config(&self, url: &str) -> Result<()> {
        let c_url = CString::new(url)?;
        unsafe {
            let mut peer_config: *mut rist_peer_config = ptr::null_mut();
            if rist_parse_address2(c_url.as_ptr(), &mut peer_config) != 0 {
                return Err(anyhow::anyhow!("Failed to parse address: {}", url));
            }

            let mut peer: *mut rist_peer = ptr::null_mut();
            let ret = rist_peer_create(self.ctx, &mut peer, peer_config);
            rist_peer_config_free2(&mut peer_config);

            if ret != 0 {
                return Err(anyhow::anyhow!("Failed to create receiver peer: {}", ret));
            }
        }
        Ok(())
    }

    /// Reads a single data block from the receiver.
    ///
    /// Blocks up to `timeout_ms` milliseconds waiting for data. Returns `None`
    /// on timeout. The returned payload is an owned copy — the underlying
    /// librist data block is freed immediately after copying.
    pub fn read_data(&self, timeout_ms: i32) -> Result<Option<RistDataBlock>> {
        unsafe {
            let mut data_block: *const rist_data_block = ptr::null();
            let ret = rist_receiver_data_read(self.ctx, &mut data_block, timeout_ms);
            if ret < 0 {
                return Err(anyhow::anyhow!("rist_receiver_data_read failed: {}", ret));
            }
            if ret == 0 || data_block.is_null() {
                return Ok(None);
            }

            // Copy payload into owned memory before freeing the librist block.
            let db = &*data_block;
            let payload =
                std::slice::from_raw_parts(db.payload as *const u8, db.payload_len).to_vec();

            let block = RistDataBlock {
                payload,
                virt_src_port: db.virt_src_port,
                virt_dst_port: db.virt_dst_port,
                seq: db.seq,
                flow_id: db.flow_id,
            };

            // Free the librist-allocated data block (free2 replaces deprecated free).
            let mut db_ptr = data_block as *mut rist_data_block;
            rist_receiver_data_block_free2(&mut db_ptr);

            Ok(Some(block))
        }
    }
}

/// Owned copy of a received RIST data block.
///
/// Payload is copied from the librist-allocated block, which is freed
/// immediately after copying.
pub struct RistDataBlock {
    pub payload: Vec<u8>,
    pub virt_src_port: u16,
    pub virt_dst_port: u16,
    pub seq: u64,
    pub flow_id: u32,
}

// Re-export specific enums for ease of use
pub use librist_sys::rist_profile_RIST_PROFILE_SIMPLE as RIST_PROFILE_SIMPLE;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_create_context() {
        let ctx = RistContext::new(RIST_PROFILE_SIMPLE);
        assert!(ctx.is_ok());
        let _ctx = ctx.unwrap();
    }

    #[test]
    fn test_add_peer() {
        let ctx = RistContext::new(RIST_PROFILE_SIMPLE).unwrap();
        // Assume I wrap peer creation too
        let res = ctx.peer_add("rist://127.0.0.1:1234", None);
        assert!(res.is_ok());
    }

    #[test]
    fn test_send_data() {
        let ctx = RistContext::new(RIST_PROFILE_SIMPLE).unwrap();
        ctx.peer_add("rist://127.0.0.1:1235", None).unwrap();
        // UDP is connectionless, so send usually succeeds locally.
        let data = b"Hello RIST";
        let sent = ctx.send_data(data);
        assert!(sent.is_ok());
        // Verify bytes sent count?
        assert_eq!(sent.unwrap(), data.len());
    }

    #[test]
    fn test_receiver_loopback_direct() {
        let receiver = RistReceiverContext::new(RIST_PROFILE_SIMPLE).unwrap();
        // Try random port roughly - retry logic helper
        // Sometimes previous test release hasn't fully propagated in kernel
        let mut bind_ok = false;
        for port in 18000..18010 {
            let url = format!("rist://@0.0.0.0:{}", port);
            if receiver.peer_config(&url).is_ok() {
                receiver.start().unwrap();

                // Sender
                let sender = RistContext::new(RIST_PROFILE_SIMPLE).unwrap();
                sender
                    .peer_add(&format!("rist://127.0.0.1:{}", port), None)
                    .unwrap();
                sender.start().unwrap();

                // Payload
                let payload = b"Direct Loopback Test";
                // Wait for stack
                std::thread::sleep(Duration::from_millis(200));

                for _ in 0..5 {
                    sender.send_data(payload).unwrap();
                    std::thread::sleep(Duration::from_millis(50));
                }

                for _ in 0..50 {
                    if let Ok(Some(block)) = receiver.read_data(100) {
                        assert_eq!(&block.payload[..], payload);
                        bind_ok = true;
                        break;
                    }
                }
                if bind_ok {
                    break;
                }

                // If we didn't get data, maybe repeat? Or just fail?
                // But here we are just trying to bind.
            }
        }

        assert!(bind_ok, "Did not receive data or bind failed");
    }

    #[test]
    fn test_peer_add_invalid_url() {
        let ctx = RistContext::new(RIST_PROFILE_SIMPLE).unwrap();
        // CString::new fails on embedded null bytes, exercising the error path
        let result = ctx.peer_add("rist://127.0.0.1\0:9999", None);
        assert!(result.is_err(), "URL with null byte should fail peer_add");
    }

    #[test]
    fn test_register_stats_twice_fails() {
        let mut ctx = RistContext::new(RIST_PROFILE_SIMPLE).unwrap();
        let stats1 = Arc::new(LinkStats::default());
        let stats2 = Arc::new(LinkStats::default());

        ctx.register_stats(stats1, 100).unwrap();
        let result = ctx.register_stats(stats2, 100);
        assert!(result.is_err(), "Double stats registration should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("already registered"));
    }

    #[test]
    fn test_send_data_without_peer() {
        let ctx = RistContext::new(RIST_PROFILE_SIMPLE).unwrap();
        let data = b"test data";
        let _ = ctx.send_data(data);
    }

    #[test]
    fn test_recovery_config_applied() {
        let ctx = RistContext::new(RIST_PROFILE_SIMPLE).unwrap();
        let recovery = RecoveryConfig {
            recovery_maxbitrate: Some(50000),
            recovery_rtt_max: Some(1000),
            recovery_reorder_buffer: Some(100),
        };
        let result = ctx.peer_add("rist://127.0.0.1:19200", Some(&recovery));
        assert!(
            result.is_ok(),
            "peer_add with recovery config should succeed"
        );
    }

    #[test]
    fn test_receiver_context_creation() {
        let ctx = RistReceiverContext::new(RIST_PROFILE_SIMPLE);
        assert!(ctx.is_ok());
    }

    #[test]
    fn test_receiver_invalid_peer_url() {
        let ctx = RistReceiverContext::new(RIST_PROFILE_SIMPLE).unwrap();
        // CString::new fails on embedded null bytes, exercising the error path
        let result = ctx.peer_config("rist://127.0.0.1\0:9999");
        assert!(result.is_err());
    }

    #[test]
    fn test_context_drop_safety() {
        {
            let mut ctx = RistContext::new(RIST_PROFILE_SIMPLE).unwrap();
            ctx.peer_add("rist://127.0.0.1:19201", None).unwrap();
            let stats = Arc::new(LinkStats::default());
            ctx.register_stats(stats, 100).unwrap();
            ctx.start().unwrap();
        }
        // Context dropped — should not crash or leak
    }
}
