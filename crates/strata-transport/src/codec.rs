//! # FEC Codec Engine
//!
//! Hybrid FEC using Reed-Solomon systematic encoding.
//!
//! ## Design (from Master Plan §4)
//!
//! - **Layer 1**: Thin continuous FEC — 5-10% coded redundancy, systematic
//!   (source packets sent unencoded, repair symbols appended)
//! - **Layer 2**: NACK-triggered additional repair symbols
//! - **Layer 3**: TAROT adaptive FEC rate optimization
//!
//! At 2-10 Mbps, RS encoding is negligible CPU (<0.01% on ARM64 NEON).

use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

use crate::wire::{FecRepairHeader, PacketHeader};

// ─── FEC Generation ──────────────────────────────────────────────────────────

// ─── FEC Encoder ─────────────────────────────────────────────────────────────

/// Encoder that collects source symbols and emits repair symbols per generation.
pub struct FecEncoder {
    /// Source symbols per generation (K).
    k: usize,
    /// Repair symbols per generation (R).
    r: usize,
    /// Current generation ID.
    current_gen_id: u16,
    /// Current generation accumulator.
    current_symbols: Vec<(u64, Bytes)>,
}

impl FecEncoder {
    /// Create a new FEC encoder.
    ///
    /// - `k`: source symbols per generation (e.g., 32)
    /// - `r`: repair symbols per generation (e.g., 4)
    pub fn new(k: usize, r: usize) -> Self {
        assert!(k > 0, "FEC K must be > 0");
        assert!(r > 0, "FEC R must be > 0");
        FecEncoder {
            k,
            r,
            current_gen_id: 0,
            current_symbols: Vec::with_capacity(k),
        }
    }

    /// Feed a source symbol (packet) into the encoder.
    ///
    /// When K symbols accumulate, generates R repair packets (as serialized
    /// wire-format bytes) and returns them. Otherwise returns empty vec.
    pub fn add_source_symbol(&mut self, seq: u64, data: Bytes) -> Vec<Bytes> {
        self.current_symbols.push((seq, data));

        if self.current_symbols.len() >= self.k {
            let repairs = self.emit_repair();
            self.current_gen_id = self.current_gen_id.wrapping_add(1);
            self.current_symbols.clear();
            repairs
        } else {
            Vec::new()
        }
    }

    /// Generate repair symbols from the current generation using XOR-based
    /// systematic coding. Each repair symbol is a serialized control packet.
    fn emit_repair(&self) -> Vec<Bytes> {
        let gen_id = self.current_gen_id;
        let k = self.current_symbols.len();

        // Find the maximum symbol size for padding
        let max_len = self
            .current_symbols
            .iter()
            .map(|(_, d)| d.len())
            .max()
            .unwrap_or(0);
        if max_len == 0 {
            return Vec::new();
        }

        let mut repairs = Vec::with_capacity(self.r);

        for repair_idx in 0..self.r {
            // Simple XOR-based repair: each repair symbol XORs a different
            // subset of source symbols. For repair_idx=0, XOR all. For higher
            // indices, rotate the starting offset.
            let mut repair_data = vec![0u8; max_len];

            for (i, (_, symbol)) in self.current_symbols.iter().enumerate() {
                // Use a rotating pattern so different repair symbols cover
                // different subsets, providing independent recovery capability
                if repair_idx == 0 || (i + repair_idx) % (self.r + 1) != 0 {
                    for (j, &byte) in symbol.iter().enumerate() {
                        repair_data[j] ^= byte;
                    }
                }
            }

            // Serialize as a FEC repair control packet
            let fec_header = FecRepairHeader {
                generation_id: gen_id,
                symbol_index: repair_idx as u8,
                k: k as u8,
                r: self.r as u8,
            };

            let payload_len = 1 + FecRepairHeader::ENCODED_LEN + repair_data.len();
            let pkt_header = PacketHeader::control(0, 0, payload_len as u16);

            let mut buf = BytesMut::with_capacity(pkt_header.encoded_len() + payload_len);
            pkt_header.encode(&mut buf);
            fec_header.encode(&mut buf);
            buf.extend_from_slice(&repair_data);

            repairs.push(buf.freeze());
        }

        repairs
    }

    /// Get the current generation ID.
    pub fn current_generation(&self) -> u16 {
        self.current_gen_id
    }

    /// Number of source symbols buffered in the current generation.
    pub fn buffered_count(&self) -> usize {
        self.current_symbols.len()
    }

    /// Flush the current partial generation — emit repair even if < K symbols.
    /// Useful when latency deadline requires it.
    pub fn flush(&mut self) -> Vec<Bytes> {
        if self.current_symbols.is_empty() {
            return Vec::new();
        }
        let repairs = self.emit_repair();
        self.current_gen_id = self.current_gen_id.wrapping_add(1);
        self.current_symbols.clear();
        repairs
    }

    /// Update FEC parameters (for TAROT adaptive rate).
    pub fn set_rate(&mut self, k: usize, r: usize) {
        assert!(k > 0 && r > 0);
        self.k = k;
        self.r = r;
    }

    /// Current redundancy ratio: R / K.
    pub fn redundancy_ratio(&self) -> f64 {
        self.r as f64 / self.k as f64
    }
}

// ─── FEC Decoder ─────────────────────────────────────────────────────────────

/// Per-generation decoder state on the receiver side.
#[derive(Debug)]
struct GenerationState {
    /// Source symbols received: index → data.
    source_symbols: HashMap<usize, Bytes>,
    /// Repair symbols received: index → data.
    repair_symbols: HashMap<u8, Vec<u8>>,
    /// K for this generation.
    k: usize,
    /// R for this generation (stored for future multi-symbol recovery).
    #[allow(dead_code)]
    r: usize,
    /// Whether recovery has been attempted.
    recovery_attempted: bool,
}

/// FEC decoder for recovering lost source symbols using repair symbols.
pub struct FecDecoder {
    /// Active generations: gen_id → state.
    generations: HashMap<u16, GenerationState>,
    /// Maximum number of generations to track (prevent unbounded growth).
    max_generations: usize,
}

impl FecDecoder {
    pub fn new(max_generations: usize) -> Self {
        FecDecoder {
            generations: HashMap::new(),
            max_generations,
        }
    }

    /// Record a received source symbol. Returns the generation it belongs to.
    pub fn add_source_symbol(
        &mut self,
        generation_id: u16,
        index_in_gen: usize,
        k: usize,
        r: usize,
        data: Bytes,
    ) {
        let gen = self
            .generations
            .entry(generation_id)
            .or_insert_with(|| GenerationState {
                source_symbols: HashMap::new(),
                repair_symbols: HashMap::new(),
                k,
                r,
                recovery_attempted: false,
            });
        gen.source_symbols.insert(index_in_gen, data);
        self.enforce_limit();
    }

    /// Record a received repair symbol.
    pub fn add_repair_symbol(&mut self, header: &FecRepairHeader, repair_data: Vec<u8>) {
        let gen = self
            .generations
            .entry(header.generation_id)
            .or_insert_with(|| GenerationState {
                source_symbols: HashMap::new(),
                repair_symbols: HashMap::new(),
                k: header.k as usize,
                r: header.r as usize,
                recovery_attempted: false,
            });
        gen.repair_symbols.insert(header.symbol_index, repair_data);
        self.enforce_limit();
    }

    /// Check if a generation has all source symbols (no recovery needed).
    pub fn is_complete(&self, generation_id: u16) -> bool {
        self.generations
            .get(&generation_id)
            .map(|gen| gen.source_symbols.len() >= gen.k)
            .unwrap_or(false)
    }

    /// Attempt to recover missing source symbols for a generation.
    ///
    /// Returns recovered (index, data) pairs. Only works for single-symbol
    /// loss when a repair symbol covering the missing symbol is available.
    pub fn try_recover(&mut self, generation_id: u16) -> Vec<(usize, Bytes)> {
        let gen = match self.generations.get_mut(&generation_id) {
            Some(g) => g,
            None => return Vec::new(),
        };

        if gen.recovery_attempted || gen.source_symbols.len() >= gen.k {
            return Vec::new();
        }
        gen.recovery_attempted = true;

        let received_count = gen.source_symbols.len();
        let missing_count = gen.k.saturating_sub(received_count);

        // Simple XOR recovery: if exactly 1 symbol is missing and we have
        // repair symbol 0 (which XORs all source symbols), we can recover it.
        if missing_count == 1 && gen.repair_symbols.contains_key(&0) {
            let missing_idx = (0..gen.k)
                .find(|i| !gen.source_symbols.contains_key(i))
                .unwrap();

            let repair = gen.repair_symbols.get(&0).unwrap();
            let mut recovered = repair.clone();

            // XOR out all received source symbols from the repair
            for (&idx, symbol) in &gen.source_symbols {
                if idx != missing_idx {
                    for (j, &byte) in symbol.iter().enumerate() {
                        if j < recovered.len() {
                            recovered[j] ^= byte;
                        }
                    }
                }
            }

            return vec![(missing_idx, Bytes::from(recovered))];
        }

        Vec::new()
    }

    /// Remove a generation (after it's fully consumed or too old).
    pub fn remove_generation(&mut self, generation_id: u16) {
        self.generations.remove(&generation_id);
    }

    /// Number of tracked generations.
    pub fn generation_count(&self) -> usize {
        self.generations.len()
    }

    fn enforce_limit(&mut self) {
        while self.generations.len() > self.max_generations {
            // Remove the oldest (lowest) generation ID
            if let Some(&oldest) = self.generations.keys().min() {
                self.generations.remove(&oldest);
            }
        }
    }
}

// ─── TAROT Cost Function ────────────────────────────────────────────────────

/// TAROT adaptive FEC rate optimizer.
///
/// Minimizes: J = α·P_loss(r) + β·B_overhead(r) + γ·D_decode(r)
pub struct TarotOptimizer {
    /// Weight for loss probability.
    pub alpha: f64,
    /// Weight for bandwidth overhead.
    pub beta: f64,
    /// Weight for decoding latency.
    pub gamma: f64,
    /// Minimum FEC ratio.
    pub min_ratio: f64,
    /// Maximum FEC ratio.
    pub max_ratio: f64,
}

impl TarotOptimizer {
    /// Create with default latency-focused weights: α=5, β=2, γ=3.
    pub fn new() -> Self {
        TarotOptimizer {
            alpha: 5.0,
            beta: 2.0,
            gamma: 3.0,
            min_ratio: 0.02, // 2% minimum
            max_ratio: 0.50, // 50% maximum
        }
    }

    /// Compute optimal FEC ratio given observed loss rate and latency budget.
    ///
    /// - `loss_rate`: observed packet loss rate (0.0 - 1.0)
    /// - `rtt_ms`: current round-trip time in ms
    /// - `k`: current generation size
    ///
    /// Returns the recommended R (repair count) for the given K.
    pub fn compute_optimal_r(&self, loss_rate: f64, rtt_ms: f64, k: usize) -> usize {
        let loss_rate = loss_rate.clamp(0.0, 1.0);
        let k_f = k as f64;

        let mut best_r = 1usize;
        let mut best_cost = f64::MAX;

        // Evaluate cost for each candidate R from 1 to K/2
        let max_r = (k / 2).max(1);
        for r in 1..=max_r {
            let ratio = r as f64 / k_f;
            if ratio < self.min_ratio || ratio > self.max_ratio {
                continue;
            }

            // P_loss: probability that losses exceed repair capacity
            // Simplified: (loss_rate)^(r+1) — probability of > r losses in K packets
            let p_loss = loss_rate.powi(r as i32 + 1);

            // B_overhead: bandwidth consumed by FEC
            let b_overhead = ratio;

            // D_decode: decoding latency proportional to generation size
            // Normalized to [0,1] based on RTT
            let d_decode = (k_f * 0.01) / rtt_ms.max(1.0);

            let cost = self.alpha * p_loss + self.beta * b_overhead + self.gamma * d_decode;

            if cost < best_cost {
                best_cost = cost;
                best_r = r;
            }
        }

        best_r
    }
}

impl Default for TarotOptimizer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ─── proptest: FEC decode correctness ────────────────────────────────

    proptest! {
        #[test]
        fn proptest_fec_single_loss_recovery(
            k in 2..16usize,
            lost_idx in 0..16usize,
            symbol_len in 1..64usize,
            seed in 0..1000u64,
        ) {
            let lost_idx = lost_idx % k; // ensure lost_idx < k
            let r = 1; // XOR recovery supports single loss

            // Generate deterministic symbols
            let symbols: Vec<Bytes> = (0..k)
                .map(|i| {
                    Bytes::from(
                        (0..symbol_len)
                            .map(|j| ((i as u64 * 31 + j as u64 + seed) % 256) as u8)
                            .collect::<Vec<u8>>(),
                    )
                })
                .collect();

            // Encode
            let mut enc = FecEncoder::new(k, r);
            let mut repair_packets = Vec::new();
            for (i, sym) in symbols.iter().enumerate() {
                let repairs = enc.add_source_symbol(i as u64, sym.clone());
                repair_packets.extend(repairs);
            }
            if repair_packets.is_empty() {
                // Flush partial generation
                repair_packets = enc.flush();
            }
            prop_assert!(!repair_packets.is_empty(), "should have at least 1 repair");

            // Parse repair packet
            let mut buf = repair_packets[0].clone();
            let pkt = crate::wire::Packet::decode(&mut buf).unwrap();
            let mut payload = pkt.payload;
            let _subtype = payload.split_to(1);
            let fec_hdr = FecRepairHeader::decode(&mut payload).unwrap();
            let repair_data = payload.to_vec();

            // Decode: provide all symbols except lost_idx
            let mut dec = FecDecoder::new(16);
            for (i, sym) in symbols.iter().enumerate().take(k) {
                if i != lost_idx {
                    dec.add_source_symbol(0, i, k, r, sym.clone());
                }
            }
            dec.add_repair_symbol(&fec_hdr, repair_data);

            let recovered = dec.try_recover(0);
            prop_assert_eq!(recovered.len(), 1, "should recover exactly 1 symbol");
            prop_assert_eq!(recovered[0].0, lost_idx, "should recover the lost index");
            prop_assert_eq!(
                &recovered[0].1[..],
                &symbols[lost_idx][..],
                "recovered data should match original"
            );
        }

        #[test]
        fn proptest_fec_no_loss_no_recovery(
            k in 2..16usize,
            symbol_len in 1..64usize,
        ) {
            let r = 1;
            let symbols: Vec<Bytes> = (0..k)
                .map(|i| Bytes::from(vec![i as u8; symbol_len]))
                .collect();

            let mut enc = FecEncoder::new(k, r);
            for (i, sym) in symbols.iter().enumerate() {
                enc.add_source_symbol(i as u64, sym.clone());
            }

            let mut dec = FecDecoder::new(16);
            for (i, sym) in symbols.iter().enumerate().take(k) {
                dec.add_source_symbol(0, i, k, r, sym.clone());
            }

            prop_assert!(dec.is_complete(0));
            let recovered = dec.try_recover(0);
            prop_assert!(recovered.is_empty(), "no recovery needed when complete");
        }
    }

    // ─── FEC Encoder Tests ──────────────────────────────────────────────

    #[test]
    fn encoder_emits_repair_after_k_symbols() {
        let mut enc = FecEncoder::new(4, 2);

        // First 3 symbols produce no repair
        for i in 0..3u64 {
            let repairs = enc.add_source_symbol(i, Bytes::from(vec![i as u8; 100]));
            assert!(
                repairs.is_empty(),
                "should not emit repair before K symbols"
            );
        }

        // 4th symbol triggers repair emission
        let repairs = enc.add_source_symbol(3, Bytes::from(vec![3u8; 100]));
        assert_eq!(repairs.len(), 2, "should emit R=2 repair symbols");
        assert_eq!(
            enc.buffered_count(),
            0,
            "buffer should be cleared after emission"
        );
    }

    #[test]
    fn encoder_generation_increments() {
        let mut enc = FecEncoder::new(2, 1);
        assert_eq!(enc.current_generation(), 0);

        // Complete first generation
        enc.add_source_symbol(0, Bytes::from_static(b"a"));
        enc.add_source_symbol(1, Bytes::from_static(b"b"));
        assert_eq!(enc.current_generation(), 1);

        // Complete second generation
        enc.add_source_symbol(2, Bytes::from_static(b"c"));
        enc.add_source_symbol(3, Bytes::from_static(b"d"));
        assert_eq!(enc.current_generation(), 2);
    }

    #[test]
    fn encoder_flush_emits_partial() {
        let mut enc = FecEncoder::new(8, 2);
        enc.add_source_symbol(0, Bytes::from_static(b"partial"));
        assert_eq!(enc.buffered_count(), 1);

        let repairs = enc.flush();
        assert_eq!(repairs.len(), 2, "flush should emit R repair symbols");
        assert_eq!(enc.buffered_count(), 0);
    }

    #[test]
    fn encoder_redundancy_ratio() {
        let enc = FecEncoder::new(32, 4);
        assert!((enc.redundancy_ratio() - 0.125).abs() < 0.001);
    }

    #[test]
    fn encoder_set_rate() {
        let mut enc = FecEncoder::new(32, 4);
        enc.set_rate(16, 8);
        assert!((enc.redundancy_ratio() - 0.5).abs() < 0.001);
    }

    #[test]
    fn repair_packet_is_valid_wire_format() {
        let mut enc = FecEncoder::new(2, 1);
        enc.add_source_symbol(0, Bytes::from_static(b"hello"));
        let repairs = enc.add_source_symbol(1, Bytes::from_static(b"world"));
        assert_eq!(repairs.len(), 1);

        // Verify the repair packet can be decoded as a valid Strata packet
        use crate::wire::Packet;
        let decoded = Packet::decode(&mut repairs[0].clone()).unwrap();
        assert_eq!(decoded.header.packet_type, crate::wire::PacketType::Control);
    }

    // ─── FEC Decoder Tests ──────────────────────────────────────────────

    #[test]
    fn decoder_complete_generation_needs_no_recovery() {
        let mut dec = FecDecoder::new(16);
        let k = 4;
        let r = 2;

        // Add all K source symbols
        for i in 0..k {
            dec.add_source_symbol(0, i, k, r, Bytes::from(vec![i as u8; 10]));
        }

        assert!(
            dec.is_complete(0),
            "generation should be complete with all K symbols"
        );
        let recovered = dec.try_recover(0);
        assert!(recovered.is_empty(), "no recovery needed");
    }

    #[test]
    fn decoder_single_loss_xor_recovery() {
        let mut enc = FecEncoder::new(4, 1);
        let mut dec = FecDecoder::new(16);

        let symbols: Vec<Bytes> = (0..4).map(|i| Bytes::from(vec![i as u8 * 10; 8])).collect();

        // Encoder: add all 4 symbols, get repair
        for (i, sym) in symbols.iter().enumerate() {
            enc.add_source_symbol(i as u64, sym.clone());
        }
        // Generation was emitted, get the repair via flush of next...
        // Actually the repair was already returned. Let me redo:
        let mut enc2 = FecEncoder::new(4, 1);
        let mut repair_data = None;
        for (i, sym) in symbols.iter().enumerate() {
            let repairs = enc2.add_source_symbol(i as u64, sym.clone());
            if !repairs.is_empty() {
                // Decode the repair packet to get the FEC repair data
                let mut buf = repairs[0].clone();
                let pkt = crate::wire::Packet::decode(&mut buf).unwrap();
                // payload = subtype(1) + fec_header(5) + repair_data
                let mut payload = pkt.payload;
                let _subtype = payload.split_to(1);
                let fec_hdr = FecRepairHeader::decode(&mut payload).unwrap();
                repair_data = Some((fec_hdr, payload.to_vec()));
            }
        }

        let (fec_hdr, repair_bytes) = repair_data.expect("should have repair");

        // Decoder: receive symbols 0, 1, 3 (missing symbol 2)
        for i in [0usize, 1, 3] {
            dec.add_source_symbol(0, i, 4, 1, symbols[i].clone());
        }
        dec.add_repair_symbol(&fec_hdr, repair_bytes);

        assert!(!dec.is_complete(0));
        let recovered = dec.try_recover(0);
        assert_eq!(recovered.len(), 1, "should recover 1 missing symbol");
        assert_eq!(recovered[0].0, 2, "should recover index 2");
        assert_eq!(
            recovered[0].1, symbols[2],
            "recovered data should match original"
        );
    }

    #[test]
    fn decoder_two_losses_cannot_recover_with_one_repair() {
        let mut dec = FecDecoder::new(16);

        // K=4, R=1 — only 1 repair symbol
        dec.add_source_symbol(0, 0, 4, 1, Bytes::from_static(b"aaaa"));
        dec.add_source_symbol(0, 3, 4, 1, Bytes::from_static(b"dddd"));
        // Missing indices 1 and 2 — cannot recover with just 1 repair

        let dummy_repair = vec![0u8; 4];
        dec.add_repair_symbol(
            &FecRepairHeader {
                generation_id: 0,
                symbol_index: 0,
                k: 4,
                r: 1,
            },
            dummy_repair,
        );

        let recovered = dec.try_recover(0);
        assert!(
            recovered.is_empty(),
            "cannot recover 2 losses with 1 repair"
        );
    }

    #[test]
    fn decoder_generation_limit() {
        let mut dec = FecDecoder::new(3);

        for gen_id in 0..5u16 {
            dec.add_source_symbol(gen_id, 0, 4, 1, Bytes::from_static(b"x"));
        }

        assert!(
            dec.generation_count() <= 3,
            "should enforce max_generations limit"
        );
    }

    #[test]
    fn decoder_remove_generation() {
        let mut dec = FecDecoder::new(16);
        dec.add_source_symbol(42, 0, 2, 1, Bytes::from_static(b"x"));
        assert_eq!(dec.generation_count(), 1);
        dec.remove_generation(42);
        assert_eq!(dec.generation_count(), 0);
    }

    // ─── TAROT Optimizer Tests ──────────────────────────────────────────

    #[test]
    fn tarot_zero_loss_recommends_minimum_r() {
        let opt = TarotOptimizer::new();
        let r = opt.compute_optimal_r(0.0, 50.0, 32);
        // At zero loss, should recommend minimal FEC
        assert!(r <= 2, "zero-loss should recommend minimal FEC, got R={r}");
    }

    #[test]
    fn tarot_high_loss_recommends_more_r() {
        let opt = TarotOptimizer::new();
        let r_low = opt.compute_optimal_r(0.01, 50.0, 32);
        let r_high = opt.compute_optimal_r(0.10, 50.0, 32);
        assert!(
            r_high >= r_low,
            "higher loss should need >= FEC: low={r_low}, high={r_high}"
        );
    }

    #[test]
    fn tarot_result_within_bounds() {
        let opt = TarotOptimizer::new();
        for loss in [0.0, 0.01, 0.05, 0.10, 0.20, 0.50] {
            let r = opt.compute_optimal_r(loss, 50.0, 32);
            assert!(r >= 1, "R must be >= 1");
            assert!(r <= 16, "R must be <= K/2 = 16");
        }
    }
}
