//! # Sliding-Window Random Linear Network Coding (RLNC)
//!
//! Replacement for block-based Reed-Solomon FEC. Instead of fixed-size
//! generations, RLNC uses a sliding window of source symbols. Repair symbols
//! are random linear combinations over GF(256) of the symbols currently in the
//! window. The receiver solves the linear system via Gaussian elimination.
//!
//! ## Advantages over block RS
//!
//! - **Lower latency**: no need to wait for a full block; repair can be sent at
//!   any point.
//! - **Multi-symbol recovery**: any `n` linearly independent coded symbols can
//!   recover `n` lost source symbols (with high probability over GF(256)).
//! - **Graceful degradation**: partial decoding is possible even without full
//!   rank.
//! - **Sliding window**: as source symbols are acknowledged, they slide out of
//!   the window, keeping memory bounded.

use bytes::Bytes;

// ─── GF(256) Arithmetic ─────────────────────────────────────────────────────

/// GF(2^8) with primitive polynomial x^8 + x^4 + x^3 + x^2 + 1 (0x11D).
/// 2 is a primitive element (generator) with order 255.
/// Log/antilog tables for O(1) multiply/divide.
mod gf256 {
    /// Multiplication in GF(256).
    pub fn mul(a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            return 0;
        }
        let log_a = LOG_TABLE[a as usize] as u16;
        let log_b = LOG_TABLE[b as usize] as u16;
        let log_sum = (log_a + log_b) % 255;
        EXP_TABLE[log_sum as usize]
    }

    /// Division in GF(256). Panics if b == 0.
    #[allow(dead_code)]
    pub fn div(a: u8, b: u8) -> u8 {
        assert_ne!(b, 0, "division by zero in GF(256)");
        if a == 0 {
            return 0;
        }
        let log_a = LOG_TABLE[a as usize] as u16;
        let log_b = LOG_TABLE[b as usize] as u16;
        let log_diff = (log_a + 255 - log_b) % 255;
        EXP_TABLE[log_diff as usize]
    }

    /// Multiplicative inverse in GF(256).
    pub fn inv(a: u8) -> u8 {
        assert_ne!(a, 0, "inverse of zero in GF(256)");
        let log_a = LOG_TABLE[a as usize] as u16;
        EXP_TABLE[(255 - log_a) as usize]
    }

    // Generate both tables together. Primitive polynomial 0x11D, generator 2.
    const fn gen_tables() -> ([u8; 256], [u8; 512]) {
        let mut log = [0u8; 256];
        let mut exp = [0u8; 512];
        let mut x: u16 = 1;
        let mut i = 0usize;
        while i < 255 {
            exp[i] = x as u8;
            exp[i + 255] = x as u8; // duplicate for easy modular lookup
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11D;
            }
            i += 1;
        }
        // log[0] is unused (0 has no logarithm)
        log[0] = 0;
        (log, exp)
    }

    const TABLES: ([u8; 256], [u8; 512]) = gen_tables();
    const LOG_TABLE: [u8; 256] = TABLES.0;
    const EXP_TABLE: [u8; 512] = TABLES.1;
}

// ─── Sliding-Window RLNC Encoder ────────────────────────────────────────────

/// A source symbol in the encoder window.
#[derive(Clone, Debug)]
struct WindowSymbol {
    /// Global sequence ID.
    seq: u64,
    /// Raw symbol data.
    data: Bytes,
}

/// RLNC encoder with sliding window.
pub struct RlncEncoder {
    /// Source symbols currently in the coding window.
    window: Vec<WindowSymbol>,
    /// Maximum window size.
    window_size: usize,
    /// Sequence number of the first symbol in the window.
    window_start: u64,
    /// RNG state for coefficient generation (xoshiro256**).
    rng_state: [u64; 4],
}

/// A coded (repair) symbol produced by the encoder.
#[derive(Clone, Debug)]
pub struct CodedSymbol {
    /// Coding coefficients: one per window symbol. Length == window.len() at
    /// time of encoding.
    pub coefficients: Vec<u8>,
    /// The coded data: linear combination of window symbols.
    pub data: Vec<u8>,
    /// Sequence of the first symbol in the window when this was encoded.
    pub window_start: u64,
    /// Number of source symbols covered.
    pub window_len: usize,
}

impl RlncEncoder {
    /// Create a new RLNC encoder.
    ///
    /// - `window_size`: max number of source symbols in the coding window.
    /// - `seed`: RNG seed for reproducible coefficient generation.
    pub fn new(window_size: usize, seed: u64) -> Self {
        assert!(window_size > 0);
        // Initialize xoshiro256** state from seed
        let mut s = [0u64; 4];
        // Simple splitmix64 seeding
        let mut z = seed;
        for slot in &mut s {
            z = z.wrapping_add(0x9e3779b97f4a7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
            *slot = z ^ (z >> 31);
        }
        RlncEncoder {
            window: Vec::with_capacity(window_size),
            window_size,
            window_start: 0,
            rng_state: s,
        }
    }

    /// Add a source symbol to the window.
    ///
    /// If the window is full, the oldest symbol is evicted.
    pub fn add_source(&mut self, seq: u64, data: Bytes) {
        if self.window.is_empty() {
            self.window_start = seq;
        }
        self.window.push(WindowSymbol { seq, data });
        if self.window.len() > self.window_size {
            self.window.remove(0);
            self.window_start = self.window[0].seq;
        }
    }

    /// Generate a coded (repair) symbol from the current window.
    ///
    /// Each coefficient is drawn uniformly from GF(256) \ {0}.
    /// The coded data is the GF(256) linear combination of all window symbols.
    pub fn generate_repair(&mut self) -> Option<CodedSymbol> {
        if self.window.is_empty() {
            return None;
        }

        let n = self.window.len();

        // Find max symbol length (zero-pad shorter symbols)
        let max_len = self.window.iter().map(|s| s.data.len()).max().unwrap_or(0);
        if max_len == 0 {
            return None;
        }

        // Generate random nonzero coefficients
        let mut coefficients = Vec::with_capacity(n);
        for _ in 0..n {
            let mut c = 0u8;
            while c == 0 {
                c = (self.next_u64() & 0xFF) as u8;
            }
            coefficients.push(c);
        }

        // Compute linear combination
        let mut coded = vec![0u8; max_len];
        for (i, sym) in self.window.iter().enumerate() {
            let coeff = coefficients[i];
            for (j, &byte) in sym.data.iter().enumerate() {
                coded[j] ^= gf256::mul(coeff, byte);
            }
        }

        Some(CodedSymbol {
            coefficients,
            data: coded,
            window_start: self.window_start,
            window_len: n,
        })
    }

    /// Acknowledge receipt of symbols up to (exclusive) `ack_seq`.
    /// Slides the window forward, evicting acknowledged symbols.
    pub fn acknowledge(&mut self, ack_seq: u64) {
        self.window.retain(|s| s.seq >= ack_seq);
        if let Some(first) = self.window.first() {
            self.window_start = first.seq;
        }
    }

    /// Current window size (number of in-flight source symbols).
    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    /// Maximum window size.
    pub fn max_window_size(&self) -> usize {
        self.window_size
    }

    fn next_u64(&mut self) -> u64 {
        // xoshiro256**
        let result = self.rng_state[1]
            .wrapping_mul(5)
            .rotate_left(7)
            .wrapping_mul(9);
        let t = self.rng_state[1] << 17;
        self.rng_state[2] ^= self.rng_state[0];
        self.rng_state[3] ^= self.rng_state[1];
        self.rng_state[1] ^= self.rng_state[2];
        self.rng_state[0] ^= self.rng_state[3];
        self.rng_state[2] ^= t;
        self.rng_state[3] = self.rng_state[3].rotate_left(45);
        result
    }
}

// ─── Sliding-Window RLNC Decoder ────────────────────────────────────────────

/// A row in the decoder's coefficient matrix (augmented).
#[derive(Clone, Debug)]
struct DecoderRow {
    /// Coefficients over the current decoding window.
    coeffs: Vec<u8>,
    /// The coded data (right-hand side of the equation).
    data: Vec<u8>,
}

/// RLNC decoder with Gaussian elimination.
pub struct RlncDecoder {
    /// Rows of the augmented matrix (coefficient + data).
    rows: Vec<DecoderRow>,
    /// Sequence of the first symbol in the decoding window.
    window_start: u64,
    /// Width of the coefficient matrix (number of unknown symbols).
    window_len: usize,
    /// Source symbols already received directly (seq → data).
    received: std::collections::HashMap<u64, Bytes>,
    /// Symbols recovered via decoding (seq → data).
    recovered: std::collections::HashMap<u64, Vec<u8>>,
    /// Maximum data length seen.
    max_data_len: usize,
}

impl RlncDecoder {
    /// Create a new decoder.
    pub fn new() -> Self {
        RlncDecoder {
            rows: Vec::new(),
            window_start: 0,
            window_len: 0,
            received: std::collections::HashMap::new(),
            recovered: std::collections::HashMap::new(),
            max_data_len: 0,
        }
    }

    /// Record a directly received source symbol.
    pub fn add_source(&mut self, seq: u64, data: Bytes) {
        self.max_data_len = self.max_data_len.max(data.len());
        self.received.insert(seq, data);
    }

    /// Add a coded (repair) symbol from the encoder.
    ///
    /// The coded symbol carries its coefficients and the window range it covers.
    pub fn add_coded(&mut self, coded: &CodedSymbol) {
        // Update window tracking
        if self.rows.is_empty() && self.received.is_empty() {
            self.window_start = coded.window_start;
        }

        // Extend window if needed
        let coded_end = coded.window_start + coded.window_len as u64;
        let current_end = self.window_start + self.window_len as u64;
        if coded_end > current_end {
            self.window_len = (coded_end - self.window_start) as usize;
        }

        self.max_data_len = self.max_data_len.max(coded.data.len());

        // Adapt coefficients to our window coordinate system
        let offset = (coded.window_start - self.window_start) as usize;
        let mut full_coeffs = vec![0u8; self.window_len];
        for (i, &c) in coded.coefficients.iter().enumerate() {
            if offset + i < full_coeffs.len() {
                full_coeffs[offset + i] = c;
            }
        }

        // Subtract out known source symbols to reduce unknowns
        let mut data = coded.data.clone();
        Self::reduce_with_known_static(
            &self.received,
            &self.recovered,
            self.window_start,
            &full_coeffs,
            &mut data,
        );
        // Zero out coefficients for known symbols
        for (i, c) in full_coeffs.iter_mut().enumerate() {
            if *c == 0 {
                continue;
            }
            let seq = self.window_start + i as u64;
            if self.received.contains_key(&seq) || self.recovered.contains_key(&seq) {
                *c = 0;
            }
        }

        self.rows.push(DecoderRow {
            coeffs: full_coeffs,
            data,
        });
    }

    /// Attempt to recover missing symbols via Gaussian elimination.
    ///
    /// Returns a list of (seq, data) pairs for newly recovered symbols.
    #[allow(unused_variables)]
    pub fn try_recover(&mut self) -> Vec<(u64, Vec<u8>)> {
        // First, reduce all rows with known source symbols
        for (ri, row) in self.rows.iter_mut().enumerate() {
            let mut data = row.data.clone();
            Self::reduce_with_known_static(
                &self.received,
                &self.recovered,
                self.window_start,
                &row.coeffs,
                &mut data,
            );
            row.data = data;
            // Zero out coefficients for known symbols
            for (i, c) in row.coeffs.iter_mut().enumerate() {
                if *c == 0 {
                    continue;
                }
                let seq = self.window_start + i as u64;
                if self.received.contains_key(&seq) || self.recovered.contains_key(&seq) {
                    *c = 0;
                }
            }
            #[cfg(debug_assertions)]
            eprintln!(
                "try_recover: row {} coeffs={:?} data={:?}",
                ri, row.coeffs, row.data
            );
        }

        // Perform Gaussian elimination on the coefficient matrix
        let n = self.window_len;
        let num_rows = self.rows.len();
        if num_rows == 0 || n == 0 {
            return Vec::new();
        }

        // Work on cloned rows for elimination
        let mut matrix: Vec<DecoderRow> = self.rows.clone();

        // Normalize data lengths
        let max_len = self.max_data_len;
        for row in &mut matrix {
            row.data.resize(max_len, 0);
            row.coeffs.resize(n, 0);
        }

        let mut pivot_row = 0;
        let mut pivot_cols = vec![None; n]; // column → row mapping

        #[allow(clippy::needless_range_loop)]
        for col in 0..n {
            let seq = self.window_start + col as u64;
            if self.received.contains_key(&seq) || self.recovered.contains_key(&seq) {
                continue; // already known
            }

            // Find a row with nonzero coefficient in this column
            let mut found = None;
            for row_idx in pivot_row..matrix.len() {
                if matrix[row_idx].coeffs[col] != 0 {
                    found = Some(row_idx);
                    break;
                }
            }

            let row_idx = match found {
                Some(r) => r,
                None => {
                    #[cfg(debug_assertions)]
                    eprintln!("  col {}: no pivot found", col);
                    continue;
                }
            };

            // Swap to pivot position
            matrix.swap(pivot_row, row_idx);

            // Scale pivot row so the pivot element becomes 1
            let pivot_val = matrix[pivot_row].coeffs[col];
            let inv = gf256::inv(pivot_val);
            #[cfg(debug_assertions)]
            eprintln!(
                "  col {}: pivot_row={}, pivot_val={}, inv={}",
                col, pivot_row, pivot_val, inv
            );
            for c in &mut matrix[pivot_row].coeffs {
                *c = gf256::mul(*c, inv);
            }
            for d in &mut matrix[pivot_row].data {
                *d = gf256::mul(*d, inv);
            }

            #[cfg(debug_assertions)]
            eprintln!(
                "  after scale: coeffs={:?} data={:?}",
                matrix[pivot_row].coeffs, matrix[pivot_row].data
            );

            // Eliminate this column from all other rows
            for other in 0..matrix.len() {
                if other == pivot_row {
                    continue;
                }
                let factor = matrix[other].coeffs[col];
                if factor == 0 {
                    continue;
                }
                // other_row -= factor * pivot_row
                let pivot_coeffs = matrix[pivot_row].coeffs.clone();
                let pivot_data = matrix[pivot_row].data.clone();
                for (j, pc) in pivot_coeffs.iter().enumerate() {
                    matrix[other].coeffs[j] ^= gf256::mul(factor, *pc);
                }
                for (j, pd) in pivot_data.iter().enumerate() {
                    matrix[other].data[j] ^= gf256::mul(factor, *pd);
                }
                #[cfg(debug_assertions)]
                eprintln!(
                    "  eliminated row {}: factor={} coeffs={:?} data={:?}",
                    other, factor, matrix[other].coeffs, matrix[other].data
                );
            }

            pivot_cols[col] = Some(pivot_row);
            pivot_row += 1;
        }

        #[cfg(debug_assertions)]
        {
            eprintln!("  pivot_cols={:?}", pivot_cols);
            for (i, row) in matrix.iter().enumerate() {
                eprintln!(
                    "  final row {}: coeffs={:?} data={:?}",
                    i, row.coeffs, row.data
                );
            }
        }

        // Extract recovered symbols from pivot rows
        let mut newly_recovered = Vec::new();
        #[allow(clippy::needless_range_loop)]
        for col in 0..n {
            let seq = self.window_start + col as u64;
            if self.received.contains_key(&seq) || self.recovered.contains_key(&seq) {
                continue;
            }
            if let Some(prow) = pivot_cols[col] {
                // Verify this row is fully reduced (only this column is nonzero)
                let is_unit = matrix[prow].coeffs.iter().enumerate().all(|(j, &c)| {
                    if j == col {
                        c == 1
                    } else {
                        c == 0
                    }
                });
                if is_unit {
                    let data = matrix[prow].data.clone();
                    self.recovered.insert(seq, data.clone());
                    newly_recovered.push((seq, data));
                }
            }
        }

        newly_recovered
    }

    /// Check if a specific sequence has been received or recovered.
    pub fn has_symbol(&self, seq: u64) -> bool {
        self.received.contains_key(&seq) || self.recovered.contains_key(&seq)
    }

    /// Get data for a symbol (either received or recovered).
    pub fn get_symbol(&self, seq: u64) -> Option<&[u8]> {
        if let Some(data) = self.received.get(&seq) {
            Some(data.as_ref())
        } else {
            self.recovered.get(&seq).map(|v| v.as_slice())
        }
    }

    /// Number of symbols known (received + recovered).
    pub fn known_count(&self) -> usize {
        self.received.len() + self.recovered.len()
    }

    fn reduce_with_known_static(
        received: &std::collections::HashMap<u64, Bytes>,
        recovered: &std::collections::HashMap<u64, Vec<u8>>,
        window_start: u64,
        coeffs: &[u8],
        data: &mut [u8],
    ) {
        for (i, &c) in coeffs.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let seq = window_start + i as u64;
            let known_data = if let Some(d) = received.get(&seq) {
                Some(d.as_ref())
            } else {
                recovered.get(&seq).map(|v| v.as_slice())
            };
            if let Some(kd) = known_data {
                for (j, &byte) in kd.iter().enumerate() {
                    if j < data.len() {
                        data[j] ^= gf256::mul(c, byte);
                    }
                }
            }
        }
    }
}

impl Default for RlncDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── GF(256) Tests ──────────────────────────────────────────────────

    #[test]
    fn gf256_identity() {
        // a * 1 = a for all nonzero a
        for a in 1u8..=255 {
            assert_eq!(gf256::mul(a, 1), a);
            assert_eq!(gf256::mul(1, a), a);
        }
    }

    #[test]
    fn gf256_zero() {
        for a in 0u8..=255 {
            assert_eq!(gf256::mul(a, 0), 0);
            assert_eq!(gf256::mul(0, a), 0);
        }
    }

    #[test]
    fn gf256_inverse() {
        // a * inv(a) = 1 for all nonzero a
        for a in 1u8..=255 {
            let inv = gf256::inv(a);
            assert_eq!(gf256::mul(a, inv), 1, "a={}, inv={}", a, inv);
        }
    }

    #[test]
    fn gf256_div_roundtrip() {
        // (a * b) / b = a for nonzero a, b
        for a in 1u8..=50 {
            for b in 1u8..=50 {
                let product = gf256::mul(a, b);
                assert_eq!(gf256::div(product, b), a);
            }
        }
    }

    // ─── Encoder Tests ──────────────────────────────────────────────────

    #[test]
    fn encoder_window_eviction() {
        let mut enc = RlncEncoder::new(4, 42);
        for i in 0..6u64 {
            enc.add_source(i, Bytes::from(vec![i as u8; 10]));
        }
        // Window should contain only the last 4 symbols
        assert_eq!(enc.window_len(), 4);
        assert_eq!(enc.window[0].seq, 2);
        assert_eq!(enc.window[3].seq, 5);
    }

    #[test]
    fn encoder_acknowledge_slides_window() {
        let mut enc = RlncEncoder::new(8, 42);
        for i in 0..6u64 {
            enc.add_source(i, Bytes::from(vec![i as u8; 10]));
        }
        assert_eq!(enc.window_len(), 6);
        enc.acknowledge(3);
        assert_eq!(enc.window_len(), 3);
        assert_eq!(enc.window[0].seq, 3);
    }

    #[test]
    fn encoder_generates_repair() {
        let mut enc = RlncEncoder::new(8, 42);
        enc.add_source(0, Bytes::from(vec![0xAA; 10]));
        enc.add_source(1, Bytes::from(vec![0xBB; 10]));
        let repair = enc.generate_repair().unwrap();
        assert_eq!(repair.coefficients.len(), 2);
        assert_eq!(repair.data.len(), 10);
        assert_eq!(repair.window_start, 0);
        assert_eq!(repair.window_len, 2);
        // All coefficients must be nonzero
        for &c in &repair.coefficients {
            assert_ne!(c, 0);
        }
    }

    // ─── Single-Symbol Recovery ─────────────────────────────────────────

    #[test]
    fn recover_single_loss_with_one_repair() {
        let symbols: Vec<Bytes> = (0..4u64)
            .map(|i| Bytes::from(vec![(i + 1) as u8; 8]))
            .collect();

        let mut enc = RlncEncoder::new(8, 123);
        for (i, sym) in symbols.iter().enumerate() {
            enc.add_source(i as u64, sym.clone());
        }
        let repair = enc.generate_repair().unwrap();

        let mut dec = RlncDecoder::new();
        // Receive all except symbol 2
        dec.add_source(0, symbols[0].clone());
        dec.add_source(1, symbols[1].clone());
        // symbol 2 is lost
        dec.add_source(3, symbols[3].clone());
        dec.add_coded(&repair);

        let recovered = dec.try_recover();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].0, 2);
        assert_eq!(&recovered[0].1[..8], &symbols[2][..]);
    }

    // ─── Multi-Symbol Recovery ──────────────────────────────────────────

    #[test]
    fn recover_two_losses_with_two_repairs() {
        let symbols: Vec<Bytes> = (0..4u64)
            .map(|i| Bytes::from(vec![(i * 17 + 3) as u8; 12]))
            .collect();

        let mut enc = RlncEncoder::new(8, 456);
        for (i, sym) in symbols.iter().enumerate() {
            enc.add_source(i as u64, sym.clone());
        }
        let repair1 = enc.generate_repair().unwrap();
        let repair2 = enc.generate_repair().unwrap();

        let mut dec = RlncDecoder::new();
        // Receive symbols 0 and 3, lose 1 and 2
        dec.add_source(0, symbols[0].clone());
        dec.add_source(3, symbols[3].clone());
        dec.add_coded(&repair1);
        dec.add_coded(&repair2);

        let mut recovered = dec.try_recover();
        recovered.sort_by_key(|(seq, _)| *seq);
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0].0, 1);
        assert_eq!(&recovered[0].1[..12], &symbols[1][..]);
        assert_eq!(recovered[1].0, 2);
        assert_eq!(&recovered[1].1[..12], &symbols[2][..]);
    }

    // ─── No Loss ────────────────────────────────────────────────────────

    #[test]
    fn no_recovery_needed_when_all_received() {
        let symbols: Vec<Bytes> = (0..4u64).map(|i| Bytes::from(vec![i as u8; 8])).collect();

        let mut dec = RlncDecoder::new();
        for (i, sym) in symbols.iter().enumerate() {
            dec.add_source(i as u64, sym.clone());
        }

        let recovered = dec.try_recover();
        assert!(recovered.is_empty());
        assert_eq!(dec.known_count(), 4);
    }

    // ─── Insufficient Rank ──────────────────────────────────────────────

    #[test]
    fn cannot_recover_without_enough_repairs() {
        let symbols: Vec<Bytes> = (0..4u64)
            .map(|i| Bytes::from(vec![(i + 1) as u8; 8]))
            .collect();

        let mut enc = RlncEncoder::new(8, 789);
        for (i, sym) in symbols.iter().enumerate() {
            enc.add_source(i as u64, sym.clone());
        }
        let repair = enc.generate_repair().unwrap();

        let mut dec = RlncDecoder::new();
        // Only receive symbol 0, lose 1, 2, 3 — but only 1 repair
        dec.add_source(0, symbols[0].clone());
        dec.add_coded(&repair);

        let recovered = dec.try_recover();
        // With 3 unknowns and 1 equation, can't solve
        assert!(recovered.len() < 3);
    }

    // ─── Variable Symbol Sizes ──────────────────────────────────────────

    #[test]
    fn handles_variable_length_symbols() {
        let symbols = [
            Bytes::from(vec![0xAA; 4]),
            Bytes::from(vec![0xBB; 8]),
            Bytes::from(vec![0xCC; 6]),
        ];

        let mut enc = RlncEncoder::new(8, 999);
        for (i, sym) in symbols.iter().enumerate() {
            enc.add_source(i as u64, sym.clone());
        }
        let repair = enc.generate_repair().unwrap();
        // Repair data length should be max(4, 8, 6) = 8
        assert_eq!(repair.data.len(), 8);

        let mut dec = RlncDecoder::new();
        dec.add_source(0, symbols[0].clone());
        // symbol 1 lost
        dec.add_source(2, symbols[2].clone());
        dec.add_coded(&repair);

        let recovered = dec.try_recover();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].0, 1);
        // First 8 bytes should match original
        assert_eq!(&recovered[0].1[..8], &symbols[1][..]);
    }

    // ─── Window Sliding ─────────────────────────────────────────────────

    #[test]
    fn decoder_has_symbol() {
        let mut dec = RlncDecoder::new();
        assert!(!dec.has_symbol(0));
        dec.add_source(0, Bytes::from(vec![1, 2, 3]));
        assert!(dec.has_symbol(0));
        assert!(!dec.has_symbol(1));
    }
}
