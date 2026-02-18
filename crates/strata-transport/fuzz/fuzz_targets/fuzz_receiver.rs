#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use strata_transport::receiver::{Receiver, ReceiverConfig};

/// Fuzz the receiver state machine with arbitrary wire-format bytes.
///
/// This target exercises:
/// - Packet parsing inside Receiver::receive()
/// - Loss detection (gap tracking, NACK generation)
/// - FEC repair processing
/// - Reorder buffer insertion and delivery
/// - Duplicate suppression
/// - Control packet handling (ACK, NACK, Ping, Session, etc.)
///
/// The receiver must never panic, even on garbage input.
fuzz_target!(|data: &[u8]| {
    let mut rx = Receiver::new(ReceiverConfig {
        reorder_capacity: 32,
        max_fec_generations: 8,
        nack_rearm_ms: 50,
        max_nack_retries: 3,
    });

    // Feed data as a single packet
    rx.receive(Bytes::copy_from_slice(data));

    // Drain any events produced
    for _ in rx.drain_events() {}

    // If the input is long enough, try splitting it into multiple packets
    // to exercise stateful interactions (loss detection, reordering)
    if data.len() >= 16 {
        let mut rx2 = Receiver::new(ReceiverConfig {
            reorder_capacity: 32,
            max_fec_generations: 8,
            nack_rearm_ms: 50,
            max_nack_retries: 3,
        });

        // Feed data in 2 chunks to exercise gap detection
        let mid = data.len() / 2;
        rx2.receive(Bytes::copy_from_slice(&data[..mid]));
        rx2.receive(Bytes::copy_from_slice(&data[mid..]));

        for _ in rx2.drain_events() {}
    }
});
