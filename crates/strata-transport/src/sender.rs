//! # Sender State Machine
//!
//! Pure logic — no I/O. Accepts application data, assigns sequence numbers,
//! serializes packets, drives FEC encoding, processes ACK/NACK feedback, and
//! produces `OutputPacket`s for the bonding scheduler to dispatch across links.
//!
//! ## Responsibilities
//!
//! 1. **Packetisation**: assign sequence numbers, timestamps, fragmentation
//! 2. **FEC Encoding**: feed payloads to `FecEncoder`, emit repair packets
//! 3. **Send Pool**: keep packets in slab pool until ACKed or expired
//! 4. **ACK Processing**: advance cumulative ACK, process SACK bitmap, purge pool
//! 5. **NACK Processing**: mark packets for retransmission via `RetransmitTracker`
//! 6. **Congestion Feedback**: expose pacing rate for scheduling decisions
//!
//! The sender does NOT manage sockets, links, or timers — the bonding layer
//! owns those.

use bytes::Bytes;
use std::collections::VecDeque;
use std::time::Duration;
use quanta::Instant;

use crate::arq::RetransmitTracker;
use crate::codec::FecEncoder;
use crate::pool::{
    PacketContext, PacketHandle, PacketPool, Priority, SequenceGenerator, TimestampClock,
};
use crate::stats::SenderStats;
use crate::wire::{AckPacket, Fragment, NackPacket, Packet, PacketHeader};

// ─── Configuration ──────────────────────────────────────────────────────────

/// Sender configuration parameters.
#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// Maximum payload per packet before fragmentation (bytes).
    pub max_payload_size: usize,
    /// Packet pool capacity (number of slots).
    pub pool_capacity: usize,
    /// FEC source symbols per generation (K).
    pub fec_k: usize,
    /// FEC repair symbols per generation (R).
    pub fec_r: usize,
    /// Maximum time to keep unacked packets before expiry.
    pub packet_ttl: Duration,
    /// Maximum retransmit attempts per packet.
    pub max_retries: u8,
}

impl Default for SenderConfig {
    fn default() -> Self {
        SenderConfig {
            max_payload_size: 1200,
            pool_capacity: 4096,
            fec_k: 32,
            fec_r: 4,
            packet_ttl: Duration::from_secs(5),
            max_retries: 3,
        }
    }
}

// ─── Output Packet ──────────────────────────────────────────────────────────

/// A packet ready for the bonding scheduler to send.
#[derive(Debug, Clone)]
pub struct OutputPacket {
    /// Serialized wire-format bytes (header + payload).
    pub data: Bytes,
    /// Priority classification for scheduling.
    pub priority: Priority,
    /// Sequence number (for correlation with ACKs).
    pub sequence: u64,
    /// Whether this is a retransmission.
    pub is_retransmit: bool,
    /// Whether this is an FEC repair packet.
    pub is_fec_repair: bool,
}

// ─── Sender ─────────────────────────────────────────────────────────────────

/// Sender state machine.
pub struct Sender {
    config: SenderConfig,
    seq_gen: SequenceGenerator,
    clock: TimestampClock,
    pool: PacketPool,
    fec_encoder: FecEncoder,
    retransmit: RetransmitTracker,
    output_queue: VecDeque<OutputPacket>,
    stats: SenderStats,
    /// Maps sequence number → pool handle for ACK/retransmit lookups.
    seq_to_handle: std::collections::HashMap<u64, PacketHandle>,
}

impl Sender {
    /// Create a new sender with the given configuration.
    pub fn new(config: SenderConfig) -> Self {
        let fec_encoder = FecEncoder::new(config.fec_k, config.fec_r);
        let retransmit = RetransmitTracker::new(config.max_retries);
        let pool = PacketPool::new(config.pool_capacity);

        Sender {
            config,
            seq_gen: SequenceGenerator::new(),
            clock: TimestampClock::new(),
            pool,
            fec_encoder,
            retransmit,
            output_queue: VecDeque::new(),
            stats: SenderStats::default(),
            seq_to_handle: std::collections::HashMap::new(),
        }
    }

    /// Submit application data for transmission.
    ///
    /// The data will be packetised (fragmented if necessary), FEC-encoded,
    /// stored in the send pool, and queued as `OutputPacket`s.
    ///
    /// Returns the number of output packets queued (including FEC repairs).
    pub fn send(&mut self, data: Bytes, priority: Priority) -> usize {
        let is_keyframe = priority >= Priority::Reference;
        let is_config = priority >= Priority::Critical;

        let fragments = self.fragment(data, is_keyframe, is_config);
        let mut count = 0;

        for (payload, fragment, kf, cfg) in fragments {
            let seq = self.seq_gen.next();
            let ts = self.clock.now_us();

            // Build wire packet
            let mut header =
                PacketHeader::data(seq, ts, payload.len() as u16).with_fragment(fragment);
            if kf {
                header = header.with_keyframe();
            }
            if cfg {
                header = header.with_config();
            }

            let pkt = Packet {
                header,
                payload: payload.clone(),
            };
            let wire_bytes = pkt.encode().freeze();

            // Store in send pool
            let mut ctx = PacketContext::new(seq, ts).with_priority(priority);
            ctx.fragment = fragment;
            ctx.is_keyframe = kf;
            ctx.is_config = cfg;

            if let Some(handle) = self.pool.insert(ctx, payload.clone()) {
                self.seq_to_handle.insert(seq, handle);
            }

            // Track stats before moving payload into FEC
            self.stats.packets_sent += 1;
            self.stats.bytes_sent += payload.len() as u64;

            // Feed FEC encoder (Bytes::clone is cheap — ref-counted)
            let fec_repairs = self.fec_encoder.add_source_symbol(seq, payload.clone());
            for repair_data in fec_repairs {
                self.output_queue.push_back(OutputPacket {
                    data: repair_data,
                    priority: Priority::Standard,
                    sequence: seq,
                    is_retransmit: false,
                    is_fec_repair: true,
                });
                self.stats.fec_repairs_sent += 1;
                count += 1;
            }

            // Queue the data packet
            self.output_queue.push_back(OutputPacket {
                data: wire_bytes,
                priority,
                sequence: seq,
                is_retransmit: false,
                is_fec_repair: false,
            });
            count += 1;
        }

        count
    }

    /// Process an ACK from the receiver.
    ///
    /// Advances cumulative acknowledgment and processes SACK bitmap.
    /// Returns the number of packets newly acknowledged.
    pub fn process_ack(&mut self, ack: &AckPacket) -> usize {
        let mut newly_acked = 0;

        // Cumulative ACK — all seqs <= cumulative_seq are acknowledged
        let cum_seq = ack.cumulative_seq.value();
        let to_remove: Vec<u64> = self
            .seq_to_handle
            .keys()
            .filter(|&&s| s <= cum_seq)
            .copied()
            .collect();

        for seq in to_remove {
            if let Some(handle) = self.seq_to_handle.remove(&seq) {
                self.pool.mark_acked(handle);
                newly_acked += 1;
            }
            self.retransmit.mark_acked(seq);
        }

        // SACK bitmap — individual acks beyond cumulative
        for sack_seq in ack.sacked_sequences() {
            if let Some(handle) = self.seq_to_handle.remove(&sack_seq) {
                self.pool.mark_acked(handle);
                newly_acked += 1;
            }
            self.retransmit.mark_acked(sack_seq);
        }

        self.stats.packets_acked += newly_acked as u64;

        // Cleanup retransmit tracker below cumulative
        self.retransmit.cleanup_below(cum_seq);

        // Purge acknowledged packets from pool
        self.pool.purge_acked();

        newly_acked
    }

    /// Process a NACK from the receiver.
    ///
    /// Enqueues retransmissions for requested sequence ranges.
    /// Returns the number of retransmissions queued.
    pub fn process_nack(&mut self, nack: &NackPacket) -> usize {
        let mut retransmitted = 0;

        for range in &nack.ranges {
            let start = range.start.value();
            let count = range.count.value();

            for seq in start..(start + count) {
                if !self.retransmit.request_retransmit(seq) {
                    continue; // retry budget exhausted
                }

                // Look up the packet in the pool
                if let Some(&handle) = self.seq_to_handle.get(&seq) {
                    if let Some(entry) = self.pool.get_mut(handle) {
                        entry.context.retry_count += 1;

                        // Re-serialize the packet
                        let header = PacketHeader::data(
                            entry.context.sequence,
                            entry.context.timestamp_us,
                            entry.payload.len() as u16,
                        )
                        .with_fragment(entry.context.fragment);

                        let pkt = Packet {
                            header,
                            payload: entry.payload.clone(),
                        };

                        self.output_queue.push_back(OutputPacket {
                            data: pkt.encode().freeze(),
                            priority: entry.context.priority,
                            sequence: seq,
                            is_retransmit: true,
                            is_fec_repair: false,
                        });

                        self.stats.retransmissions += 1;
                        retransmitted += 1;
                    }
                }
            }
        }

        retransmitted
    }

    /// Drain output packets ready for the bonding scheduler.
    pub fn drain_output(&mut self) -> impl Iterator<Item = OutputPacket> + '_ {
        self.output_queue.drain(..)
    }

    /// Peek at the number of queued output packets.
    pub fn output_queue_len(&self) -> usize {
        self.output_queue.len()
    }

    /// Expire old unacked packets from the pool.
    /// Returns the number of expired packets.
    pub fn expire_old_packets(&mut self) -> usize {
        let cutoff = Instant::now() - self.config.packet_ttl;
        let expired = self.pool.drain_expired(cutoff);
        let count = expired.len();

        // Remove expired seqs from the handle map
        for entry in &expired {
            self.seq_to_handle.remove(&entry.context.sequence);
            self.retransmit.mark_acked(entry.context.sequence); // stop retransmitting
        }
        self.stats.packets_expired += count as u64;
        count
    }

    /// Flush partial FEC generation (e.g., at end of GOP or latency deadline).
    /// Returns the number of repair packets queued.
    pub fn flush_fec(&mut self) -> usize {
        let repairs = self.fec_encoder.flush();
        let count = repairs.len();
        for repair in repairs {
            self.output_queue.push_back(OutputPacket {
                data: repair,
                priority: Priority::Standard,
                sequence: 0,
                is_retransmit: false,
                is_fec_repair: true,
            });
            self.stats.fec_repairs_sent += 1;
        }
        count
    }

    /// Update FEC encoding rate (called by TAROT optimizer).
    pub fn set_fec_rate(&mut self, k: usize, r: usize) {
        self.fec_encoder.set_rate(k, r);
    }

    /// Get send pool utilization (0.0 - 1.0).
    pub fn pool_utilization(&self) -> f64 {
        self.pool.len() as f64 / self.pool.capacity() as f64
    }

    /// Get current sender statistics.
    pub fn stats(&self) -> &SenderStats {
        &self.stats
    }

    /// Get mutable access to stats (for linking with congestion metrics).
    pub fn stats_mut(&mut self) -> &mut SenderStats {
        &mut self.stats
    }

    /// Number of packets currently in the send pool (unacked).
    pub fn in_flight(&self) -> usize {
        self.pool.len()
    }

    /// Next sequence number that will be assigned.
    pub fn next_sequence(&self) -> u64 {
        self.seq_gen.current()
    }

    // ─── Internal Helpers ────────────────────────────────────────────────

    /// Fragment data into MTU-sized chunks with appropriate fragment flags.
    fn fragment(
        &self,
        data: Bytes,
        is_keyframe: bool,
        is_config: bool,
    ) -> Vec<(Bytes, Fragment, bool, bool)> {
        let max = self.config.max_payload_size;
        if data.len() <= max {
            return vec![(data, Fragment::Complete, is_keyframe, is_config)];
        }

        let mut fragments = Vec::new();
        let mut offset = 0;
        let total = data.len();

        while offset < total {
            let end = (offset + max).min(total);
            let chunk = data.slice(offset..end);
            let frag = if offset == 0 {
                Fragment::Start
            } else if end == total {
                Fragment::End
            } else {
                Fragment::Middle
            };

            // Keyframe/config flags only on first fragment
            let kf = is_keyframe && offset == 0;
            let cfg = is_config && offset == 0;
            fragments.push((chunk, frag, kf, cfg));
            offset = end;
        }

        fragments
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{NackRange, PacketType, VarInt};

    fn test_config() -> SenderConfig {
        SenderConfig {
            max_payload_size: 1200,
            pool_capacity: 256,
            fec_k: 4,
            fec_r: 1,
            packet_ttl: Duration::from_secs(5),
            max_retries: 3,
        }
    }

    // ─── Send & Output ──────────────────────────────────────────────────

    #[test]
    fn send_single_packet_queues_output() {
        let mut sender = Sender::new(test_config());
        let n = sender.send(Bytes::from(vec![0u8; 100]), Priority::Standard);

        // 1 data packet, no FEC yet (need 4 for K=4)
        assert_eq!(n, 1);
        assert_eq!(sender.output_queue_len(), 1);

        let out: Vec<_> = sender.drain_output().collect();
        assert_eq!(out.len(), 1);
        assert!(!out[0].is_retransmit);
        assert!(!out[0].is_fec_repair);
        assert_eq!(out[0].priority, Priority::Standard);
    }

    #[test]
    fn send_triggers_fec_at_k() {
        let mut sender = Sender::new(test_config()); // K=4, R=1

        // Send 3 packets — no FEC yet
        for i in 0..3 {
            sender.send(Bytes::from(vec![i; 100]), Priority::Standard);
        }
        assert_eq!(sender.output_queue_len(), 3);

        // 4th packet triggers FEC (1 repair + 1 data = 2 added)
        sender.send(Bytes::from(vec![3; 100]), Priority::Standard);
        // Queue: 3 data + 1 repair + 1 data = 5
        assert_eq!(sender.output_queue_len(), 5);

        let out: Vec<_> = sender.drain_output().collect();
        let fec_count = out.iter().filter(|o| o.is_fec_repair).count();
        assert_eq!(fec_count, 1, "should have 1 FEC repair packet");
    }

    #[test]
    fn send_assigns_monotonic_sequences() {
        let mut sender = Sender::new(test_config());
        for _ in 0..5 {
            sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        }
        let out: Vec<_> = sender.drain_output().collect();
        let seqs: Vec<u64> = out
            .iter()
            .filter(|o| !o.is_fec_repair)
            .map(|o| o.sequence)
            .collect();
        for w in seqs.windows(2) {
            assert_eq!(w[1], w[0] + 1, "sequences should be monotonic");
        }
    }

    #[test]
    fn send_stores_in_pool() {
        let mut sender = Sender::new(test_config());
        sender.send(Bytes::from(vec![0; 100]), Priority::Standard);
        assert_eq!(sender.in_flight(), 1);

        sender.send(Bytes::from(vec![1; 100]), Priority::Standard);
        assert_eq!(sender.in_flight(), 2);
    }

    // ─── Fragmentation ──────────────────────────────────────────────────

    #[test]
    fn send_fragments_large_payload() {
        let config = SenderConfig {
            max_payload_size: 100,
            ..test_config()
        };
        let mut sender = Sender::new(config);
        // 250 bytes → 3 fragments: 100 + 100 + 50
        let n = sender.send(Bytes::from(vec![0xAB; 250]), Priority::Standard);
        assert_eq!(n, 3);

        let out: Vec<_> = sender.drain_output().collect();
        assert_eq!(out.len(), 3);

        // Decode fragments and check fragment flags
        let decoded: Vec<_> = out
            .iter()
            .map(|o| Packet::decode(&mut o.data.clone()).unwrap())
            .collect();
        assert_eq!(decoded[0].header.fragment, Fragment::Start);
        assert_eq!(decoded[1].header.fragment, Fragment::Middle);
        assert_eq!(decoded[2].header.fragment, Fragment::End);
    }

    #[test]
    fn small_payload_is_complete_fragment() {
        let mut sender = Sender::new(test_config());
        sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        let out: Vec<_> = sender.drain_output().collect();
        let decoded = Packet::decode(&mut out[0].data.clone()).unwrap();
        assert_eq!(decoded.header.fragment, Fragment::Complete);
    }

    // ─── ACK Processing ─────────────────────────────────────────────────

    #[test]
    fn ack_removes_packets_from_pool() {
        let mut sender = Sender::new(test_config());
        for _ in 0..5 {
            sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        }
        sender.drain_output().for_each(drop);

        assert_eq!(sender.in_flight(), 5);

        // ACK sequences 0-2
        let ack = AckPacket {
            cumulative_seq: VarInt::from_u64(2),
            sack_bitmap: 0,
        };
        let newly_acked = sender.process_ack(&ack);
        assert_eq!(newly_acked, 3); // seqs 0, 1, 2
        assert_eq!(sender.in_flight(), 2); // seqs 3, 4 remain
    }

    #[test]
    fn sack_bitmap_acks_specific_packets() {
        let mut sender = Sender::new(test_config());
        for _ in 0..6 {
            sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        }
        sender.drain_output().for_each(drop);

        // Cumulative=1, SACK bits: bit0=seq2(no), bit1=seq3(yes), bit2=seq4(yes)
        let ack = AckPacket {
            cumulative_seq: VarInt::from_u64(1),
            sack_bitmap: 0b110, // bits 1,2 → seqs 3,4
        };
        let newly_acked = sender.process_ack(&ack);
        assert_eq!(newly_acked, 4); // seqs 0, 1, 3, 4
        assert_eq!(sender.in_flight(), 2); // seqs 2, 5 remain
    }

    #[test]
    fn ack_updates_stats() {
        let mut sender = Sender::new(test_config());
        sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        sender.drain_output().for_each(drop);

        let ack = AckPacket {
            cumulative_seq: VarInt::from_u64(0),
            sack_bitmap: 0,
        };
        sender.process_ack(&ack);
        assert_eq!(sender.stats().packets_acked, 1);
    }

    // ─── NACK Processing ────────────────────────────────────────────────

    #[test]
    fn nack_queues_retransmit() {
        let mut sender = Sender::new(test_config());
        for _ in 0..5 {
            sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        }
        sender.drain_output().for_each(drop);

        let nack = NackPacket {
            ranges: vec![NackRange {
                start: VarInt::from_u64(2),
                count: VarInt::from_u64(2), // seqs 2, 3
            }],
        };

        let retransmitted = sender.process_nack(&nack);
        assert_eq!(retransmitted, 2);
        assert_eq!(sender.output_queue_len(), 2);

        let out: Vec<_> = sender.drain_output().collect();
        assert!(out.iter().all(|o| o.is_retransmit));
        assert_eq!(out[0].sequence, 2);
        assert_eq!(out[1].sequence, 3);
    }

    #[test]
    fn nack_retry_budget_exhaustion() {
        let config = SenderConfig {
            max_retries: 1,
            ..test_config()
        };
        let mut sender = Sender::new(config);
        sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        sender.drain_output().for_each(drop);

        let nack = NackPacket {
            ranges: vec![NackRange {
                start: VarInt::from_u64(0),
                count: VarInt::from_u64(1),
            }],
        };

        // First NACK → retransmit
        assert_eq!(sender.process_nack(&nack), 1);
        sender.drain_output().for_each(drop);

        // Second NACK → budget exhausted
        assert_eq!(sender.process_nack(&nack), 0);
    }

    #[test]
    fn nack_updates_stats() {
        let mut sender = Sender::new(test_config());
        sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        sender.drain_output().for_each(drop);

        let nack = NackPacket {
            ranges: vec![NackRange {
                start: VarInt::from_u64(0),
                count: VarInt::from_u64(1),
            }],
        };
        sender.process_nack(&nack);
        assert_eq!(sender.stats().retransmissions, 1);
    }

    // ─── FEC Flush ──────────────────────────────────────────────────────

    #[test]
    fn flush_fec_emits_partial_generation() {
        let mut sender = Sender::new(test_config()); // K=4
                                                     // Send 2 packets (partial generation)
        sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        sender.send(Bytes::from(vec![1; 10]), Priority::Standard);
        sender.drain_output().for_each(drop);

        let fec_count = sender.flush_fec();
        assert_eq!(fec_count, 1); // R=1
        let out: Vec<_> = sender.drain_output().collect();
        assert!(out[0].is_fec_repair);
    }

    // ─── Pool Utilization ───────────────────────────────────────────────

    #[test]
    fn pool_utilization_calculation() {
        let config = SenderConfig {
            pool_capacity: 100,
            ..test_config()
        };
        let mut sender = Sender::new(config);
        assert!((sender.pool_utilization() - 0.0).abs() < f64::EPSILON);

        for _ in 0..10 {
            sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        }
        assert!((sender.pool_utilization() - 0.10).abs() < f64::EPSILON);
    }

    // ─── Keyframe / Config Flags ────────────────────────────────────────

    #[test]
    fn keyframe_flag_set_for_reference_priority() {
        let mut sender = Sender::new(test_config());
        sender.send(Bytes::from(vec![0; 10]), Priority::Reference);
        let out: Vec<_> = sender.drain_output().collect();
        let decoded = Packet::decode(&mut out[0].data.clone()).unwrap();
        assert!(
            decoded.header.is_keyframe,
            "Reference priority should set keyframe flag"
        );
    }

    #[test]
    fn config_flag_set_for_critical_priority() {
        let mut sender = Sender::new(test_config());
        sender.send(Bytes::from(vec![0; 10]), Priority::Critical);
        let out: Vec<_> = sender.drain_output().collect();
        let decoded = Packet::decode(&mut out[0].data.clone()).unwrap();
        assert!(
            decoded.header.is_config,
            "Critical priority should set config flag"
        );
        assert!(
            decoded.header.is_keyframe,
            "Critical priority should also set keyframe flag"
        );
    }

    // ─── Wire Format ────────────────────────────────────────────────────

    #[test]
    fn output_packets_are_valid_wire_format() {
        let mut sender = Sender::new(test_config());
        for i in 0..8u8 {
            sender.send(Bytes::from(vec![i; 50 + i as usize]), Priority::Standard);
        }
        let out: Vec<_> = sender.drain_output().collect();

        for o in &out {
            let decoded = Packet::decode(&mut o.data.clone());
            assert!(
                decoded.is_some(),
                "all output packets must decode as valid wire format"
            );
            if !o.is_fec_repair {
                let pkt = decoded.unwrap();
                assert_eq!(pkt.header.packet_type, PacketType::Data);
            }
        }
    }

    // ─── Sequence Numbering ─────────────────────────────────────────────

    #[test]
    fn next_sequence_tracks_correctly() {
        let mut sender = Sender::new(test_config());
        assert_eq!(sender.next_sequence(), 0);
        sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        assert_eq!(sender.next_sequence(), 1);
        sender.send(Bytes::from(vec![0; 10]), Priority::Standard);
        assert_eq!(sender.next_sequence(), 2);
    }
}
