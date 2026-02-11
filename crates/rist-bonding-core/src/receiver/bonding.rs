use crate::net::wrapper::{RistReceiverContext, RIST_PROFILE_SIMPLE};
use crate::protocol::header::BondingHeader;
use crate::receiver::aggregator::{Packet, ReassemblyBuffer, ReassemblyConfig, ReassemblyStats};
use anyhow::Result;
use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Multi-link RIST receiver with jitter-buffer reassembly.
///
/// Spawns per-link reader threads that feed received packets (after
/// stripping the bonding header) into a shared [`ReassemblyBuffer`].
/// A dedicated jitter-buffer thread ticks the buffer and emits
/// ordered payloads on `output_rx`.
///
/// [`ReassemblyBuffer`]: crate::receiver::aggregator::ReassemblyBuffer
pub struct BondingReceiver {
    input_tx: Option<Sender<Packet>>,
    output_tx: Option<Sender<Bytes>>,
    pub output_rx: Receiver<Bytes>, // Public so GStreamer can pull
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<ReassemblyStats>>,
    /// Track spawned thread handles for clean shutdown
    thread_handles: Mutex<Vec<thread::JoinHandle<()>>>,
    /// Number of links added (each `add_link` call increments this).
    link_count: Arc<std::sync::atomic::AtomicU64>,
}

impl BondingReceiver {
    pub fn new(latency: Duration) -> Self {
        Self::new_with_config(ReassemblyConfig {
            start_latency: latency,
            ..ReassemblyConfig::default()
        })
    }

    pub fn new_with_config(config: ReassemblyConfig) -> Self {
        let (output_tx, output_rx) = bounded(100);
        let (input_tx, input_rx) = bounded::<Packet>(1000);
        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Mutex::new(ReassemblyStats::default()));

        let stats_clone = stats.clone();
        let running_clone = running.clone();
        let output_tx_clone = output_tx.clone();

        // Dedicated jitter buffer/tick thread
        let jitter_handle = thread::Builder::new()
            .name("rist-rcv-jitter".into())
            .spawn(move || {
                let mut buffer = ReassemblyBuffer::with_config(0, config);
                let tick_interval = Duration::from_millis(10);

                while running_clone.load(Ordering::Relaxed) {
                    match input_rx.recv_timeout(tick_interval) {
                        Ok(packet) => {
                            buffer.push(packet.seq_id, packet.payload, packet.arrival_time);
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            // No packet; still tick below
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }

                    let now = Instant::now();
                    let ready = buffer.tick(now);

                    if let Ok(mut s) = stats_clone.lock() {
                        *s = buffer.get_stats();
                    }

                    for p in ready {
                        if output_tx_clone.send(p).is_err() {
                            return;
                        }
                    }
                }
            })
            .expect("failed to spawn jitter buffer thread");

        Self {
            input_tx: Some(input_tx),
            output_tx: Some(output_tx),
            output_rx,
            running,
            stats,
            thread_handles: Mutex::new(vec![jitter_handle]),
            link_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Drop the main sender handles to unblock receiver threads
        self.input_tx = None;
        self.output_tx = None;
        // Join all spawned threads for clean shutdown
        if let Ok(mut handles) = self.thread_handles.lock() {
            for handle in handles.drain(..) {
                let _ = handle.join();
            }
        }
    }

    pub fn get_stats(&self) -> crate::receiver::aggregator::ReassemblyStats {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Returns the number of links that have been added to this receiver.
    pub fn link_count(&self) -> u64 {
        self.link_count.load(Ordering::Relaxed)
    }

    pub fn add_link(&self, bind_url: &str) -> Result<()> {
        let ctx = RistReceiverContext::new(RIST_PROFILE_SIMPLE)?;
        ctx.peer_config(bind_url)?;
        ctx.start()?;

        self.link_count.fetch_add(1, Ordering::Relaxed);

        let input_tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Receiver shut down"))?
            .clone();
        let running = self.running.clone();
        let link_url = bind_url.to_string();

        let handle = thread::Builder::new()
            .name(format!("rist-rcv-{}", bind_url))
            .spawn(move || {
                while running.load(Ordering::Relaxed) {
                    match ctx.read_data(50) {
                        Ok(Some(block)) => {
                            let payload = Bytes::from(block.payload);

                            if let Some((header, original_payload)) = BondingHeader::unwrap(payload)
                            {
                                let packet = Packet {
                                    seq_id: header.seq_id,
                                    payload: original_payload,
                                    arrival_time: Instant::now(),
                                };
                                if let Err(e) = input_tx.send(packet) {
                                    debug!(
                                        "BondingReceiver: input channel full/closed on {}: {}",
                                        link_url, e
                                    );
                                }
                            } else {
                                debug!(
                                    "BondingReceiver: Dropped packet with invalid header on {}",
                                    link_url
                                );
                            }
                        }
                        Ok(None) => {
                            // No data; jitter thread handles tick.
                        }
                        Err(e) => {
                            warn!("Receiver error on {}: {}", link_url, e);
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("Failed to spawn receiver thread: {}", e))?;

        if let Ok(mut handles) = self.thread_handles.lock() {
            handles.push(handle);
        }

        Ok(())
    }
}

impl Drop for BondingReceiver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn new_and_shutdown() {
        let mut rcv = BondingReceiver::new(Duration::from_millis(50));
        assert!(rcv.input_tx.is_some());
        rcv.shutdown();
        assert!(rcv.input_tx.is_none());
        assert!(rcv.output_tx.is_none());
    }

    #[test]
    fn shutdown_is_idempotent() {
        let mut rcv = BondingReceiver::new(Duration::from_millis(50));
        rcv.shutdown();
        rcv.shutdown(); // must not panic
    }

    #[test]
    fn add_link_after_shutdown_fails() {
        let mut rcv = BondingReceiver::new(Duration::from_millis(50));
        rcv.shutdown();
        let result = rcv.add_link("rist://127.0.0.1:9999");
        assert!(result.is_err(), "add_link after shutdown should fail");
    }

    #[test]
    fn get_stats_returns_defaults() {
        let rcv = BondingReceiver::new(Duration::from_millis(50));
        let stats = rcv.get_stats();
        assert_eq!(stats.queue_depth, 0);
        assert_eq!(stats.lost_packets, 0);
        assert_eq!(stats.late_packets, 0);
        assert_eq!(stats.duplicate_packets, 0);
    }

    #[test]
    fn drop_triggers_shutdown() {
        let rcv = BondingReceiver::new(Duration::from_millis(50));
        assert!(rcv.running.load(Ordering::Relaxed));
        let running = rcv.running.clone();
        drop(rcv);
        assert!(!running.load(Ordering::Relaxed));
    }

    #[test]
    fn output_rx_available_after_new() {
        let rcv = BondingReceiver::new(Duration::from_millis(50));
        // output_rx should be available and empty (no data sent)
        assert!(rcv.output_rx.try_recv().is_err());
    }
}
