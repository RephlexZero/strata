#![no_main]

use libfuzzer_sys::fuzz_target;
use strata_transport::wire::{ControlBody, Packet, PacketHeader, VarInt};

/// Fuzz the complete wire format parsing pipeline.
///
/// This target exercises:
/// - VarInt::decode with arbitrary bytes
/// - PacketHeader::decode with arbitrary bytes
/// - Packet::decode (header + payload extraction)
/// - ControlBody::decode (control subtype dispatch)
///
/// The parser must never panic on any input — only return None for invalid data.
fuzz_target!(|data: &[u8]| {
    // 1. VarInt decode — must not panic
    let _ = VarInt::decode(&mut &data[..]);

    // 2. PacketHeader decode — must not panic
    let _ = PacketHeader::decode(&mut &data[..]);

    // 3. Full Packet decode — must not panic
    let _ = Packet::decode(&mut &data[..]);

    // 4. ControlBody decode (skip first 8 bytes as if they were a header)
    if data.len() > 8 {
        let _ = ControlBody::decode(&mut &data[8..]);
    }

    // 5. If we can decode a header, verify roundtrip stability
    if let Some(header) = PacketHeader::decode(&mut &data[..]) {
        let mut buf = bytes::BytesMut::new();
        header.encode(&mut buf);
        let re_decoded = PacketHeader::decode(&mut &buf[..]);
        assert!(re_decoded.is_some(), "re-encode/decode must succeed");
        let re = re_decoded.unwrap();
        assert_eq!(re.version, header.version);
        assert_eq!(re.packet_type, header.packet_type);
        assert_eq!(re.sequence.value(), header.sequence.value());
        assert_eq!(re.timestamp_us, header.timestamp_us);
        assert_eq!(re.payload_len, header.payload_len);
    }
});
