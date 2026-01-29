use crate::net::wrapper::{RistReceiverContext, RIST_PROFILE_SIMPLE};
use crate::protocol::header::BondingHeader;
use crate::receiver::aggregator::ReassemblyBuffer;
use anyhow::Result;
use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

pub struct BondingReceiver {
    // We can have multiple receivers, or one RIST context listening on multiple peers?
    // Usually RIST is 1 context -> N peers.
    // But if we want physical separation (binding to eth0 vs wlan0), we might need multiple contexts if simple profile binds to all.
    // For now, let's support adding multiple `RistReceiverContext`s, each running in its own thread?
    // Or just one if we bind 0.0.0.0.
    // Spec says: "We do NOT use librist groups. Each LinkManager creates a standalone rist_ctx."
    // So receiver side also likely needs multiple contexts if we want per-interface statistics/control.
    buffer: Arc<Mutex<ReassemblyBuffer>>,
    output_tx: Option<Sender<Bytes>>,
    pub output_rx: Receiver<Bytes>, // Public so GStreamer can pull
    running: Arc<AtomicBool>,
}

impl BondingReceiver {
    pub fn new(latency: Duration) -> Self {
        let (output_tx, output_rx) = bounded(100);
        Self {
            buffer: Arc::new(Mutex::new(ReassemblyBuffer::new(0, latency))),
            output_tx: Some(output_tx),
            output_rx,
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Drop the main sender handle to unblock receiver if no threads are running
        self.output_tx = None;
    }

    pub fn get_stats(&self) -> crate::receiver::aggregator::ReassemblyStats {
        let buffer = self.buffer.lock().unwrap();
        buffer.get_stats()
    }

    pub fn add_link(&self, bind_url: &str) -> Result<()> {
        let ctx = RistReceiverContext::new(RIST_PROFILE_SIMPLE)?;
        // bind_url usually format "rist://@0.0.0.0:5000" or similar?
        // librist URL format:
        // rist://@<interface_ip>:<port> for receiver binding.
        // The '@' indicates listening.
        ctx.peer_config(bind_url)?;
        ctx.start()?;

        let buffer = self.buffer.clone();
        let output_tx = self
            .output_tx
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
                            let mut buf = buffer.lock().unwrap();
                            buf.push(header.seq_id, original_payload, std::time::Instant::now());

                            // Tick immediately? Or separate ticker thread?
                            // If we tick here, we do it per packet.
                            let ready = buf.tick(std::time::Instant::now());
                            drop(buf); // Release lock

                            for p in ready {
                                // Push to output
                                // block nicely
                                if let Err(_) = output_tx.send(p) {
                                    // Channel closed
                                    return;
                                }
                            }
                        } else {
                            eprintln!("BondingReceiver: Dropped packet with invalid header");
                        }
                    }
                    Ok(None) => {
                        // Timeout, run tick anyway (to handle latency expiry)
                        let mut buf = buffer.lock().unwrap();
                        let ready = buf.tick(std::time::Instant::now());
                        drop(buf);
                        for p in ready {
                            if let Err(_) = output_tx.send(p) {
                                return;
                            }
                        }
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
