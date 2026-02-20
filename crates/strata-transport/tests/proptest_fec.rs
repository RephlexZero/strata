//! Property-based tests for the FEC codec engine.
//!
//! These tests verify FEC encoding/decoding correctness across varied
//! generation sizes, loss patterns, and data contents.

use bytes::Bytes;
use proptest::prelude::*;
use strata_transport::codec::{FecDecoder, FecEncoder, TarotOptimizer};
use strata_transport::wire::{FecRepairHeader, Packet};

// ─── FEC Single-Symbol Recovery ──────────────────────────────────────────────

proptest! {
    /// The XOR-based FEC encoder/decoder should recover any single lost symbol
    /// from a generation when repair symbol 0 is available.
    #[test]
    fn fec_recovers_single_loss(
        k in 2usize..=16,
        missing_idx in 0usize..16,
        symbol_len in 1usize..=256,
        seed in any::<u64>(),
    ) {
        let missing_idx = missing_idx % k;

        // Generate K distinct source symbols
        let symbols: Vec<Bytes> = (0..k)
            .map(|i| {
                let data: Vec<u8> = (0..symbol_len)
                    .map(|j| ((i as u64).wrapping_mul(31).wrapping_add(j as u64).wrapping_add(seed)) as u8)
                    .collect();
                Bytes::from(data)
            })
            .collect();

        // Encode: feed all K symbols to get repair packets
        let mut encoder = FecEncoder::new(k, 1);
        let mut repairs = Vec::new();
        for (i, sym) in symbols.iter().enumerate() {
            let r = encoder.add_source_symbol(i as u64, sym.clone());
            repairs.extend(r);
        }
        prop_assert!(!repairs.is_empty(), "encoder should emit repairs after K symbols");

        // Parse the actual RLNC repair packet (uses GF(2^8) coding coefficients,
        // not plain XOR — the decoder must use the same coefficient function).
        let mut buf = repairs[0].clone();
        let pkt = Packet::decode(&mut buf);
        prop_assume!(pkt.is_some());
        let pkt = pkt.unwrap();
        let mut payload = pkt.payload;
        let _subtype = payload.split_to(1);
        let fec_hdr = FecRepairHeader::decode(&mut payload);
        prop_assume!(fec_hdr.is_some());
        let fec_hdr = fec_hdr.unwrap();
        let repair_data = payload.to_vec();

        // Decode: feed all symbols except the missing one, plus the RLNC repair
        let mut decoder = FecDecoder::new(16);
        for (i, sym) in symbols.iter().enumerate().take(k) {
            if i == missing_idx {
                continue; // simulate loss
            }
            decoder.add_source_symbol(0, i, k, 1, sym.clone());
        }
        decoder.add_repair_symbol(&fec_hdr, repair_data);

        // Try recovery
        let recovered = decoder.try_recover(0);
        prop_assert_eq!(recovered.len(), 1, "should recover exactly 1 symbol");
        let (idx, data) = &recovered[0];
        prop_assert_eq!(*idx, missing_idx);
        for (j, &orig_byte) in symbols[missing_idx].iter().enumerate() {
            prop_assert_eq!(
                data[j], orig_byte,
                "mismatch at byte {} of recovered symbol", j
            );
        }
    }
}

// ─── FEC Cannot Recover Multiple Losses ──────────────────────────────────────

proptest! {
    /// With only 1 repair symbol, the decoder cannot recover 2+ lost symbols.
    #[test]
    fn fec_cannot_recover_double_loss(
        k in 3usize..=16,
        symbol_len in 4usize..=64,
    ) {
        // Generate K symbols
        let symbols: Vec<Bytes> = (0..k)
            .map(|i| Bytes::from(vec![(i + 1) as u8; symbol_len]))
            .collect();

        // Encode through the actual RLNC encoder (1 repair symbol)
        let mut encoder = FecEncoder::new(k, 1);
        let mut repair_packets = Vec::new();
        for (i, sym) in symbols.iter().enumerate() {
            let repairs = encoder.add_source_symbol(i as u64, sym.clone());
            repair_packets.extend(repairs);
        }
        prop_assume!(!repair_packets.is_empty());

        // Parse the repair packet
        let mut buf = repair_packets[0].clone();
        let pkt = Packet::decode(&mut buf);
        prop_assume!(pkt.is_some());
        let pkt = pkt.unwrap();
        let mut payload = pkt.payload;
        let _subtype = payload.split_to(1);
        let fec_hdr = FecRepairHeader::decode(&mut payload);
        prop_assume!(fec_hdr.is_some());
        let fec_hdr = fec_hdr.unwrap();
        let repair_data = payload.to_vec();

        let mut decoder = FecDecoder::new(16);
        // Feed all except first two (2 losses)
        for (i, sym) in symbols.iter().enumerate().take(k).skip(2) {
            decoder.add_source_symbol(0, i, k, 1, sym.clone());
        }
        decoder.add_repair_symbol(&fec_hdr, repair_data);

        let recovered = decoder.try_recover(0);
        prop_assert!(
            recovered.is_empty(),
            "should not recover with 2 missing symbols and 1 repair"
        );
    }
}

// ─── FEC Complete Generation Needs No Recovery ───────────────────────────────

proptest! {
    /// When all K source symbols are received, is_complete returns true
    /// and try_recover returns nothing.
    #[test]
    fn fec_complete_generation(
        k in 2usize..=32,
        symbol_len in 1usize..=128,
    ) {
        let mut decoder = FecDecoder::new(16);

        for i in 0..k {
            decoder.add_source_symbol(
                0,
                i,
                k,
                1,
                Bytes::from(vec![i as u8; symbol_len]),
            );
        }

        prop_assert!(decoder.is_complete(0));
        let recovered = decoder.try_recover(0);
        prop_assert!(recovered.is_empty(), "no recovery needed when complete");
    }
}

// ─── Encoder Generation Cycling ──────────────────────────────────────────────

proptest! {
    /// The encoder should advance generation IDs as groups of K symbols
    /// are accumulated.
    #[test]
    fn encoder_advances_generations(
        k in 2usize..=16,
        num_generations in 1usize..=8,
    ) {
        let mut encoder = FecEncoder::new(k, 1);
        let mut gen_repairs = 0usize;

        for seq in 0..(k * num_generations) {
            let repairs = encoder.add_source_symbol(
                seq as u64,
                Bytes::from(vec![seq as u8; 10]),
            );
            if !repairs.is_empty() {
                gen_repairs += 1;
            }
        }

        prop_assert_eq!(gen_repairs, num_generations);
        prop_assert_eq!(encoder.current_generation(), num_generations as u16);
        prop_assert_eq!(encoder.buffered_count(), 0);
    }

    /// Flush emits repair for partial generations.
    #[test]
    fn encoder_flush_partial(
        k in 3usize..=16,
        partial in 1usize..=15,
    ) {
        let partial = partial % (k - 1) + 1; // ensure 1..k-1
        let mut encoder = FecEncoder::new(k, 2);

        for i in 0..partial {
            let _ = encoder.add_source_symbol(i as u64, Bytes::from(vec![i as u8; 20]));
        }

        prop_assert_eq!(encoder.buffered_count(), partial);

        let repairs = encoder.flush();
        prop_assert!(!repairs.is_empty(), "flush should emit repairs");
        prop_assert_eq!(encoder.buffered_count(), 0);
    }
}

// ─── TAROT Optimizer Properties ──────────────────────────────────────────────

proptest! {
    /// TAROT should always return R in [1, K/2].
    #[test]
    fn tarot_result_bounded(
        loss_rate in 0.0f64..=1.0,
        rtt_ms in 1.0f64..=500.0,
        k in 4usize..=64,
    ) {
        let opt = TarotOptimizer::new();
        let r = opt.compute_optimal_r(loss_rate, rtt_ms, k);
        prop_assert!(r >= 1, "R must be >= 1, got {r}");
        prop_assert!(r <= k / 2, "R must be <= K/2 = {}, got {r}", k / 2);
    }

    /// Higher loss rate should never decrease the recommended FEC level.
    #[test]
    fn tarot_monotonic_with_loss(
        rtt_ms in 10.0f64..=200.0,
        k in 8usize..=64,
    ) {
        let opt = TarotOptimizer::new();
        let mut prev_r = 0usize;
        for loss_pct in [0, 1, 2, 5, 10, 20, 30, 50] {
            let loss_rate = loss_pct as f64 / 100.0;
            let r = opt.compute_optimal_r(loss_rate, rtt_ms, k);
            prop_assert!(
                r >= prev_r,
                "TAROT should be monotonic: at {loss_pct}% loss, R={r} < prev R={prev_r}"
            );
            prev_r = r;
        }
    }
}
