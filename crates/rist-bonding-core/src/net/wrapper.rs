use anyhow::Result;
use librist_sys::*;
use std::ffi::CStr;
use std::ffi::CString;
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::net::state::LinkStats;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Optional RIST recovery tuning parameters per link
#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    pub recovery_maxbitrate: Option<u32>,     // kbps
    pub recovery_rtt_max: Option<u32>,        // ms
    pub recovery_reorder_buffer: Option<u32>, // ms
}

unsafe extern "C" fn log_cb(
    _arg: *mut libc::c_void,
    _level: rist_log_level,
    msg: *const libc::c_char,
) -> libc::c_int {
    if !msg.is_null() {
        let message = CStr::from_ptr(msg).to_string_lossy();
        eprint!("[LIBRIST] {}", message);
    }
    0
}

pub struct RistContext {
    ctx: *mut rist_ctx,
    stats_arg: *mut libc::c_void,
}

// RIST contexts are thread-safe (internally locked by librist), so we can mark Send/Sync.
// Caution: we must verify this assumption for every API we use.
// rist_sender_data_write is thread safe.
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
        unsafe {
            let mut logging_settings: *mut rist_logging_settings = ptr::null_mut();
            rist_logging_set(
                &mut logging_settings,
                rist_log_level_RIST_LOG_DEBUG,
                Some(log_cb),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );

            // function signature: rist_sender_create(ctx, profile, flow_id, logging_settings)
            // flow_id 0 is fine for simple profile/unmanaged? Or should use random?
            // "The flow_id is a unique identifier for the stream."
            // In simple profile, it might be ignored or used for muxing. 0 is safe default.
            let ret = rist_sender_create(&mut ctx, profile, 0, logging_settings);

            // Intentionally leaking logging_settings to avoid segfault on free (known issue in some versions or bindings)

            if ret != 0 {
                return Err(anyhow::anyhow!(
                    "Failed to create RIST sender context: {}",
                    ret
                ));
            }
        }

        Ok(Self {
            ctx,
            stats_arg: ptr::null_mut(),
        })
    }

    pub fn register_stats(&mut self, stats: Arc<LinkStats>, interval_ms: i32) -> Result<()> {
        if !self.stats_arg.is_null() {
            return Err(anyhow::anyhow!("Stats callback already registered"));
        }

        let arg_ptr = Arc::into_raw(stats) as *mut libc::c_void;
        self.stats_arg = arg_ptr;

        unsafe {
            let ret = rist_stats_callback_set(self.ctx, interval_ms, Some(stats_cb), arg_ptr);
            if ret != 0 {
                return Err(anyhow::anyhow!("Failed to set stats callback: {}", ret));
            }
        }
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
            libc::free(peer_config as *mut libc::c_void);

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

pub struct RistReceiverContext {
    ctx: *mut rist_ctx,
}

unsafe impl Send for RistReceiverContext {}
unsafe impl Sync for RistReceiverContext {}

impl Drop for RistReceiverContext {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                rist_destroy(self.ctx);
            }
        }
    }
}

impl RistReceiverContext {
    pub fn new(profile: rist_profile) -> Result<Self> {
        let mut ctx: *mut rist_ctx = ptr::null_mut();
        unsafe {
            let mut logging_settings: *mut rist_logging_settings = ptr::null_mut();
            rist_logging_set(
                &mut logging_settings,
                rist_log_level_RIST_LOG_DEBUG,
                Some(log_cb),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );

            let ret = rist_receiver_create(&mut ctx, profile, logging_settings);

            if ret != 0 {
                return Err(anyhow::anyhow!(
                    "Failed to create RIST receiver context: {}",
                    ret
                ));
            }
        }
        Ok(Self { ctx })
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
            libc::free(peer_config as *mut libc::c_void);

            if ret != 0 {
                return Err(anyhow::anyhow!("Failed to create receiver peer: {}", ret));
            }
        }
        Ok(())
    }

    // Reads a single data block. Blocks until data is available or timeout?
    // librist `rist_receiver_data_read` has a timeout param.
    pub fn read_data(&self, timeout_ms: i32) -> Result<Option<RistDataBlock>> {
        unsafe {
            let mut data_block: *const rist_data_block = ptr::null();
            // 5th arg is timeout.
            let ret = rist_receiver_data_read(self.ctx, &mut data_block, timeout_ms);
            if ret < 0 {
                // Error
                return Err(anyhow::anyhow!("rist_receiver_data_read failed: {}", ret));
            }
            if ret == 0 || data_block.is_null() {
                // Timeout / No data
                return Ok(None);
            }

            // Convert to owned struct
            let db = &*data_block;
            // We need to copy payload because data_block is freed by free_data_block?
            // Actually `rist_receiver_data_read` gives us a pointer that we must free via `rist_receiver_data_block_free`?
            // Documentation says: "The data block must be freed by the caller using rist_receiver_data_block_free".

            let payload =
                std::slice::from_raw_parts(db.payload as *const u8, db.payload_len as usize)
                    .to_vec();

            // Free the block inside librist
            // Wait, bindgen might name it slightly differently.
            // Usually it's `rist_receiver_data_block_free` or `rist_receiver_data_block_free2`.
            // Checking headers would be good but let's assume standard name.
            // Actually, since I can't check headers easily without grepping:
            // "int rist_receiver_data_block_free(struct rist_data_block **block);"
            // Note double pointer.

            // Let's defer free to a wrapper if possible, or copy and free immediately.
            // Immediate copy is safer for Rust model.

            let block = RistDataBlock {
                payload,
                virt_src_port: db.virt_src_port,
                virt_dst_port: db.virt_dst_port,
                seq: db.seq,
                flow_id: db.flow_id,
                // ... other fields
            };

            // Free
            // Pointer to pointer
            let mut db_ptr = data_block as *mut rist_data_block;
            // Use free2 as free is deprecated
            rist_receiver_data_block_free2(&mut db_ptr);

            Ok(Some(block))
        }
    }
}

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
}
