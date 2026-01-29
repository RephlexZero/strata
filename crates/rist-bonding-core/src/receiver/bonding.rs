use crate::net::wrapper::{RistReceiverContext, RIST_PROFILE_SIMPLE};
use crate::protocol::header::BondingHeader;
use crate::receiver::aggregator::{Packet, ReassemblyBuffer, ReassemblyStats};
use anyhow::Result;
use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

pub struct BondingReceiver {
    // We can have multiple receivers, or one RIST context listening on multiple peers?
    // Usually RIST is 1 context -> N peers.
    // But if we want physical separation (binding to eth0 vs wlan0), we might need multiple contexts if simple profile binds to all.
    // For now, let's support adding multiple `RistReceiverContext`s, each running in its own thread?
    // Or just one if we bind 0.0.0.0.
    // Spec says: "We do NOT use librist groups. Each LinkManager creates a standalone rist_ctx."
    // So receiver side also likely needs multiple contexts if we want per-interface statistics/control.
    input_tx: Option<Sender<Packet>>,
    output_tx: Option<Sender<Bytes>>,
    pub output_rx: Receiver<Bytes>, // Public so GStreamer can pull
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<ReassemblyStats>>,
}

impl BondingReceiver {
    pub fn new(latency: Duration) -> Self {
        let (output_tx, output_rx) = bounded(100);
        let (input_tx, input_rx) = bounded::<Packet>(1000);
        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Mutex::new(ReassemblyStats::default()));

        let stats_clone = stats.clone();
        let running_clone = running.clone();
        let output_tx_clone = output_tx.clone();

        // Dedicated jitter buffer/tick thread
        thread::spawn(move || {
            let mut buffer = ReassemblyBuffer::new(0, latency);
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
        });
        Self {
            input_tx: Some(input_tx),
            output_tx: Some(output_tx),
            output_rx,
            running,
            stats,
        }
    }

    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Drop the main sender handle to unblock receiver if no threads are running
        self.input_tx = None;
        self.output_tx = None;
    }

    pub fn get_stats(&self) -> crate::receiver::aggregator::ReassemblyStats {
        self.stats.lock().unwrap().clone()
    }

    pub fn add_link(&self, bind_url: &str) -> Result<()> {
        let ctx = RistReceiverContext::new(RIST_PROFILE_SIMPLE)?;
        // bind_url usually format "rist://@0.0.0.0:5000" or similar?
        // librist URL format:
        // rist://@<interface_ip>:<port> for receiver binding.
        // The '@' indicates listening.
        ctx.peer_config(bind_url)?;
        ctx.start()?;

        let input_tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Receiver shut down"))?
            .clone();
        let running = self.running.clone();

        thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                // Read with 50ms timeout
                match ctx.read_data(50) {
                    Ok(Some(block)) => {
                        // Process Packet
                        let payload = Bytes::from(block.payload);
                        // eprintln!("Debug: Got {} bytes from librist", payload.len());

                        // Parse Header
                        if let Some((header, original_payload)) = BondingHeader::unwrap(payload) {
                            let packet = Packet {
                                seq_id: header.seq_id,
                                payload: original_payload,
                                arrival_time: Instant::now(),
                            };
                            let _ = input_tx.send(packet);
                        } else {
                            eprintln!("BondingReceiver: Dropped packet with invalid header");
                        }
                    }
                    Ok(None) => {
                        // No data; jitter thread handles tick.
                    }
                    Err(e) => {
                        // Log error
                        eprintln!("Receiver Error: {}", e);
                        // Backoff?
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });

        Ok(())
    }
}
