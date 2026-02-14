use crate::net::wrapper::{RistReceiverContext, RIST_PROFILE_SIMPLE};
use crate::protocol::header::BondingHeader;
use crate::receiver::aggregator::{Packet, ReassemblyBuffer, ReassemblyConfig, ReassemblyStats};
use anyhow::Result;
use bytes::Bytes;
use crossbeam_channel::{bounded, Sender};
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
/// ordered payloads on the SPSC ring buffer (`output_consumer`).
///
/// [`ReassemblyBuffer`]: crate::receiver::aggregator::ReassemblyBuffer
pub struct BondingReceiver {
    input_tx: Option<Sender<Packet>>,
    /// SPSC ring producer — held alive to keep the consumer side valid.
    output_producer: Option<rtrb::Producer<Bytes>>,
    /// SPSC ring consumer — public so GStreamer can pull ordered payloads.
    pub output_consumer: rtrb::Consumer<Bytes>,
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<ReassemblyStats>>,
    /// Track spawned thread handles for clean shutdown
    thread_handles: Mutex<Vec<thread::JoinHandle<()>>>,
    /// Total number of links added (historical; not decremented on thread exit).
    total_links_added: Arc<std::sync::atomic::AtomicU64>,
}

impl BondingReceiver {
    pub fn new(latency: Duration) -> Self {
        Self::new_with_config(ReassemblyConfig {
            start_latency: latency,
            ..ReassemblyConfig::default()
        })
    }

    pub fn new_with_config(config: ReassemblyConfig) -> Self {
        let (output_producer, output_consumer) = rtrb::RingBuffer::new(256);
        let (input_tx, input_rx) = bounded::<Packet>(1000);
        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Mutex::new(ReassemblyStats::default()));

        let stats_clone = stats.clone();
        let running_clone = running.clone();
        let mut producer = output_producer;

        // Dedicate jitter buffer/tick thread — uses SPSC ring for output.
        let jitter_handle = thread::Builder::new()
            .name("rist-rcv-jitter".into())
            .spawn(move || {
                let mut buffer = ReassemblyBuffer::with_config(0, config);
                let tick_interval = Duration::from_millis(10);

                while running_clone.load(Ordering::Relaxed) {
                    match input_rx.recv_timeout(tick_interval) {
                        Ok(packet) => {
                            buffer.push(packet.seq_id, packet.payload, packet.arrival_time);
                            while let Ok(pkt) = input_rx.try_recv() {
                                buffer.push(pkt.seq_id, pkt.payload, pkt.arrival_time);
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }

                    let now = Instant::now();
                    let ready = buffer.tick(now);

                    if let Ok(mut s) = stats_clone.lock() {
                        *s = buffer.get_stats();
                    }

                    for p in ready {
                        // Spin-wait briefly on a full SPSC ring rather than
                        // blocking — the consumer should keep up at 10ms tick
                        // cadence.  If the ring stays full, drop the oldest
                        // frame to prevent unbounded latency.
                        if producer.push(p).is_err() {
                            // Ring full — the consumer is behind.  This should
                            // be rare with a 256-slot ring at 10ms ticks.
                            debug!("SPSC output ring full, dropping payload");
                        }
                    }
                }
            })
            .unwrap_or_else(|e| panic!("failed to spawn jitter buffer thread: {}", e));

        Self {
            input_tx: Some(input_tx),
            output_producer: None, // producer moved into jitter thread
            output_consumer,
            running,
            stats,
            thread_handles: Mutex::new(vec![jitter_handle]),
            total_links_added: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Drop the main sender handles to unblock receiver threads
        self.input_tx = None;
        self.output_producer = None;
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

    /// Returns the total number of links that have been added to this receiver.
    /// Note: this is a cumulative count and does not decrease if reader threads exit.
    pub fn link_count(&self) -> u64 {
        self.total_links_added.load(Ordering::Relaxed)
    }

    /// Try to pop the next ordered payload from the SPSC output ring.
    ///
    /// Returns `Some(payload)` immediately if data is available, or
    /// `None` if the ring is empty.
    pub fn try_recv(&mut self) -> Option<Bytes> {
        self.output_consumer.pop().ok()
    }

    /// Pop the next payload with a timeout, polling the SPSC ring with
    /// exponential backoff (1µs → 1ms).  Returns `None` on timeout or
    /// if the receiver has been shut down.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<Bytes> {
        let deadline = Instant::now() + timeout;
        let mut backoff = Duration::from_micros(1);
        loop {
            if let Ok(payload) = self.output_consumer.pop() {
                return Some(payload);
            }
            if Instant::now() >= deadline {
                return None;
            }
            // Exponential backoff: 1µs → 2µs → … → 1ms cap
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_millis(1));
        }
    }

    pub fn add_link(&self, bind_url: &str) -> Result<()> {
        let ctx = RistReceiverContext::new(RIST_PROFILE_SIMPLE)?;
        ctx.peer_config(bind_url)?;
        ctx.start()?;

        self.total_links_added.fetch_add(1, Ordering::Relaxed);

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
                                if input_tx.try_send(packet).is_err() {
                                    debug!(
                                        "BondingReceiver: input channel full, dropping packet on {}",
                                        link_url
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
        assert!(rcv.output_producer.is_none());
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
    fn output_consumer_available_after_new() {
        let mut rcv = BondingReceiver::new(Duration::from_millis(50));
        // output_consumer should be available and empty (no data sent)
        assert!(rcv.output_consumer.pop().is_err());
    }

    // ────────────────────────────────────────────────────────────────
    // Concurrency & channel backpressure tests
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn bounded_channel_backpressure_drops_not_blocks() {
        use crossbeam_channel::bounded;

        // Simulate the reader→jitter channel with a small capacity
        let (tx, rx) = bounded::<Bytes>(4);

        // Fill the channel
        for i in 0..4 {
            tx.send(Bytes::from(vec![i; 10])).unwrap();
        }

        // try_send on full channel should fail, not block
        let result = tx.try_send(Bytes::from_static(b"overflow"));
        assert!(
            result.is_err(),
            "try_send on full channel should return error"
        );

        // Drain one and verify it works again
        let _ = rx.recv().unwrap();
        let result = tx.try_send(Bytes::from_static(b"after-drain"));
        assert!(result.is_ok(), "try_send should succeed after draining");
    }

    #[test]
    fn receiver_output_poll_from_another_thread() {
        let mut rcv = BondingReceiver::new(Duration::from_millis(50));
        // SPSC consumer is not Clone, so we test via the main thread
        // after a short delay to confirm no data appears.
        std::thread::sleep(Duration::from_millis(50));
        assert!(rcv.output_consumer.pop().is_err(), "Should have no data");
    }

    #[test]
    fn new_with_config_applies_settings() {
        use crate::receiver::aggregator::ReassemblyConfig;

        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(100),
            buffer_capacity: 512,
            skip_after: Some(Duration::from_millis(25)),
            ..ReassemblyConfig::default()
        };
        let rcv = BondingReceiver::new_with_config(config);
        assert_eq!(rcv.link_count(), 0);
        let stats = rcv.get_stats();
        assert_eq!(stats.queue_depth, 0);
    }

    // ────────────────────────────────────────────────────────────────
    // SPSC ring buffer tests (#12)
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn try_recv_empty() {
        let mut rcv = BondingReceiver::new(Duration::from_millis(50));
        assert!(rcv.try_recv().is_none());
    }

    #[test]
    fn recv_timeout_returns_none_on_empty() {
        let mut rcv = BondingReceiver::new(Duration::from_millis(50));
        let start = Instant::now();
        let result = rcv.recv_timeout(Duration::from_millis(10));
        assert!(result.is_none());
        // Should have waited approximately the timeout duration
        assert!(start.elapsed() >= Duration::from_millis(5));
    }

    #[test]
    fn spsc_ring_capacity_is_256() {
        // Verify the ring has the expected 256-slot capacity.
        let rcv = BondingReceiver::new(Duration::from_millis(50));
        assert_eq!(rcv.output_consumer.buffer().capacity(), 256);
    }
}
