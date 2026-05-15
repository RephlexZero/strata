//! # Receiver State Machine
//!
//! Pure logic — no I/O. Accepts raw wire-format bytes from the network layer,
//! deserializes packets, detects loss, performs FEC recovery, reassembles
//! fragments, and delivers completed application data to the consumer.
//!
//! ## Responsibilities
//!
//! 1. **Deserialization**: decode wire packets, classify data vs control
//! 2. **Loss Detection**: feed sequences to `LossDetector`, generate NACKs
//! 3. **FEC Recovery**: feed source/repair symbols to `FecDecoder`, recover losses
//! 4. **Reordering Buffer**: hold out-of-order packets, deliver in-order
//! 5. **Fragment Reassembly**: collect fragmented payloads into complete units
//! 6. **ACK Generation**: periodically emit cumulative ACK + SACK bitmap
//!
//! The receiver does NOT manage sockets — the bonding layer feeds it raw packets.

use bytes::{BufMut, Bytes, BytesMut};
use std::collections::BTreeMap;

use crate::arq::LossDetector;
use crate::codec::FecDecoder;
use crate::pool::SequenceGenerator;
use crate::stats::ReceiverStats;
use crate::wire::{
    AckPacket, ControlBody, Fragment, NackPacket, Packet, PacketHeader, PacketType,
    PpdReportPacket, VarInt,
};

// ─── Configuration ──────────────────────────────────────────────────────────

/// Receiver configuration parameters.
#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    /// Reorder buffer capacity (max out-of-order packets to hold).
    pub reorder_capacity: usize,
    /// Maximum FEC generations to track.
    pub max_fec_generations: usize,
    /// NACK rearm interval in milliseconds.
    pub nack_rearm_ms: u64,
    /// Maximum NACK retries per sequence.
    pub max_nack_retries: u8,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        ReceiverConfig {
            reorder_capacity: 4096,
            max_fec_generations: 64,
            nack_rearm_ms: 50,
            max_nack_retries: 3,
        }
    }
}

// ─── Delivered Packet ───────────────────────────────────────────────────────

/// A packet delivered to the application layer (after reordering & reassembly).
#[derive(Debug, Clone)]
pub struct DeliveredPacket {
    /// Sequence number (of the first fragment if reassembled).
    pub sequence: u64,
    /// Microsecond timestamp from the sender.
    pub timestamp_us: u32,
    /// Reassembled payload data.
    pub payload: Bytes,
    /// Whether this was a keyframe.
    pub is_keyframe: bool,
    /// Whether this was codec config data.
    pub is_config: bool,
    /// Whether FEC was used to recover any fragment.
    pub fec_recovered: bool,
}

// ─── Receiver Events ────────────────────────────────────────────────────────

/// Events the receiver generates for the bonding/session layer.
#[derive(Debug)]
pub enum ReceiverEvent {
    /// A NACK should be sent to the sender.
    SendNack(NackPacket),
    /// An ACK should be sent to the sender.
    SendAck(AckPacket),
    /// Application data is ready for delivery.
    Deliver(DeliveredPacket),
    /// A PPD probe pair was detected — send capacity report back to sender.
    SendPpdReport(PpdReportPacket),
}

// ─── Reorder Buffer Entry ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BufferedPacket {
    header: PacketHeader,
    payload: Bytes,
    fec_recovered: bool,
}

// ─── Fragment Assembler ─────────────────────────────────────────────────────

/// Assembles fragmented packets into complete application payloads.
#[derive(Debug)]
struct FragmentAssembler {
    /// In-progress fragment chains: start_seq → (accumulated data, expected next fragment, keyframe, config).
    in_progress: BTreeMap<u64, FragmentChain>,
}

#[derive(Debug)]
struct FragmentChain {
    data: BytesMut,
    expected_next_seq: u64,
    is_keyframe: bool,
    is_config: bool,
    timestamp_us: u32,
    fec_recovered: bool,
}

impl FragmentAssembler {
    fn new() -> Self {
        FragmentAssembler {
            in_progress: BTreeMap::new(),
        }
    }

    /// Process a packet. Returns a completed payload if this fragment completes
    /// a chain, or if the packet is unfragmented.
    fn process(&mut self, pkt: &BufferedPacket) -> Option<DeliveredPacket> {
        let seq = pkt.header.sequence.value();

        match pkt.header.fragment {
            Fragment::Complete => {
                // No fragmentation — deliver immediately.
                Some(DeliveredPacket {
                    sequence: seq,
                    timestamp_us: pkt.header.timestamp_us,
                    payload: pkt.payload.clone(),
                    is_keyframe: pkt.header.is_keyframe,
                    is_config: pkt.header.is_config,
                    fec_recovered: pkt.fec_recovered,
                })
            }
            Fragment::Start => {
                // Begin a new chain
                let mut data = BytesMut::with_capacity(pkt.payload.len() * 4);
                data.put(pkt.payload.clone());
                self.in_progress.insert(
                    seq,
                    FragmentChain {
                        data,
                        expected_next_seq: seq + 1,
                        is_keyframe: pkt.header.is_keyframe,
                        is_config: pkt.header.is_config,
                        timestamp_us: pkt.header.timestamp_us,
                        fec_recovered: pkt.fec_recovered,
                    },
                );
                None
            }
            Fragment::Middle => {
                // Find the chain this belongs to
                let chain = self.find_chain_for(seq)?;
                let entry = self.in_progress.get_mut(&chain)?;
                if entry.expected_next_seq != seq {
                    return None; // Out of order within fragment chain — drop
                }
                entry.data.put(pkt.payload.clone());
                entry.expected_next_seq = seq + 1;
                entry.fec_recovered |= pkt.fec_recovered;
                None
            }
            Fragment::End => {
                // Complete the chain
                let chain = self.find_chain_for(seq)?;
                let mut entry = self.in_progress.remove(&chain)?;
                if entry.expected_next_seq != seq {
                    return None; // Missing middle fragment
                }
                entry.data.put(pkt.payload.clone());
                entry.fec_recovered |= pkt.fec_recovered;
                Some(DeliveredPacket {
                    sequence: chain,
                    timestamp_us: entry.timestamp_us,
                    payload: entry.data.freeze(),
                    is_keyframe: entry.is_keyframe,
                    is_config: entry.is_config,
                    fec_recovered: entry.fec_recovered,
                })
            }
        }
    }

    /// Find the chain start seq that this seq belongs to.
    fn find_chain_for(&self, seq: u64) -> Option<u64> {
        // Find the latest chain whose expected_next_seq == seq
        for (&start, chain) in self.in_progress.iter().rev() {
            if chain.expected_next_seq == seq {
                return Some(start);
            }
        }
        None
    }

    /// Cleanup stale chains (more than `max_gap` seqs behind current).
    fn cleanup_stale(&mut self, current_seq: u64, max_gap: u64) {
        self.in_progress
            .retain(|&start, _| current_seq.saturating_sub(start) < max_gap);
    }
}

// ─── Receiver ───────────────────────────────────────────────────────────────

/// Generation metadata learned from a FEC repair header. Lets a
/// late-arriving source packet (repair-before-source reordering) also
/// trigger a recovery attempt.
#[derive(Debug, Clone, Copy)]
struct FecGenInfo {
    base_seq: u64,
    k: u8,
    r: u8,
}

/// How many sequence numbers of received source wire-bytes to retain for
/// FEC. Must comfortably exceed the largest expected generation (K) plus
/// reordering depth so the decoder can be fed every known source symbol
/// of a generation when its repair arrives. 1024 ≈ 1.2 MB at 1200 B MTU.
const FEC_SOURCE_CACHE_CAP: usize = 1024;

/// Receiver state machine.
pub struct Receiver {
    #[allow(dead_code)]
    config: ReceiverConfig,
    loss_detector: LossDetector,
    fec_decoder: FecDecoder,
    reorder_buf: BTreeMap<u64, BufferedPacket>,
    next_deliver_seq: u64,
    assembler: FragmentAssembler,
    stats: ReceiverStats,
    #[allow(dead_code)]
    ack_seq_gen: SequenceGenerator,
    events: Vec<ReceiverEvent>,
    initialized: bool,
    /// PPD state: arrival time and wire size of the last PPD-flagged packet.
    last_ppd_arrival: Option<std::time::Instant>,
    last_ppd_wire_size: usize,
    /// Full wire bytes of recently received DATA packets, keyed by seq.
    /// Fed to the FEC decoder as known source symbols so the linear
    /// system can be solved for the missing ones. Bounded by
    /// `FEC_SOURCE_CACHE_CAP`.
    fec_source_cache: BTreeMap<u64, Bytes>,
    /// Generations for which a repair header has been seen, keyed by
    /// generation id. Used to map recovered indices back to global seqs
    /// and to retry recovery when a late source packet arrives.
    fec_generations: std::collections::HashMap<u16, FecGenInfo>,
}

impl Receiver {
    /// Create a new receiver with the given configuration.
    pub fn new(config: ReceiverConfig) -> Self {
        let loss_detector = {
            let mut d = LossDetector::new();
            d.set_rearm_interval(std::time::Duration::from_millis(config.nack_rearm_ms));
            d.set_max_nacks(config.max_nack_retries);
            d
        };
        let fec_decoder = FecDecoder::new(config.max_fec_generations);

        Receiver {
            config,
            loss_detector,
            fec_decoder,
            reorder_buf: BTreeMap::new(),
            next_deliver_seq: 0,
            assembler: FragmentAssembler::new(),
            stats: ReceiverStats::default(),
            ack_seq_gen: SequenceGenerator::new(),
            events: Vec::new(),
            initialized: false,
            last_ppd_arrival: None,
            last_ppd_wire_size: 0,
            fec_source_cache: BTreeMap::new(),
            fec_generations: std::collections::HashMap::new(),
        }
    }

    /// Process a raw wire-format packet from the network.
    ///
    /// Deserializes, updates loss detector, handles FEC repair packets,
    /// buffers for reordering, and delivers in-order packets.
    pub fn receive(&mut self, raw: Bytes) {
        // Keep the original wire bytes — for DATA packets these are cached
        // verbatim as FEC source symbols (the encoder protects the full
        // wire packet, so the decoder must be fed the same bytes).
        let mut buf = raw.clone();
        let pkt = match Packet::decode(&mut buf) {
            Some(p) => p,
            None => return, // Invalid packet — silently drop
        };

        match pkt.header.packet_type {
            PacketType::Data => self.handle_data_packet(pkt, raw),
            PacketType::Control => self.handle_control_packet(pkt),
        }
    }

    /// Process a pre-decoded data packet. `raw` is the full wire encoding
    /// (header + payload) as received, used as the FEC source symbol.
    fn handle_data_packet(&mut self, pkt: Packet, raw: Bytes) {
        let seq = pkt.header.sequence.value();

        if !self.initialized {
            self.next_deliver_seq = seq;
            self.initialized = true;
        }

        self.stats.packets_received += 1;
        self.stats.bytes_received += pkt.payload.len() as u64;

        // Check for duplicate
        if seq < self.next_deliver_seq {
            self.stats.duplicates += 1;
            return;
        }
        if self.reorder_buf.contains_key(&seq) {
            self.stats.duplicates += 1;
            return;
        }

        // Feed loss detector
        self.loss_detector.record_received(seq);

        // PPD probe pair detection: when two consecutive PPD-flagged packets
        // arrive within a short window, compute bottleneck capacity from
        // the inter-arrival dispersion.
        if pkt.header.is_ppd_probe {
            let now = std::time::Instant::now();
            // Wire size = header + payload (what the bottleneck had to transmit)
            let wire_size = pkt.header.encoded_len() + pkt.payload.len();

            if let Some(prev_arrival) = self.last_ppd_arrival {
                let dispersion = now.duration_since(prev_arrival);
                let dispersion_us = dispersion.as_micros() as u64;
                // Guard: ignore unreasonable dispersions (< 200µs or > 100ms).
                // Kernel batching (recvmmsg) can deliver both probe packets
                // in the same syscall, producing sub-100µs dispersions that
                // translate to multi-Gbps capacity estimates.  200µs minimum
                // caps the maximum measurable rate at ~48 Mbps for 1200B packets.
                if (200..=100_000).contains(&dispersion_us) {
                    let avg_size = (self.last_ppd_wire_size + wire_size) / 2;
                    let capacity_bps =
                        (avg_size as f64 * 8.0) / (dispersion_us as f64 / 1_000_000.0);
                    self.events
                        .push(ReceiverEvent::SendPpdReport(PpdReportPacket {
                            capacity_bps: capacity_bps as u64,
                            dispersion_us: dispersion_us as u32,
                            packet_size: avg_size as u16,
                        }));
                }
            }
            self.last_ppd_arrival = Some(now);
            self.last_ppd_wire_size = wire_size;
        }

        // Cache the full wire bytes as a potential FEC source symbol, then
        // bound the cache. A monotonic seq stream means dropping the lowest
        // keys evicts the oldest packets.
        self.fec_source_cache.insert(seq, raw);
        while self.fec_source_cache.len() > FEC_SOURCE_CACHE_CAP {
            let oldest = *self.fec_source_cache.keys().next().unwrap();
            self.fec_source_cache.remove(&oldest);
        }

        // Buffer for reordering
        self.reorder_buf.insert(
            seq,
            BufferedPacket {
                header: pkt.header,
                payload: pkt.payload,
                fec_recovered: false,
            },
        );

        // Try to deliver in-order packets
        self.deliver_in_order();

        // Repair-before-source reordering: if this packet falls inside a
        // generation whose repair already arrived, a fresh source symbol
        // may now make the linear system solvable.
        let pending_gen = self.fec_generations.iter().find_map(|(g, info)| {
            let end = info.base_seq + info.k as u64;
            (seq >= info.base_seq && seq < end).then_some(*g)
        });
        if let Some(gen_id) = pending_gen {
            self.attempt_fec_recovery(gen_id);
        }
    }

    /// Handle a control packet (FEC repair, etc.)
    fn handle_control_packet(&mut self, pkt: Packet) {
        let mut payload = pkt.payload;
        if let Some(ControlBody::FecRepair(fec_hdr)) = ControlBody::decode(&mut payload) {
            // Record generation geometry so we can map recovered indices
            // back to global seqs and retry on late source arrivals.
            self.fec_generations.insert(
                fec_hdr.generation_id,
                FecGenInfo {
                    base_seq: fec_hdr.base_seq,
                    k: fec_hdr.k,
                    r: fec_hdr.r,
                },
            );

            // Remaining payload is the repair data.
            self.fec_decoder
                .add_repair_symbol(&fec_hdr, payload.to_vec());

            self.attempt_fec_recovery(fec_hdr.generation_id);
        }
    }

    /// Feed every cached source symbol of `gen_id` into the decoder, run
    /// Gaussian elimination, and reinsert any recovered packets into the
    /// reorder buffer at their true global sequence numbers.
    fn attempt_fec_recovery(&mut self, gen_id: u16) {
        let info = match self.fec_generations.get(&gen_id) {
            Some(i) => *i,
            None => return,
        };
        let k = info.k as usize;
        if k == 0 {
            return;
        }

        // Feed known source symbols (full wire bytes) for this generation
        // so the decoder can reduce the linear system. `add_source` is
        // idempotent per index, so re-feeding across repair/source arrivals
        // is safe.
        for idx in 0..k {
            let seq = info.base_seq + idx as u64;
            if let Some(raw) = self.fec_source_cache.get(&seq) {
                self.fec_decoder
                    .add_source_symbol(gen_id, idx, k, info.r as usize, raw.clone());
            }
        }

        let recovered = self.fec_decoder.try_recover(gen_id);
        if recovered.is_empty() {
            return;
        }

        let mut reinserted = false;
        for (idx, data) in recovered {
            let seq = info.base_seq + idx as u64;

            // Already delivered, buffered, or directly received — nothing
            // to do. (try_recover re-reports the same symbols on every
            // call; these guards make reinsertion idempotent.)
            if seq < self.next_deliver_seq
                || self.reorder_buf.contains_key(&seq)
                || self.fec_source_cache.contains_key(&seq)
            {
                continue;
            }

            // The recovered symbol is a full wire packet (header+payload),
            // zero-padded to the generation's max symbol length.
            // `Packet::decode` reads `payload_len` from the header and
            // ignores the trailing padding, so it is self-describing.
            let mut buf = data;
            let rpkt = match Packet::decode(&mut buf) {
                Some(p) => p,
                None => continue, // corrupt recovery — drop
            };
            if rpkt.header.packet_type != PacketType::Data {
                continue;
            }
            // Sanity: the decoded seq must match the index→seq mapping.
            if rpkt.header.sequence.value() != seq {
                continue;
            }

            self.stats.fec_recoveries += 1;
            self.loss_detector.record_received(seq);
            self.reorder_buf.insert(
                seq,
                BufferedPacket {
                    header: rpkt.header,
                    payload: rpkt.payload,
                    fec_recovered: true,
                },
            );
            reinserted = true;
            tracing::trace!("FEC reinserted seq {} (generation {})", seq, gen_id);
        }

        if reinserted {
            self.deliver_in_order();
        }

        // Drop generation bookkeeping once it can no longer help: every
        // source seq is below the delivery frontier.
        if info.base_seq + k as u64 <= self.next_deliver_seq {
            self.fec_generations.remove(&gen_id);
            self.fec_decoder.remove_generation(gen_id);
        }
    }

    /// Deliver packets in sequence order from the reorder buffer.
    fn deliver_in_order(&mut self) {
        loop {
            let next = self.next_deliver_seq;
            let pkt = match self.reorder_buf.remove(&next) {
                Some(p) => p,
                None => break,
            };

            self.next_deliver_seq += 1;
            self.stats.packets_delivered += 1;
            self.stats.bytes_delivered += pkt.payload.len() as u64;

            // Reassemble fragments
            if let Some(delivered) = self.assembler.process(&pkt) {
                self.events.push(ReceiverEvent::Deliver(delivered));
            }
        }
        self.stats.highest_delivered_seq = self.next_deliver_seq;

        // Cleanup old fragment chains
        self.assembler.cleanup_stale(self.next_deliver_seq, 1000);
    }

    /// Advance `next_deliver_seq` past irrecoverable gaps so that the
    /// reorder buffer doesn't block forever on permanently lost packets.
    ///
    /// When the loss detector declares a sequence irrecoverable (NACK
    /// budget exhausted), `highest_contiguous` advances past it.  We
    /// must mirror that in `next_deliver_seq`, delivering any buffered
    /// packets along the way.
    fn skip_irrecoverable_gaps(&mut self) {
        let frontier = self.loss_detector.highest_contiguous();
        if frontier < self.next_deliver_seq {
            return;
        }
        // Walk from next_deliver_seq to frontier, delivering buffered
        // packets and skipping gaps.
        while self.next_deliver_seq <= frontier {
            let seq = self.next_deliver_seq;
            if let Some(pkt) = self.reorder_buf.remove(&seq) {
                self.stats.packets_delivered += 1;
                self.stats.bytes_delivered += pkt.payload.len() as u64;
                if let Some(delivered) = self.assembler.process(&pkt) {
                    self.events.push(ReceiverEvent::Deliver(delivered));
                }
            }
            self.next_deliver_seq += 1;
        }
        self.stats.highest_delivered_seq = self.next_deliver_seq;
        // Continue delivering any consecutive packets after the gap.
        self.deliver_in_order();
    }

    /// Generate NACKs for detected losses.
    /// Call periodically (e.g., every 10-50ms).
    pub fn generate_nacks(&mut self) -> Option<NackPacket> {
        let nack = self.loss_detector.generate_nacks();
        // Advance cumulative sequence past packets whose NACK budget is
        // exhausted.  Without this, a single unrecoverable loss early in
        // the stream permanently stalls the cumulative ACK, capping the
        // 64-bit SACK window and breaking sender-side delivery-rate
        // measurement.
        self.loss_detector.advance_past_irrecoverable();
        self.skip_irrecoverable_gaps();
        if let Some(nack) = nack {
            self.stats.nacks_sent += 1;
            self.events.push(ReceiverEvent::SendNack(nack.clone()));
            Some(nack)
        } else {
            None
        }
    }

    /// Generate an ACK packet for the current state.
    pub fn generate_ack(&mut self) -> AckPacket {
        // Advance past irrecoverable gaps before reading the cumulative
        // so that ACKs always reflect the latest recoverable frontier.
        self.loss_detector.advance_past_irrecoverable();
        self.skip_irrecoverable_gaps();

        let cum_seq = self.loss_detector.highest_contiguous();

        // Build SACK bitmap from reorder buffer
        let mut bitmap: u64 = 0;
        for &seq in self.reorder_buf.keys() {
            if seq > cum_seq && seq <= cum_seq + 64 {
                let bit = (seq - cum_seq - 1) as u32;
                bitmap |= 1u64 << bit;
            }
        }

        let ack = AckPacket {
            cumulative_seq: VarInt::from_u64(cum_seq),
            sack_bitmap: bitmap,
            total_received: VarInt::from_u64(self.loss_detector.total_received()),
        };

        self.events.push(ReceiverEvent::SendAck(ack.clone()));
        ack
    }

    /// Drain all receiver events.
    pub fn drain_events(&mut self) -> impl Iterator<Item = ReceiverEvent> + '_ {
        self.events.drain(..)
    }

    /// Peek at the number of pending events.
    pub fn pending_events(&self) -> usize {
        self.events.len()
    }

    /// Current receiver statistics.
    pub fn stats(&self) -> &ReceiverStats {
        &self.stats
    }

    /// Number of packets in the reorder buffer.
    pub fn reorder_buffer_len(&self) -> usize {
        self.reorder_buf.len()
    }

    /// The next sequence number expected for in-order delivery.
    pub fn next_expected_seq(&self) -> u64 {
        self.next_deliver_seq
    }

    /// Highest contiguous sequence received.
    pub fn highest_contiguous(&self) -> u64 {
        self.loss_detector.highest_contiguous()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::PacketHeader;

    /// Helper: build a serialized data packet.
    fn make_wire_packet(seq: u64, payload: &[u8]) -> Bytes {
        Packet::new_data(seq, seq as u32 * 1000, Bytes::copy_from_slice(payload))
            .encode()
            .freeze()
    }

    /// Helper: build a fragmented wire packet.
    fn make_fragment_packet(seq: u64, frag: Fragment, payload: &[u8], keyframe: bool) -> Bytes {
        let mut header =
            PacketHeader::data(seq, seq as u32 * 1000, payload.len() as u16).with_fragment(frag);
        if keyframe {
            header = header.with_keyframe();
        }
        let pkt = Packet {
            header,
            payload: Bytes::copy_from_slice(payload),
        };
        pkt.encode().freeze()
    }

    fn default_receiver() -> Receiver {
        Receiver::new(ReceiverConfig::default())
    }

    // ─── Basic Receive & Delivery ───────────────────────────────────────

    #[test]
    fn receive_single_packet_delivers() {
        let mut rx = default_receiver();
        rx.receive(make_wire_packet(0, b"hello"));

        let events: Vec<_> = rx.drain_events().collect();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ReceiverEvent::Deliver(d) => {
                assert_eq!(d.sequence, 0);
                assert_eq!(d.payload, &b"hello"[..]);
            }
            _ => panic!("expected Deliver event"),
        }
    }

    #[test]
    fn receive_in_order_delivers_all() {
        let mut rx = default_receiver();
        for i in 0..5 {
            rx.receive(make_wire_packet(i, &[i as u8; 10]));
        }

        let delivers: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(delivers.len(), 5);
        for (i, d) in delivers.iter().enumerate() {
            assert_eq!(d.sequence, i as u64);
        }
    }

    #[test]
    fn receive_updates_stats() {
        let mut rx = default_receiver();
        rx.receive(make_wire_packet(0, b"data"));
        assert_eq!(rx.stats().packets_received, 1);
        assert_eq!(rx.stats().bytes_received, 4);
        assert_eq!(rx.stats().packets_delivered, 1);
    }

    // ─── Reordering ─────────────────────────────────────────────────────

    #[test]
    fn out_of_order_delivered_after_gap_fills() {
        let mut rx = default_receiver();

        // Receive 0, skip 1, receive 2
        rx.receive(make_wire_packet(0, b"pkt0"));
        rx.drain_events().for_each(drop);

        rx.receive(make_wire_packet(2, b"pkt2"));
        // seq 2 should be buffered, not delivered
        let delivers: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert!(
            delivers.is_empty(),
            "should not deliver out-of-order packet"
        );
        assert_eq!(rx.reorder_buffer_len(), 1);

        // Now receive 1 — should deliver both 1 and 2
        rx.receive(make_wire_packet(1, b"pkt1"));
        let delivers: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(delivers.len(), 2);
        assert_eq!(delivers[0].sequence, 1);
        assert_eq!(delivers[1].sequence, 2);
        assert_eq!(rx.reorder_buffer_len(), 0);
    }

    // ─── Duplicate Detection ────────────────────────────────────────────

    #[test]
    fn duplicate_packet_counted() {
        let mut rx = default_receiver();
        rx.receive(make_wire_packet(0, b"data"));
        rx.drain_events().for_each(drop);

        // Same sequence again
        rx.receive(make_wire_packet(0, b"data"));
        assert_eq!(rx.stats().duplicates, 1);
    }

    #[test]
    fn duplicate_in_reorder_buffer_counted() {
        let mut rx = default_receiver();
        rx.receive(make_wire_packet(0, b"pkt0"));
        rx.drain_events().for_each(drop);

        // Skip 1, receive 2 twice
        rx.receive(make_wire_packet(2, b"pkt2"));
        rx.receive(make_wire_packet(2, b"pkt2"));
        assert_eq!(rx.stats().duplicates, 1);
    }

    // ─── ACK Generation ─────────────────────────────────────────────────

    #[test]
    fn ack_cumulative_seq() {
        let mut rx = default_receiver();
        for i in 0..5 {
            rx.receive(make_wire_packet(i, b"x"));
        }
        rx.drain_events().for_each(drop);

        let ack = rx.generate_ack();
        assert_eq!(ack.cumulative_seq.value(), 4);
    }

    #[test]
    fn ack_sack_bitmap() {
        let mut rx = default_receiver();
        rx.receive(make_wire_packet(0, b"x"));
        rx.drain_events().for_each(drop);

        // Skip 1, receive 2 and 4
        rx.receive(make_wire_packet(2, b"x"));
        rx.receive(make_wire_packet(4, b"x"));
        rx.drain_events().for_each(drop);

        let ack = rx.generate_ack();
        assert_eq!(ack.cumulative_seq.value(), 0);
        // bit 1 = seq 2 (cum+2), bit 3 = seq 4 (cum+4)
        assert!(
            ack.sack_bitmap & (1 << 1) != 0,
            "bit 1 (seq 2) should be set"
        );
        assert!(
            ack.sack_bitmap & (1 << 3) != 0,
            "bit 3 (seq 4) should be set"
        );
    }

    // ─── NACK Generation ────────────────────────────────────────────────

    #[test]
    fn nack_generated_for_gap() {
        let mut rx = Receiver::new(ReceiverConfig {
            nack_rearm_ms: 0, // instant rearm for test
            ..Default::default()
        });
        rx.receive(make_wire_packet(0, b"x"));
        rx.receive(make_wire_packet(2, b"x")); // gap at 1
        rx.drain_events().for_each(drop);

        let nack = rx.generate_nacks();
        assert!(nack.is_some());
        let nack = nack.unwrap();
        assert_eq!(nack.ranges[0].start.value(), 1);
        assert_eq!(nack.ranges[0].count.value(), 1);
    }

    #[test]
    fn nack_updates_stats() {
        let mut rx = Receiver::new(ReceiverConfig {
            nack_rearm_ms: 0,
            ..Default::default()
        });
        rx.receive(make_wire_packet(0, b"x"));
        rx.receive(make_wire_packet(2, b"x"));
        rx.drain_events().for_each(drop);

        rx.generate_nacks();
        assert_eq!(rx.stats().nacks_sent, 1);
    }

    // ─── Fragment Reassembly ────────────────────────────────────────────

    #[test]
    fn fragment_reassembly_three_pieces() {
        let mut rx = default_receiver();

        rx.receive(make_fragment_packet(0, Fragment::Start, b"AAA", true));
        let d1: Vec<_> = rx.drain_events().collect();
        assert!(d1.is_empty(), "should not deliver until End fragment");

        rx.receive(make_fragment_packet(1, Fragment::Middle, b"BBB", false));
        let d2: Vec<_> = rx.drain_events().collect();
        assert!(d2.is_empty());

        rx.receive(make_fragment_packet(2, Fragment::End, b"CCC", false));
        let delivers: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(delivers.len(), 1);
        assert_eq!(delivers[0].payload, &b"AAABBBCCC"[..]);
        assert!(
            delivers[0].is_keyframe,
            "keyframe flag from Start should propagate"
        );
        assert_eq!(delivers[0].sequence, 0);
    }

    #[test]
    fn complete_packet_delivers_immediately() {
        let mut rx = default_receiver();
        rx.receive(make_fragment_packet(0, Fragment::Complete, b"whole", false));

        let delivers: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(delivers.len(), 1);
        assert_eq!(delivers[0].payload, &b"whole"[..]);
    }

    // ─── Sequence Tracking ──────────────────────────────────────────────

    #[test]
    fn next_expected_seq_advances() {
        let mut rx = default_receiver();
        assert_eq!(rx.next_expected_seq(), 0);

        rx.receive(make_wire_packet(0, b"x"));
        assert_eq!(rx.next_expected_seq(), 1);

        rx.receive(make_wire_packet(1, b"x"));
        assert_eq!(rx.next_expected_seq(), 2);
    }

    #[test]
    fn highest_contiguous_tracks_correctly() {
        let mut rx = default_receiver();
        rx.receive(make_wire_packet(0, b"x"));
        rx.receive(make_wire_packet(1, b"x"));
        rx.receive(make_wire_packet(2, b"x"));
        assert_eq!(rx.highest_contiguous(), 2);

        // Gap at 3, receive 4
        rx.receive(make_wire_packet(4, b"x"));
        assert_eq!(rx.highest_contiguous(), 2);

        // Fill gap
        rx.receive(make_wire_packet(3, b"x"));
        assert_eq!(rx.highest_contiguous(), 4);
    }

    // ─── Invalid Packet ─────────────────────────────────────────────────

    #[test]
    fn invalid_wire_data_ignored() {
        let mut rx = default_receiver();
        rx.receive(Bytes::from_static(b"\x00\x00\x00")); // too short / invalid
        assert_eq!(rx.stats().packets_received, 0);
    }

    // ─── PPD Probe Pair Detection ───────────────────────────────────────

    /// Helper: build a PPD-flagged wire packet.
    fn make_ppd_probe_packet(seq: u64, payload: &[u8]) -> Bytes {
        let header =
            PacketHeader::data(seq, seq as u32 * 1000, payload.len() as u16).with_ppd_probe();
        let pkt = Packet {
            header,
            payload: Bytes::copy_from_slice(payload),
        };
        pkt.encode().freeze()
    }

    #[test]
    fn ppd_single_probe_no_report() {
        let mut rx = default_receiver();
        rx.receive(make_ppd_probe_packet(0, &[0u8; 1200]));

        let ppd_events: Vec<_> = rx
            .drain_events()
            .filter(|e| matches!(e, ReceiverEvent::SendPpdReport(_)))
            .collect();
        assert!(
            ppd_events.is_empty(),
            "single PPD probe should not generate a report"
        );
    }

    #[test]
    fn ppd_pair_generates_report() {
        let mut rx = default_receiver();
        let payload = vec![0u8; 1200];

        rx.receive(make_ppd_probe_packet(0, &payload));
        rx.drain_events().for_each(drop);

        // Sleep > 200μs to exceed the minimum dispersion guard
        std::thread::sleep(std::time::Duration::from_micros(300));

        rx.receive(make_ppd_probe_packet(1, &payload));
        let ppd_events: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::SendPpdReport(ppd) => Some(ppd),
                _ => None,
            })
            .collect();
        assert_eq!(
            ppd_events.len(),
            1,
            "back-to-back PPD pair should generate 1 report"
        );
        assert!(
            ppd_events[0].capacity_bps > 0,
            "capacity should be positive"
        );
        assert!(
            ppd_events[0].dispersion_us >= 200,
            "dispersion should be >= 200μs guard"
        );
        assert!(
            ppd_events[0].packet_size > 0,
            "packet_size should be positive"
        );
    }

    #[test]
    fn ppd_pair_with_normal_packet_between_still_works() {
        let mut rx = default_receiver();
        let payload = vec![0u8; 1200];

        // First PPD probe
        rx.receive(make_ppd_probe_packet(0, &payload));
        rx.drain_events().for_each(drop);

        // Normal (non-PPD) packet in between
        rx.receive(make_wire_packet(1, &payload));
        rx.drain_events().for_each(drop);

        // Sleep > 200μs to exceed the minimum dispersion guard
        std::thread::sleep(std::time::Duration::from_micros(300));

        // Second PPD probe — should still pair with the first
        rx.receive(make_ppd_probe_packet(2, &payload));
        let ppd_events: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::SendPpdReport(ppd) => Some(ppd),
                _ => None,
            })
            .collect();
        assert_eq!(
            ppd_events.len(),
            1,
            "PPD pair should work even with normal packets between"
        );
    }

    #[test]
    fn ppd_non_probe_packets_dont_trigger_report() {
        let mut rx = default_receiver();
        for i in 0..10 {
            rx.receive(make_wire_packet(i, &[0u8; 1200]));
        }
        let ppd_events: Vec<_> = rx
            .drain_events()
            .filter(|e| matches!(e, ReceiverEvent::SendPpdReport(_)))
            .collect();
        assert!(
            ppd_events.is_empty(),
            "non-PPD packets should never generate PPD reports"
        );
    }

    #[test]
    fn ppd_probe_packets_still_delivered() {
        let mut rx = default_receiver();
        rx.receive(make_ppd_probe_packet(0, b"ppd data"));

        let delivers: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(
            delivers.len(),
            1,
            "PPD probes should still be delivered as data"
        );
        assert_eq!(delivers[0].payload, &b"ppd data"[..]);
    }

    #[test]
    fn irrecoverable_gap_unblocks_delivery() {
        // Receive packets 0, 2, 3 — packet 1 is lost.
        let mut rx = Receiver::new(ReceiverConfig {
            max_nack_retries: 1,
            nack_rearm_ms: 0,
            ..ReceiverConfig::default()
        });

        rx.receive(make_wire_packet(0, b"p0"));
        rx.receive(make_wire_packet(2, b"p2"));
        rx.receive(make_wire_packet(3, b"p3"));

        // Drain initial events (delivery of p0 + possible NACKs).
        let initial: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(initial.len(), 1, "only p0 should deliver initially");
        assert_eq!(initial[0].payload, &b"p0"[..]);
        assert_eq!(rx.next_expected_seq(), 1, "blocked on missing seq 1");

        // First generate_nacks: nack_count goes 0→1, which equals
        // max_nack_retries=1, so the gap is immediately irrecoverable.
        // skip_irrecoverable_gaps then delivers p2 and p3.
        rx.generate_nacks();

        let delivered: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(delivered.len(), 2, "p2 and p3 should now be delivered");
        assert_eq!(delivered[0].payload, &b"p2"[..]);
        assert_eq!(delivered[1].payload, &b"p3"[..]);
        assert_eq!(rx.next_expected_seq(), 4);
    }

    // ─── P0 Gap-Skip Tests ─────────────────────────────────────────────

    #[test]
    fn irrecoverable_gap_multiple_bursts() {
        // Skip gap, deliver, encounter another gap, skip again.
        let mut rx = Receiver::new(ReceiverConfig {
            max_nack_retries: 1,
            nack_rearm_ms: 0,
            ..ReceiverConfig::default()
        });

        // First burst: 0 arrives, 1 lost, 2-3 arrive
        rx.receive(make_wire_packet(0, b"p0"));
        rx.receive(make_wire_packet(2, b"p2"));
        rx.receive(make_wire_packet(3, b"p3"));
        rx.drain_events().for_each(drop);

        // Skip first gap
        rx.generate_nacks();
        let d1: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(d1.len(), 2, "should deliver p2 and p3 after first gap skip");
        assert_eq!(rx.next_expected_seq(), 4);

        // Second burst: 4 arrives, 5 lost, 6-7 arrive
        rx.receive(make_wire_packet(4, b"p4"));
        rx.receive(make_wire_packet(6, b"p6"));
        rx.receive(make_wire_packet(7, b"p7"));
        rx.drain_events().for_each(drop);
        assert_eq!(rx.next_expected_seq(), 5, "blocked on missing seq 5");

        // Skip second gap
        rx.generate_nacks();
        let d2: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(
            d2.len(),
            2,
            "should deliver p6 and p7 after second gap skip"
        );
        assert_eq!(rx.next_expected_seq(), 8);
    }

    #[test]
    fn irrecoverable_gap_with_fragments() {
        // Lost middle fragment: Start arrives, Middle lost, End arrives.
        // Gap-skip should advance past the orphaned fragment chain.
        let mut rx = Receiver::new(ReceiverConfig {
            max_nack_retries: 1,
            nack_rearm_ms: 0,
            ..ReceiverConfig::default()
        });

        // seq 0: Start fragment, seq 1: Middle (lost), seq 2: End fragment
        rx.receive(make_fragment_packet(0, Fragment::Start, b"AAA", true));
        // Skip seq 1
        rx.receive(make_fragment_packet(2, Fragment::End, b"CCC", false));

        rx.drain_events().for_each(drop);
        assert_eq!(rx.next_expected_seq(), 1, "blocked on missing seq 1");

        // Skip the gap — should advance past all 3 fragment seqs
        rx.generate_nacks();

        // seq 3 should be the next expected (0 delivered as partial Start,
        // 1 was skipped, 2 delivered as End — but the fragment chain is
        // incomplete so no DeliveredPacket event for the fragment group)
        assert_eq!(
            rx.next_expected_seq(),
            3,
            "should advance past orphaned fragment chain"
        );

        // Now send a complete packet at seq 3 — should deliver fine
        rx.receive(make_wire_packet(3, b"p3"));
        let d: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(d.len(), 1, "packet after gap should deliver normally");
        assert_eq!(d[0].payload, &b"p3"[..]);
    }

    #[test]
    fn generate_ack_also_skips_gaps() {
        // Verify that generate_ack() (not just generate_nacks()) triggers
        // gap-skip deliveries.
        let mut rx = Receiver::new(ReceiverConfig {
            max_nack_retries: 1,
            nack_rearm_ms: 0,
            ..ReceiverConfig::default()
        });

        rx.receive(make_wire_packet(0, b"p0"));
        rx.receive(make_wire_packet(2, b"p2"));
        rx.receive(make_wire_packet(3, b"p3"));
        rx.drain_events().for_each(drop);

        // Exhaust NACK budget so the gap becomes irrecoverable
        rx.generate_nacks();
        rx.drain_events().for_each(drop);

        // Now receive more data: 4 arrives, 5 lost, 6 arrives
        rx.receive(make_wire_packet(4, b"p4"));
        rx.receive(make_wire_packet(6, b"p6"));
        rx.drain_events().for_each(drop);

        // Exhaust NACK budget for seq 5
        rx.generate_nacks();
        rx.drain_events().for_each(drop);

        // Now use generate_ack (not nacks) to trigger gap skip
        let ack = rx.generate_ack();

        let _d: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();
        // p6 should have been delivered via the ack-triggered gap skip
        // (p4 was already delivered when received since next_deliver_seq was at 4)
        assert!(
            ack.cumulative_seq.value() >= 6,
            "ACK cumulative should advance past gap: got {}",
            ack.cumulative_seq.value()
        );
    }

    #[test]
    fn delivery_after_gap_updates_stats() {
        let mut rx = Receiver::new(ReceiverConfig {
            max_nack_retries: 1,
            nack_rearm_ms: 0,
            ..ReceiverConfig::default()
        });

        // Send 10 packets, lose 2 (seq 3 and 7)
        for i in 0..10u64 {
            if i != 3 && i != 7 {
                rx.receive(make_wire_packet(i, &[i as u8; 100]));
            }
        }
        rx.drain_events().for_each(drop);

        // Initially: delivered 0,1,2 (blocked at 3)
        assert_eq!(rx.stats().packets_delivered, 3);
        assert_eq!(rx.stats().bytes_delivered, 300);

        // Skip gaps
        rx.generate_nacks();
        rx.drain_events().for_each(drop);

        // After gap skip: delivered 0-9 minus 3 and 7 = 8 packets
        assert_eq!(rx.stats().packets_delivered, 8);
        assert_eq!(rx.stats().bytes_delivered, 800);
        assert_eq!(rx.next_expected_seq(), 10);
    }

    #[test]
    fn high_loss_burst_then_recovery() {
        // 50% of a 100-packet burst is lost, rest arrives.
        let mut rx = Receiver::new(ReceiverConfig {
            max_nack_retries: 1,
            nack_rearm_ms: 0,
            ..ReceiverConfig::default()
        });

        // Send even-numbered packets only (odd are "lost")
        for i in 0..100u64 {
            if i % 2 == 0 {
                rx.receive(make_wire_packet(i, &[i as u8; 50]));
            }
        }
        rx.drain_events().for_each(drop);

        // Only seq 0 delivered initially (blocked at 1)
        assert_eq!(rx.stats().packets_delivered, 1);

        // Skip all gaps
        rx.generate_nacks();
        rx.drain_events().for_each(drop);

        // All 50 received packets should now be delivered.
        // next_deliver_seq advances to the highest contiguous after gap-skip.
        // The ceiling for NACKs is the highest out-of-order seq (98), so
        // seq 99 is never NACKed (nothing above it to detect it as a gap).
        assert_eq!(rx.stats().packets_delivered, 50);
        assert_eq!(rx.next_expected_seq(), 99);

        // Now send recovery packets 100-109 (all arrive).
        // Seq 99 (lost, odd) is now detectable as a gap.
        for i in 100..110u64 {
            rx.receive(make_wire_packet(i, &[i as u8; 50]));
        }
        rx.drain_events().for_each(drop);

        // Seq 99 gap blocks delivery — trigger gap-skip
        rx.generate_nacks();
        rx.drain_events().for_each(drop);

        // All recovery packets plus the skip past 99 should deliver
        assert_eq!(rx.stats().packets_delivered, 60);
        assert_eq!(rx.next_expected_seq(), 110);
    }

    // ─── End-to-End FEC Reinsertion ─────────────────────────────────────

    /// A source packet lost on the wire but covered by FEC repair must be
    /// recovered AND actually delivered to the application — not merely
    /// counted. Drives the real `Sender` so the encoding matches
    /// production exactly.
    #[test]
    fn fec_recovers_and_delivers_lost_source_packet() {
        use crate::pool::Priority;
        use crate::sender::{Sender, SenderConfig};

        // Small generation so one send() fills it and emits repairs.
        let mut tx = Sender::new(SenderConfig {
            fec_k: 8,
            fec_r: 4,
            ..SenderConfig::default()
        });

        // 8 distinct unfragmented payloads → one full generation (seqs 0..8)
        // plus 4 repair symbols.
        for i in 0..8u64 {
            tx.send(Bytes::from(vec![i as u8 + 1; 200]), Priority::Standard);
        }
        let outputs: Vec<_> = tx.drain_output().collect();

        let repairs = outputs.iter().filter(|o| o.is_fec_repair).count();
        assert!(repairs >= 1, "sender should emit FEC repair packets");

        let mut rx = default_receiver();

        // Deliver everything except source seq 3, which we "lose" on the
        // wire. Repairs are delivered so FEC can reconstruct it.
        const LOST_SEQ: u64 = 3;
        for o in &outputs {
            if !o.is_fec_repair && o.sequence == LOST_SEQ {
                continue;
            }
            rx.receive(o.data.clone());
        }

        let delivered: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();

        // The lost packet must be recovered via FEC and delivered with the
        // correct payload and the fec_recovered flag set.
        assert!(
            rx.stats().fec_recoveries >= 1,
            "expected at least one FEC recovery, got {}",
            rx.stats().fec_recoveries
        );
        let lost = delivered
            .iter()
            .find(|d| d.sequence == LOST_SEQ)
            .expect("lost seq 3 should be recovered and delivered");
        assert_eq!(
            lost.payload,
            &vec![LOST_SEQ as u8 + 1; 200][..],
            "recovered payload must match the original"
        );
        assert!(
            lost.fec_recovered,
            "delivered packet must be flagged fec_recovered"
        );
        // All 8 source packets ultimately delivered, in order.
        assert_eq!(rx.next_expected_seq(), 8, "all 8 sources delivered");
    }

    /// FEC must still recover when the repair arrives BEFORE the surviving
    /// source packets (reordering across links is common on cellular).
    #[test]
    fn fec_recovers_when_repair_arrives_before_sources() {
        use crate::pool::Priority;
        use crate::sender::{Sender, SenderConfig};

        let mut tx = Sender::new(SenderConfig {
            fec_k: 6,
            fec_r: 3,
            ..SenderConfig::default()
        });
        for i in 0..6u64 {
            tx.send(Bytes::from(vec![i as u8 + 10; 150]), Priority::Standard);
        }
        let outputs: Vec<_> = tx.drain_output().collect();

        let mut rx = default_receiver();
        const LOST_SEQ: u64 = 2;

        // Repairs first…
        for o in outputs.iter().filter(|o| o.is_fec_repair) {
            rx.receive(o.data.clone());
        }
        // …then the surviving sources (seq 2 stays lost).
        for o in outputs.iter().filter(|o| !o.is_fec_repair) {
            if o.sequence == LOST_SEQ {
                continue;
            }
            rx.receive(o.data.clone());
        }

        let delivered: Vec<_> = rx
            .drain_events()
            .filter_map(|e| match e {
                ReceiverEvent::Deliver(d) => Some(d),
                _ => None,
            })
            .collect();

        assert!(
            rx.stats().fec_recoveries >= 1,
            "repair-before-source ordering must still recover"
        );
        let lost = delivered
            .iter()
            .find(|d| d.sequence == LOST_SEQ)
            .expect("seq 2 should be recovered despite repair-first ordering");
        assert_eq!(lost.payload, &vec![LOST_SEQ as u8 + 10; 150][..]);
        assert_eq!(rx.next_expected_seq(), 6);
    }
}
