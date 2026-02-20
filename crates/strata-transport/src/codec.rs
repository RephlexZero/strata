//! # FEC Codec Engine — Sliding-Window RLNC
//!
//! Random Linear Network Coding over GF(2^8) with a sliding source window.
//!
//! ## Design (from Master Plan §4)
//!
//! - **Layer 1**: Thin continuous FEC — 5-10% coded redundancy, systematic
//!   (source packets sent unencoded, repair symbols appended)
//! - **Layer 2**: NACK-triggered additional repair symbols from the same window
//! - **Layer 3**: TAROT adaptive FEC rate optimization
//!
//! Unlike block FEC (Reed-Solomon), the sliding-window approach does not
//! require accumulating a full block before encoding. Repair symbols can
//! reference any source symbol still in the window, giving immediate
//! recoverability and lower latency.
//!
//! ## GF(2^8) Arithmetic
//!
//! All field operations use the irreducible polynomial x^8 + x^4 + x^3 + x + 1
//! (0x11B), matching AES and most coding libraries.

use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

use crate::wire::{FecRepairHeader, PacketHeader};

// ─── GF(2^8) Arithmetic ────────────────────────────────────────────────────

/// Multiplication and inverse tables for GF(2^8) with polynomial 0x11B.
struct Gf256Tables {
    mul: [[u8; 256]; 256],
    inv: [u8; 256],
}

impl Gf256Tables {
    const fn generate() -> Self {
        let mut mul = [[0u8; 256]; 256];
        let mut inv = [0u8; 256];

        // Build log/exp tables using generator 3 (primitive root of 0x11B).
        // Note: 2 has order 51 in GF(2^8)/0x11B, so it is NOT a generator.
        // 3 = (x+1) has order 255 and generates the full multiplicative group.
        let mut exp = [0u8; 256];
        let mut log = [0u8; 256];
        let mut val: u16 = 1;
        let mut i: usize = 0;
        while i < 255 {
            exp[i] = val as u8;
            log[val as usize] = i as u8;
            // Multiply val by generator 3 in GF(2^8):
            //   val * 3 = val * (x+1) = val*x + val = xtime(val) ^ val
            let mut xtime: u16 = val << 1;
            if xtime & 0x100 != 0 {
                xtime ^= 0x11B;
            }
            val = xtime ^ val;
            i += 1;
        }
        exp[255] = exp[0]; // wrap

        // Build full multiplication table
        let mut a = 0usize;
        while a < 256 {
            mul[a][0] = 0;
            mul[0][a] = 0;
            a += 1;
        }
        a = 1;
        while a < 256 {
            let mut b = 1usize;
            while b < 256 {
                let log_sum = (log[a] as u16 + log[b] as u16) % 255;
                mul[a][b] = exp[log_sum as usize];
                b += 1;
            }
            a += 1;
        }

        // Build inverse table: inv[a] = a^254 in GF(2^8)
        inv[0] = 0; // 0 has no inverse
        inv[1] = 1;
        i = 2;
        while i < 256 {
            // inv[i] = exp[255 - log[i]]
            let l = log[i] as u16;
            inv[i] = exp[(255 - l) as usize];
            i += 1;
        }

        Gf256Tables { mul, inv }
    }
}

static GF: Gf256Tables = Gf256Tables::generate();

#[inline]
fn gf_mul(a: u8, b: u8) -> u8 {
    GF.mul[a as usize][b as usize]
}

#[inline]
fn gf_inv(a: u8) -> u8 {
    GF.inv[a as usize]
}

#[inline]
fn gf_add(a: u8, b: u8) -> u8 {
    a ^ b // addition in GF(2^8) is XOR
}

/// Deterministic coefficient generation from (window_start, repair_index, symbol_offset).
/// Uses a simple but effective hash to produce well-distributed GF(2^8) coefficients.
fn coding_coefficient(window_start: u16, repair_index: u8, symbol_offset: usize) -> u8 {
    // Mix the inputs into a non-zero GF(2^8) element
    let mut h: u32 = 0x9E37_79B9; // golden ratio fractional
    h = h.wrapping_mul(31).wrapping_add(window_start as u32);
    h = h.wrapping_mul(31).wrapping_add(repair_index as u32);
    h = h.wrapping_mul(31).wrapping_add(symbol_offset as u32);
    h ^= h >> 16;
    h = h.wrapping_mul(0x045D_9F3B);
    h ^= h >> 16;
    // Map to non-zero GF(2^8) element (1..=255)
    ((h % 255) + 1) as u8
}

// ─── FEC Encoder ─────────────────────────────────────────────────────────

/// Sliding-window RLNC encoder.
///
/// Maintains a window of up to `window_size` source symbols. Each call to
/// `add_source_symbol` may trigger emission of repair symbols when the window
/// reaches the target size. Repair symbols are random linear combinations
/// over GF(2^8) of all source symbols in the current window.
pub struct FecEncoder {
    /// Maximum source symbols in the window (K).
    window_size: usize,
    /// Number of repair symbols to emit per window (R).
    repair_count: usize,
    /// Current window generation ID (incremented on each window slide).
    current_gen_id: u16,
    /// Source symbols in the current window: (seq, data).
    window: Vec<(u64, Bytes)>,
}

impl FecEncoder {
    /// Create a new sliding-window RLNC encoder.
    ///
    /// - `k`: window size (number of source symbols per window)
    /// - `r`: repair symbols generated per window
    pub fn new(k: usize, r: usize) -> Self {
        assert!(k > 0, "FEC K must be > 0");
        assert!(r > 0, "FEC R must be > 0");
        FecEncoder {
            window_size: k,
            repair_count: r,
            current_gen_id: 0,
            window: Vec::with_capacity(k),
        }
    }

    /// Feed a source symbol into the encoder.
    ///
    /// When the window reaches `window_size` symbols, generates `repair_count`
    /// RLNC repair packets and slides the window. Returns repair packets
    /// (empty vec if window not yet full).
    pub fn add_source_symbol(&mut self, seq: u64, data: Bytes) -> Vec<Bytes> {
        self.window.push((seq, data));

        if self.window.len() >= self.window_size {
            let repairs = self.emit_repair();
            self.current_gen_id = self.current_gen_id.wrapping_add(1);
            self.window.clear();
            repairs
        } else {
            Vec::new()
        }
    }

    /// Generate RLNC repair symbols from the current window.
    ///
    /// Each repair symbol is a random linear combination in GF(2^8):
    ///   repair[j] = Σ (c_i · source[i])  for all i in window
    ///
    /// Coefficients are deterministically derived from (gen_id, repair_index, i)
    /// so the decoder can reconstruct them without out-of-band signalling.
    fn emit_repair(&self) -> Vec<Bytes> {
        let gen_id = self.current_gen_id;
        let k = self.window.len();

        let max_len = self.window.iter().map(|(_, d)| d.len()).max().unwrap_or(0);
        if max_len == 0 {
            return Vec::new();
        }

        let mut repairs = Vec::with_capacity(self.repair_count);

        for repair_idx in 0..self.repair_count {
            let mut repair_data = vec![0u8; max_len];

            for (i, (_, symbol)) in self.window.iter().enumerate() {
                let coeff = coding_coefficient(gen_id, repair_idx as u8, i);
                for (j, &byte) in symbol.iter().enumerate() {
                    // repair[j] += coeff * symbol[j]  in GF(2^8)
                    repair_data[j] = gf_add(repair_data[j], gf_mul(coeff, byte));
                }
            }

            // Serialize as a FEC repair control packet
            let fec_header = FecRepairHeader {
                generation_id: gen_id,
                symbol_index: repair_idx as u8,
                k: k as u8,
                r: self.repair_count as u8,
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

    /// Number of source symbols buffered in the current window.
    pub fn buffered_count(&self) -> usize {
        self.window.len()
    }

    /// Flush the current partial window — emit repair even if < K symbols.
    pub fn flush(&mut self) -> Vec<Bytes> {
        if self.window.is_empty() {
            return Vec::new();
        }
        let repairs = self.emit_repair();
        self.current_gen_id = self.current_gen_id.wrapping_add(1);
        self.window.clear();
        repairs
    }

    /// Update FEC parameters (for TAROT adaptive rate).
    pub fn set_rate(&mut self, k: usize, r: usize) {
        assert!(k > 0 && r > 0);
        self.window_size = k;
        self.repair_count = r;
    }

    /// Current redundancy ratio: R / K.
    pub fn redundancy_ratio(&self) -> f64 {
        self.repair_count as f64 / self.window_size as f64
    }
}

// ─── FEC Decoder ─────────────────────────────────────────────────────────

/// Per-generation decoder state using Gaussian elimination over GF(2^8).
#[derive(Debug)]
struct GenerationState {
    /// Number of source symbols expected (K).
    k: usize,
    /// R value from header (stored for diagnostics).
    #[allow(dead_code)]
    r: usize,
    /// Source symbols received: position_in_window → data.
    source_symbols: HashMap<usize, Bytes>,
    /// Augmented matrix rows for Gaussian elimination.
    /// Each row: [k coefficients | max_symbol_len data bytes].
    matrix_rows: Vec<Vec<u8>>,
    /// Column pivots: col → row that has the pivot.
    pivots: HashMap<usize, usize>,
    /// Data length (max symbol size seen).
    symbol_len: usize,
    /// Generation ID (for coefficient reconstruction).
    gen_id: u16,
}

impl GenerationState {
    fn new(gen_id: u16, k: usize, r: usize) -> Self {
        GenerationState {
            k,
            r,
            source_symbols: HashMap::new(),
            matrix_rows: Vec::new(),
            pivots: HashMap::new(),
            symbol_len: 0,
            gen_id,
        }
    }

    /// Insert a source symbol. Returns true if it was new.
    fn add_source(&mut self, index: usize, data: Bytes) -> bool {
        if self.source_symbols.contains_key(&index) {
            return false;
        }
        if data.len() > self.symbol_len {
            self.symbol_len = data.len();
        }
        self.source_symbols.insert(index, data.clone());

        // Add as a unit vector row in the augmented matrix
        let mut row = vec![0u8; self.k + self.symbol_len];
        row[index] = 1; // unit vector coefficient
        for (j, &b) in data.iter().enumerate() {
            row[self.k + j] = b;
        }
        self.add_row(row);
        true
    }

    /// Insert a repair symbol with its coding coefficients.
    fn add_repair(&mut self, repair_index: u8, data: &[u8]) {
        if data.len() > self.symbol_len {
            self.symbol_len = data.len();
            // Expand existing rows
            for row in &mut self.matrix_rows {
                row.resize(self.k + self.symbol_len, 0);
            }
        }

        // Reconstruct coefficients
        let mut row = vec![0u8; self.k + self.symbol_len];
        for (i, coeff) in row.iter_mut().enumerate().take(self.k) {
            *coeff = coding_coefficient(self.gen_id, repair_index, i);
        }
        for (j, &b) in data.iter().enumerate() {
            row[self.k + j] = b;
        }

        self.add_row(row);
    }

    /// Add a row to the matrix and perform incremental Gaussian elimination.
    fn add_row(&mut self, mut row: Vec<u8>) {
        // Ensure row is long enough
        let total = self.k + self.symbol_len;
        row.resize(total, 0);

        // Reduce this row by existing pivots
        for (&col, &pivot_row_idx) in &self.pivots {
            if row[col] != 0 {
                let factor = row[col]; // we need to eliminate this
                let pivot_val = self.matrix_rows[pivot_row_idx][col];
                let scale = gf_mul(factor, gf_inv(pivot_val));
                for (j, r) in row.iter_mut().enumerate() {
                    *r = gf_add(*r, gf_mul(scale, self.matrix_rows[pivot_row_idx][j]));
                }
            }
        }

        // Find the first non-zero coefficient column in this reduced row
        let pivot_col = match (0..self.k).find(|&c| row[c] != 0) {
            Some(c) => c,
            None => return, // linearly dependent — discard
        };

        let row_idx = self.matrix_rows.len();
        self.matrix_rows.push(row);
        self.pivots.insert(pivot_col, row_idx);

        // Back-substitute: reduce older rows by this new pivot
        let new_pivot_val = self.matrix_rows[row_idx][pivot_col];
        for (&other_col, &other_row_idx) in &self.pivots {
            if other_col == pivot_col {
                continue;
            }
            let val = self.matrix_rows[other_row_idx][pivot_col];
            if val != 0 {
                let scale = gf_mul(val, gf_inv(new_pivot_val));
                // We need to clone the new row to avoid double borrow
                let new_row_data: Vec<u8> = self.matrix_rows[row_idx].clone();
                let other_row = &mut self.matrix_rows[other_row_idx];
                for j in 0..other_row.len() {
                    other_row[j] = gf_add(other_row[j], gf_mul(scale, new_row_data[j]));
                }
            }
        }
    }

    /// Attempt to recover all missing source symbols.
    /// Returns (index, data) pairs for recovered symbols.
    ///
    /// A symbol at `col` is only recovered when its pivot row has zero
    /// coefficients for every other missing symbol — i.e. the system is fully
    /// determined for that column.  Rows that still mix multiple unknowns
    /// (under-determined) are skipped.
    fn try_recover(&self) -> Vec<(usize, Bytes)> {
        let mut recovered = Vec::new();

        for col in 0..self.k {
            if self.source_symbols.contains_key(&col) {
                continue; // already have this one
            }
            // Check if we have a pivot for this column
            if let Some(&row_idx) = self.pivots.get(&col) {
                let row = &self.matrix_rows[row_idx];
                let pivot_val = row[col];
                if pivot_val == 0 {
                    continue;
                }

                // Only recover if all other UNKNOWN columns have zero coefficient
                // in this row.  If another missing symbol still has a non-zero
                // coefficient, the system is under-determined and we cannot
                // isolate this symbol.
                let fully_determined = (0..self.k).all(|other| {
                    other == col || self.source_symbols.contains_key(&other) || row[other] == 0
                });
                if !fully_determined {
                    continue;
                }

                let inv = gf_inv(pivot_val);

                // Extract the data portion, scaled by the inverse of the pivot
                let data_start = self.k;
                let data: Vec<u8> = row[data_start..data_start + self.symbol_len]
                    .iter()
                    .map(|&b| gf_mul(b, inv))
                    .collect();

                recovered.push((col, Bytes::from(data)));
            }
        }

        recovered
    }

    /// Check if all K source symbols are available (received or recovered).
    fn is_complete(&self) -> bool {
        if self.source_symbols.len() >= self.k {
            return true;
        }
        // Check if we have pivots for all missing columns
        (0..self.k)
            .all(|col| self.source_symbols.contains_key(&col) || self.pivots.contains_key(&col))
    }
}

/// Sliding-window RLNC decoder.
///
/// Tracks multiple generations and performs progressive Gaussian elimination
/// to recover lost source symbols from repair symbols.
pub struct FecDecoder {
    /// Active generations: gen_id → state.
    generations: HashMap<u16, GenerationState>,
    /// Maximum number of generations to track.
    max_generations: usize,
}

impl FecDecoder {
    pub fn new(max_generations: usize) -> Self {
        FecDecoder {
            generations: HashMap::new(),
            max_generations,
        }
    }

    /// Record a received source symbol.
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
            .or_insert_with(|| GenerationState::new(generation_id, k, r));
        gen.add_source(index_in_gen, data);
        self.enforce_limit();
    }

    /// Record a received repair symbol.
    pub fn add_repair_symbol(&mut self, header: &FecRepairHeader, repair_data: Vec<u8>) {
        let gen = self
            .generations
            .entry(header.generation_id)
            .or_insert_with(|| {
                GenerationState::new(header.generation_id, header.k as usize, header.r as usize)
            });
        gen.add_repair(header.symbol_index, &repair_data);
        self.enforce_limit();
    }

    /// Check if a generation has all source symbols (no recovery needed).
    pub fn is_complete(&self, generation_id: u16) -> bool {
        self.generations
            .get(&generation_id)
            .map(|gen| gen.is_complete())
            .unwrap_or(false)
    }

    /// Attempt to recover missing source symbols for a generation.
    ///
    /// Uses Gaussian elimination over GF(2^8) to solve the system of linear
    /// equations formed by the received repair symbols and their coding
    /// coefficients.
    ///
    /// Returns recovered (index, data) pairs.
    pub fn try_recover(&mut self, generation_id: u16) -> Vec<(usize, Bytes)> {
        let gen = match self.generations.get(&generation_id) {
            Some(g) => g,
            None => return Vec::new(),
        };
        gen.try_recover()
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
    /// - `k`: current window size
    ///
    /// Returns the recommended R (repair count) for the given K.
    pub fn compute_optimal_r(&self, loss_rate: f64, rtt_ms: f64, k: usize) -> usize {
        let loss_rate = loss_rate.clamp(0.0, 1.0);
        let k_f = k as f64;

        let mut best_r = 1usize;
        let mut best_cost = f64::MAX;

        let max_r = (k / 2).max(1);
        for r in 1..=max_r {
            let ratio = r as f64 / k_f;
            if ratio < self.min_ratio || ratio > self.max_ratio {
                continue;
            }

            // P_loss: probability that losses exceed repair capacity
            let p_loss = loss_rate.powi(r as i32 + 1);

            // B_overhead: bandwidth consumed by FEC
            let b_overhead = ratio;

            // D_decode: decoding latency proportional to window size
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

    // ─── GF(2^8) arithmetic tests ───────────────────────────────────────

    #[test]
    fn gf_mul_identity() {
        for a in 0..=255u8 {
            assert_eq!(gf_mul(a, 1), a, "a*1 = a failed for a={a}");
            assert_eq!(gf_mul(1, a), a, "1*a = a failed for a={a}");
        }
    }

    #[test]
    fn gf_mul_zero() {
        for a in 0..=255u8 {
            assert_eq!(gf_mul(a, 0), 0, "a*0 = 0 failed for a={a}");
            assert_eq!(gf_mul(0, a), 0, "0*a = 0 failed for a={a}");
        }
    }

    #[test]
    fn gf_inverse_property() {
        for a in 1..=255u8 {
            let inv = gf_inv(a);
            assert_ne!(inv, 0, "inverse of {a} should be non-zero");
            assert_eq!(gf_mul(a, inv), 1, "a * inv(a) = 1 failed for a={a}");
        }
    }

    #[test]
    fn gf_mul_commutative() {
        for a in 0..32u8 {
            for b in 0..32u8 {
                assert_eq!(
                    gf_mul(a, b),
                    gf_mul(b, a),
                    "commutativity failed for ({a}, {b})"
                );
            }
        }
    }

    // ─── proptest: RLNC encode/decode correctness ───────────────────────

    proptest! {
        /// Single-symbol loss recovery via RLNC.
        #[test]
        fn proptest_rlnc_single_loss_recovery(
            k in 2..16usize,
            lost_idx in 0..16usize,
            symbol_len in 1..64usize,
            seed in 0..1000u64,
        ) {
            let lost_idx = lost_idx % k;
            let r = 1;

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
                &recovered[0].1[..symbol_len],
                &symbols[lost_idx][..],
                "recovered data should match original"
            );
        }

        /// When all symbols are received, no recovery is needed.
        #[test]
        fn proptest_rlnc_no_loss_no_recovery(
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

        /// Multi-loss recovery: lose `losses` symbols, provide `losses` repair symbols.
        #[test]
        fn proptest_rlnc_multi_loss_recovery(
            k in 3..10usize,
            losses in 1..3usize,
            symbol_len in 4..32usize,
            seed in 0..500u64,
        ) {
            let losses = losses.min(k - 1); // can't lose all symbols
            let r = losses; // need exactly `losses` repair symbols

            let symbols: Vec<Bytes> = (0..k)
                .map(|i| {
                    Bytes::from(
                        (0..symbol_len)
                            .map(|j| ((i as u64 * 37 + j as u64 * 13 + seed) % 256) as u8)
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
                repair_packets = enc.flush();
            }

            // Pick which symbols to lose (first `losses` indices)
            let lost_indices: Vec<usize> = (0..losses).collect();

            // Decode
            let mut dec = FecDecoder::new(16);
            for (i, sym) in symbols.iter().enumerate().take(k) {
                if !lost_indices.contains(&i) {
                    dec.add_source_symbol(0, i, k, r, sym.clone());
                }
            }

            // Add repair symbols
            for repair_pkt in &repair_packets {
                let mut buf = repair_pkt.clone();
                let pkt = crate::wire::Packet::decode(&mut buf).unwrap();
                let mut payload = pkt.payload;
                let _subtype = payload.split_to(1);
                let fec_hdr = FecRepairHeader::decode(&mut payload).unwrap();
                let repair_data = payload.to_vec();
                dec.add_repair_symbol(&fec_hdr, repair_data);
            }

            let mut recovered = dec.try_recover(0);
            recovered.sort_by_key(|(idx, _)| *idx);

            prop_assert_eq!(
                recovered.len(),
                losses,
                "should recover exactly {} symbols, got {}",
                losses,
                recovered.len()
            );

            for (rec_idx, rec_data) in &recovered {
                prop_assert!(
                    lost_indices.contains(rec_idx),
                    "recovered index {rec_idx} was not in lost set"
                );
                prop_assert_eq!(
                    &rec_data[..symbol_len],
                    &symbols[*rec_idx][..],
                    "recovered data mismatch at index {}",
                    rec_idx
                );
            }
        }
    }

    // ─── FEC Encoder Tests ──────────────────────────────────────────────

    #[test]
    fn encoder_emits_repair_after_k_symbols() {
        let mut enc = FecEncoder::new(4, 2);

        for i in 0..3u64 {
            let repairs = enc.add_source_symbol(i, Bytes::from(vec![i as u8; 100]));
            assert!(
                repairs.is_empty(),
                "should not emit repair before K symbols"
            );
        }

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

        enc.add_source_symbol(0, Bytes::from_static(b"a"));
        enc.add_source_symbol(1, Bytes::from_static(b"b"));
        assert_eq!(enc.current_generation(), 1);

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

        use crate::wire::Packet;
        let decoded = Packet::decode(&mut repairs[0].clone()).unwrap();
        assert_eq!(decoded.header.packet_type, crate::wire::PacketType::Control);
    }

    // ─── Multi-loss recovery (deterministic) ────────────────────────────

    #[test]
    fn rlnc_recover_two_losses_with_two_repairs() {
        let k = 4;
        let r = 2;
        let symbols: Vec<Bytes> = (0..k)
            .map(|i| Bytes::from(vec![(i * 10 + 1) as u8; 8]))
            .collect();

        let mut enc = FecEncoder::new(k, r);
        let mut repair_packets = Vec::new();
        for (i, sym) in symbols.iter().enumerate() {
            let repairs = enc.add_source_symbol(i as u64, sym.clone());
            repair_packets.extend(repairs);
        }
        assert_eq!(repair_packets.len(), 2);

        // Lose symbols 1 and 3
        let mut dec = FecDecoder::new(16);
        dec.add_source_symbol(0, 0, k, r, symbols[0].clone());
        dec.add_source_symbol(0, 2, k, r, symbols[2].clone());

        for repair_pkt in &repair_packets {
            let mut buf = repair_pkt.clone();
            let pkt = crate::wire::Packet::decode(&mut buf).unwrap();
            let mut payload = pkt.payload;
            let _subtype = payload.split_to(1);
            let fec_hdr = FecRepairHeader::decode(&mut payload).unwrap();
            dec.add_repair_symbol(&fec_hdr, payload.to_vec());
        }

        let mut recovered = dec.try_recover(0);
        recovered.sort_by_key(|(idx, _)| *idx);
        assert_eq!(recovered.len(), 2, "should recover 2 missing symbols");
        assert_eq!(recovered[0].0, 1);
        assert_eq!(recovered[1].0, 3);
        assert_eq!(&recovered[0].1[..8], &symbols[1][..]);
        assert_eq!(&recovered[1].1[..8], &symbols[3][..]);
    }

    #[test]
    fn rlnc_three_losses_exceeds_two_repairs() {
        // K=6, R=2, lose 3 symbols — should NOT fully recover
        let k = 6;
        let r = 2;
        let symbols: Vec<Bytes> = (0..k).map(|i| Bytes::from(vec![i as u8; 10])).collect();

        let mut enc = FecEncoder::new(k, r);
        let mut repair_packets = Vec::new();
        for (i, sym) in symbols.iter().enumerate() {
            repair_packets.extend(enc.add_source_symbol(i as u64, sym.clone()));
        }

        let mut dec = FecDecoder::new(16);
        // Only provide symbols 0, 1, 2 (lose 3, 4, 5)
        for (i, sym) in symbols.iter().enumerate().take(3) {
            dec.add_source_symbol(0, i, k, r, sym.clone());
        }
        for repair_pkt in &repair_packets {
            let mut buf = repair_pkt.clone();
            let pkt = crate::wire::Packet::decode(&mut buf).unwrap();
            let mut payload = pkt.payload;
            let _subtype = payload.split_to(1);
            let fec_hdr = FecRepairHeader::decode(&mut payload).unwrap();
            dec.add_repair_symbol(&fec_hdr, payload.to_vec());
        }

        let recovered = dec.try_recover(0);
        assert!(
            recovered.len() < 3,
            "cannot recover 3 losses with only 2 repair symbols"
        );
    }

    // ─── FEC Decoder Tests ──────────────────────────────────────────────

    #[test]
    fn decoder_complete_generation_needs_no_recovery() {
        let mut dec = FecDecoder::new(16);
        let k = 4;
        let r = 2;

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
    fn decoder_single_loss_rlnc_recovery() {
        let mut enc = FecEncoder::new(4, 1);
        let mut dec = FecDecoder::new(16);

        let symbols: Vec<Bytes> = (0..4).map(|i| Bytes::from(vec![i as u8 * 10; 8])).collect();

        let mut repair_packets = Vec::new();
        for (i, sym) in symbols.iter().enumerate() {
            let repairs = enc.add_source_symbol(i as u64, sym.clone());
            repair_packets.extend(repairs);
        }
        assert_eq!(repair_packets.len(), 1);

        // Parse repair packet
        let mut buf = repair_packets[0].clone();
        let pkt = crate::wire::Packet::decode(&mut buf).unwrap();
        let mut payload = pkt.payload;
        let _subtype = payload.split_to(1);
        let fec_hdr = FecRepairHeader::decode(&mut payload).unwrap();
        let repair_bytes = payload.to_vec();

        // Missing symbol 2
        for i in [0usize, 1, 3] {
            dec.add_source_symbol(0, i, 4, 1, symbols[i].clone());
        }
        dec.add_repair_symbol(&fec_hdr, repair_bytes);

        assert!(!dec.is_complete(0) || !dec.try_recover(0).is_empty());
        let recovered = dec.try_recover(0);
        assert_eq!(recovered.len(), 1, "should recover 1 missing symbol");
        assert_eq!(recovered[0].0, 2, "should recover index 2");
        assert_eq!(
            &recovered[0].1[..8],
            &symbols[2][..],
            "recovered data should match original"
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
