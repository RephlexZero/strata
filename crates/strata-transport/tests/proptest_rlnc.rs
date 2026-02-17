//! Property-based tests for the sliding-window RLNC codec.

use bytes::Bytes;
use proptest::prelude::*;
use strata_transport::rlnc::{RlncDecoder, RlncEncoder};

// ─── RLNC Single-Loss Recovery ──────────────────────────────────────────────

proptest! {
    /// With N source symbols and 1 coded symbol, we can recover any single loss.
    #[test]
    fn rlnc_recovers_single_loss(
        n in 2usize..=16,
        missing in 0usize..16,
        symbol_len in 1usize..=64,
        seed in any::<u64>(),
    ) {
        let missing = missing % n;
        let symbols: Vec<Bytes> = (0..n)
            .map(|i| {
                Bytes::from(
                    (0..symbol_len)
                        .map(|j| ((i as u64).wrapping_mul(37).wrapping_add(j as u64).wrapping_add(seed)) as u8)
                        .collect::<Vec<u8>>(),
                )
            })
            .collect();

        let mut enc = RlncEncoder::new(32, seed);
        for (i, sym) in symbols.iter().enumerate() {
            enc.add_source(i as u64, sym.clone());
        }
        let repair = enc.generate_repair().unwrap();

        let mut dec = RlncDecoder::new();
        for (i, sym) in symbols.iter().enumerate() {
            if i != missing {
                dec.add_source(i as u64, sym.clone());
            }
        }
        dec.add_coded(&repair);

        let recovered = dec.try_recover();
        prop_assert_eq!(recovered.len(), 1, "should recover exactly 1 symbol");
        prop_assert_eq!(recovered[0].0, missing as u64);
        for (j, &orig) in symbols[missing].iter().enumerate() {
            prop_assert_eq!(
                recovered[0].1[j], orig,
                "mismatch at byte {} of recovered symbol", j
            );
        }
    }
}

// ─── RLNC Double-Loss Recovery ──────────────────────────────────────────────

proptest! {
    /// With N source symbols and extra coded symbols, we can recover 2 losses.
    /// We generate `losses + 2` repair symbols to make rank deficiency
    /// astronomically unlikely over GF(256).
    #[test]
    fn rlnc_recovers_double_loss(
        n in 3usize..=12,
        miss_a in 0usize..12,
        miss_b in 0usize..12,
        symbol_len in 4usize..=32,
        seed in any::<u64>(),
    ) {
        let miss_a = miss_a % n;
        let mut miss_b = miss_b % n;
        if miss_b == miss_a {
            miss_b = (miss_a + 1) % n;
        }

        let symbols: Vec<Bytes> = (0..n)
            .map(|i| {
                Bytes::from(
                    (0..symbol_len)
                        .map(|j| ((i as u64).wrapping_mul(47).wrapping_add(j as u64).wrapping_add(seed)) as u8)
                        .collect::<Vec<u8>>(),
                )
            })
            .collect();

        let mut enc = RlncEncoder::new(32, seed);
        for (i, sym) in symbols.iter().enumerate() {
            enc.add_source(i as u64, sym.clone());
        }
        // Generate 4 repair symbols (2 losses + 2 extra) to virtually
        // eliminate the chance of a rank-deficient coefficient matrix.
        let repairs: Vec<_> = (0..4).map(|_| enc.generate_repair().unwrap()).collect();

        let mut dec = RlncDecoder::new();
        for (i, sym) in symbols.iter().enumerate() {
            if i != miss_a && i != miss_b {
                dec.add_source(i as u64, sym.clone());
            }
        }
        for r in &repairs {
            dec.add_coded(r);
        }

        let mut recovered = dec.try_recover();
        recovered.sort_by_key(|(seq, _)| *seq);

        prop_assert_eq!(recovered.len(), 2, "should recover exactly 2 symbols");

        let mut expected = [miss_a as u64, miss_b as u64];
        expected.sort();

        prop_assert_eq!(recovered[0].0, expected[0]);
        prop_assert_eq!(recovered[1].0, expected[1]);

        for (j, &orig) in symbols[expected[0] as usize].iter().enumerate() {
            prop_assert_eq!(recovered[0].1[j], orig);
        }
        for (j, &orig) in symbols[expected[1] as usize].iter().enumerate() {
            prop_assert_eq!(recovered[1].1[j], orig);
        }
    }
}

// ─── RLNC No Recovery When All Received ─────────────────────────────────────

proptest! {
    #[test]
    fn rlnc_no_recovery_when_complete(
        n in 1usize..=16,
        symbol_len in 1usize..=32,
    ) {
        let symbols: Vec<Bytes> = (0..n)
            .map(|i| Bytes::from(vec![i as u8; symbol_len]))
            .collect();

        let mut dec = RlncDecoder::new();
        for (i, sym) in symbols.iter().enumerate() {
            dec.add_source(i as u64, sym.clone());
        }
        let recovered = dec.try_recover();
        prop_assert!(recovered.is_empty());
        prop_assert_eq!(dec.known_count(), n);
    }
}

// ─── RLNC Encoder Window Properties ────────────────────────────────────────

proptest! {
    #[test]
    fn rlnc_window_respects_max_size(
        window_size in 2usize..=32,
        num_symbols in 1usize..=64,
    ) {
        let mut enc = RlncEncoder::new(window_size, 42);
        for i in 0..num_symbols as u64 {
            enc.add_source(i, Bytes::from(vec![i as u8; 4]));
        }
        prop_assert!(enc.window_len() <= window_size);
    }

    #[test]
    fn rlnc_repair_coefficients_nonzero(
        n in 1usize..=16,
        seed in any::<u64>(),
    ) {
        let mut enc = RlncEncoder::new(32, seed);
        for i in 0..n as u64 {
            enc.add_source(i, Bytes::from(vec![i as u8; 8]));
        }
        let repair = enc.generate_repair().unwrap();
        for &c in &repair.coefficients {
            prop_assert_ne!(c, 0, "all RLNC coefficients must be nonzero");
        }
    }
}
