#![no_main]

use libfuzzer_sys::fuzz_target;
use strata_transport::wire::{ControlBody, Packet, PacketHeader, VarInt};

/// Fuzz the entire packet decode pipeline.
///
/// This target exercises:
/// - VarInt::decode (variable-length integer parsing)
/// - PacketHeader::decode (flags, payload length, sequence, timestamp)
/// - Packet::decode (header + payload extraction)
/// - ControlBody::decode (typed control packet dispatch)
///
/// The decoder must never panic on any input; it should return `None`
/// for malformed data.
fuzz_target!(|data: &[u8]| {
    // 1. Try decoding a full Packet from raw bytes
    let mut buf = data;
    let _ = Packet::decode(&mut buf);

    // 2. Try decoding just a PacketHeader
    let mut buf = data;
    let _ = PacketHeader::decode(&mut buf);

    // 3. Try decoding a VarInt
    let mut buf = data;
    let _ = VarInt::decode(&mut buf);

    // 4. Try decoding a control body (subtype dispatch)
    let mut buf = data;
    let _ = ControlBody::decode(&mut buf);
});
