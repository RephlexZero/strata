//! Property-based tests for the Strata wire format.
//!
//! These tests verify roundtrip correctness for VarInt encoding, packet headers,
//! and all control packet types across the full value range.

use bytes::{Buf, Bytes, BytesMut};
use proptest::prelude::*;
use strata_transport::wire::*;

// ─── VarInt Roundtrip ────────────────────────────────────────────────────────

/// Strategy that generates valid VarInt values (0..2^62 - 1).
fn varint_value() -> impl Strategy<Value = u64> {
    prop_oneof![
        // 1-byte range: 0..0x3F
        0u64..0x40,
        // 2-byte range: 0x40..0x3FFF
        0x40u64..0x4000,
        // 4-byte range: 0x4000..0x3FFF_FFFF
        0x4000u64..0x4000_0000,
        // 8-byte range: 0x4000_0000..VarInt::MAX
        0x4000_0000u64..VarInt::MAX,
    ]
}

/// Strategy that specifically targets VarInt encoding boundaries.
fn varint_boundary() -> impl Strategy<Value = u64> {
    prop_oneof![
        Just(0u64),
        Just(0x3F),        // max 1-byte
        Just(0x40),        // min 2-byte
        Just(0x3FFF),      // max 2-byte
        Just(0x4000),      // min 4-byte
        Just(0x3FFF_FFFF), // max 4-byte
        Just(0x4000_0000), // min 8-byte
        Just(VarInt::MAX), // max 8-byte (2^62 - 1)
    ]
}

proptest! {
    #[test]
    fn varint_roundtrip(val in varint_value()) {
        let vi = VarInt::from_u64(val);
        let mut buf = BytesMut::new();
        vi.encode(&mut buf);

        // Verify encoded length matches prediction
        let expected_len = vi.encoded_len();
        prop_assert_eq!(buf.len(), expected_len);

        // Decode and verify value is preserved
        let decoded = VarInt::decode(&mut buf.freeze()).unwrap();
        prop_assert_eq!(decoded.value(), val);
    }

    #[test]
    fn varint_boundary_roundtrip(val in varint_boundary()) {
        let vi = VarInt::from_u64(val);
        let mut buf = BytesMut::new();
        vi.encode(&mut buf);
        let decoded = VarInt::decode(&mut buf.freeze()).unwrap();
        prop_assert_eq!(decoded.value(), val);
    }

    #[test]
    fn varint_encoding_length_is_correct(val in varint_value()) {
        let vi = VarInt::from_u64(val);
        let expected = if val < 0x40 { 1 }
                       else if val < 0x4000 { 2 }
                       else if val < 0x4000_0000 { 4 }
                       else { 8 };
        prop_assert_eq!(vi.encoded_len(), expected);
    }

    #[test]
    fn varint_rejects_values_above_max(val in (VarInt::MAX + 1)..=u64::MAX) {
        prop_assert!(VarInt::new(val).is_none());
    }

    #[test]
    fn varint_multiple_roundtrip(
        a in varint_value(),
        b in varint_value(),
        c in varint_value()
    ) {
        // Encode three VarInts back-to-back, then decode them in order
        let mut buf = BytesMut::new();
        VarInt::from_u64(a).encode(&mut buf);
        VarInt::from_u64(b).encode(&mut buf);
        VarInt::from_u64(c).encode(&mut buf);

        let mut readable = buf.freeze();
        let da = VarInt::decode(&mut readable).unwrap();
        let db = VarInt::decode(&mut readable).unwrap();
        let dc = VarInt::decode(&mut readable).unwrap();

        prop_assert_eq!(da.value(), a);
        prop_assert_eq!(db.value(), b);
        prop_assert_eq!(dc.value(), c);
        prop_assert_eq!(readable.remaining(), 0);
    }
}

// ─── Packet Header Roundtrip ─────────────────────────────────────────────────

fn fragment_strategy() -> impl Strategy<Value = Fragment> {
    prop_oneof![
        Just(Fragment::Complete),
        Just(Fragment::Start),
        Just(Fragment::Middle),
        Just(Fragment::End),
    ]
}

proptest! {
    #[test]
    fn packet_header_roundtrip(
        seq in varint_value(),
        timestamp in any::<u32>(),
        payload_len in any::<u16>(),
        fragment in fragment_strategy(),
        is_keyframe in any::<bool>(),
        is_config in any::<bool>(),
    ) {
        let header = PacketHeader {
            version: PROTOCOL_VERSION,
            packet_type: PacketType::Data,
            fragment,
            is_keyframe,
            is_config,
            payload_len,
            sequence: VarInt::from_u64(seq),
            timestamp_us: timestamp,
        };

        let mut buf = BytesMut::new();
        header.encode(&mut buf);
        let decoded = PacketHeader::decode(&mut buf.freeze()).unwrap();

        prop_assert_eq!(decoded.version, PROTOCOL_VERSION);
        prop_assert_eq!(decoded.packet_type, PacketType::Data);
        prop_assert_eq!(decoded.fragment, fragment);
        prop_assert_eq!(decoded.is_keyframe, is_keyframe);
        prop_assert_eq!(decoded.is_config, is_config);
        prop_assert_eq!(decoded.payload_len, payload_len);
        prop_assert_eq!(decoded.sequence.value(), seq);
        prop_assert_eq!(decoded.timestamp_us, timestamp);
    }

    #[test]
    fn full_data_packet_roundtrip(
        seq in varint_value(),
        timestamp in any::<u32>(),
        payload_len in 0usize..1024,
    ) {
        let payload: Vec<u8> = (0..payload_len).map(|i| (i % 256) as u8).collect();
        let payload = Bytes::from(payload);

        let pkt = Packet::new_data(seq, timestamp, payload.clone());
        let encoded = pkt.encode();
        let decoded = Packet::decode(&mut encoded.freeze()).unwrap();

        prop_assert_eq!(decoded.header.sequence.value(), seq);
        prop_assert_eq!(decoded.header.timestamp_us, timestamp);
        prop_assert_eq!(decoded.payload, payload);
    }
}

// ─── ACK Packet Roundtrip ────────────────────────────────────────────────────

proptest! {
    #[test]
    fn ack_roundtrip(
        cumulative in varint_value(),
        bitmap in any::<u64>(),
    ) {
        let ack = AckPacket {
            cumulative_seq: VarInt::from_u64(cumulative),
            sack_bitmap: bitmap,
        };

        let mut buf = BytesMut::new();
        ack.encode(&mut buf);
        let _ = buf.split_to(1); // skip subtype byte
        let decoded = AckPacket::decode(&mut buf).unwrap();

        prop_assert_eq!(decoded.cumulative_seq.value(), cumulative);
        prop_assert_eq!(decoded.sack_bitmap, bitmap);
    }

    #[test]
    fn sack_iterator_produces_correct_sequences(
        base in 0u64..1_000_000,
        bitmap in any::<u64>(),
    ) {
        let ack = AckPacket {
            cumulative_seq: VarInt::from_u64(base),
            sack_bitmap: bitmap,
        };

        let sacked: Vec<u64> = ack.sacked_sequences().collect();

        // Every returned sequence should correspond to a set bit
        for &seq in &sacked {
            let offset = seq - base - 1;
            prop_assert!(offset < 64, "offset out of range: {offset}");
            prop_assert!((bitmap >> offset) & 1 == 1,
                "seq {seq} returned but bit {offset} is not set");
        }

        // Count should match popcount
        prop_assert_eq!(sacked.len(), bitmap.count_ones() as usize);
    }
}

// ─── NACK Packet Roundtrip ───────────────────────────────────────────────────

proptest! {
    #[test]
    fn nack_roundtrip(
        starts in prop::collection::vec(0u64..1_000_000, 1..8),
        counts in prop::collection::vec(1u64..100, 1..8),
    ) {
        let len = starts.len().min(counts.len());
        let ranges: Vec<NackRange> = starts.into_iter().zip(counts)
            .take(len)
            .map(|(s, c)| NackRange {
                start: VarInt::from_u64(s),
                count: VarInt::from_u64(c),
            })
            .collect();

        let nack = NackPacket { ranges: ranges.clone() };

        let mut buf = BytesMut::new();
        nack.encode(&mut buf);
        let _ = buf.split_to(1); // skip subtype byte
        let decoded = NackPacket::decode(&mut buf).unwrap();

        prop_assert_eq!(decoded.ranges.len(), ranges.len());
        for (orig, dec) in ranges.iter().zip(decoded.ranges.iter()) {
            prop_assert_eq!(orig.start.value(), dec.start.value());
            prop_assert_eq!(orig.count.value(), dec.count.value());
        }
    }
}

// ─── Ping/Pong Roundtrip ────────────────────────────────────────────────────

proptest! {
    #[test]
    fn ping_roundtrip(
        origin_ts in any::<u32>(),
        ping_id in any::<u16>(),
    ) {
        let ping = PingPacket { origin_timestamp_us: origin_ts, ping_id };
        let mut buf = BytesMut::new();
        ping.encode(&mut buf);
        let _ = buf.split_to(1);
        let decoded = PingPacket::decode(&mut buf).unwrap();
        prop_assert_eq!(decoded.origin_timestamp_us, origin_ts);
        prop_assert_eq!(decoded.ping_id, ping_id);
    }

    #[test]
    fn pong_roundtrip(
        origin_ts in any::<u32>(),
        ping_id in any::<u16>(),
        recv_ts in any::<u32>(),
    ) {
        let pong = PongPacket {
            origin_timestamp_us: origin_ts,
            ping_id,
            receive_timestamp_us: recv_ts,
        };
        let mut buf = BytesMut::new();
        pong.encode(&mut buf);
        let _ = buf.split_to(1);
        let decoded = PongPacket::decode(&mut buf).unwrap();
        prop_assert_eq!(decoded.origin_timestamp_us, origin_ts);
        prop_assert_eq!(decoded.ping_id, ping_id);
        prop_assert_eq!(decoded.receive_timestamp_us, recv_ts);
    }
}

// ─── Session Packet Roundtrip ────────────────────────────────────────────────

fn session_action_strategy() -> impl Strategy<Value = SessionAction> {
    prop_oneof![
        Just(SessionAction::Hello),
        Just(SessionAction::Accept),
        Just(SessionAction::Teardown),
        Just(SessionAction::LinkJoin),
        Just(SessionAction::LinkLeave),
    ]
}

proptest! {
    #[test]
    fn session_roundtrip(
        action in session_action_strategy(),
        session_id in any::<u64>(),
        has_link_id in any::<bool>(),
        link_id_val in any::<u8>(),
    ) {
        let link_id = if has_link_id { Some(link_id_val) } else { None };
        let session = SessionPacket { action, session_id, link_id };

        let mut buf = BytesMut::new();
        session.encode(&mut buf);
        let _ = buf.split_to(1); // skip subtype
        let decoded = SessionPacket::decode(&mut buf).unwrap();

        prop_assert_eq!(decoded.action, action);
        prop_assert_eq!(decoded.session_id, session_id);
        prop_assert_eq!(decoded.link_id, link_id);
    }
}

// ─── Link Report Roundtrip ───────────────────────────────────────────────────

proptest! {
    #[test]
    fn link_report_roundtrip(
        link_id in any::<u8>(),
        rtt_us in any::<u32>(),
        loss_rate in any::<u16>(),
        capacity_kbps in any::<u32>(),
        sinr_db10 in any::<i16>(),
    ) {
        let report = LinkReport {
            link_id,
            rtt_us,
            loss_rate,
            capacity_kbps,
            sinr_db10,
        };

        let mut buf = BytesMut::new();
        report.encode(&mut buf);
        let _ = buf.split_to(1); // skip subtype
        let decoded = LinkReport::decode(&mut buf).unwrap();

        prop_assert_eq!(decoded.link_id, link_id);
        prop_assert_eq!(decoded.rtt_us, rtt_us);
        prop_assert_eq!(decoded.loss_rate, loss_rate);
        prop_assert_eq!(decoded.capacity_kbps, capacity_kbps);
        prop_assert_eq!(decoded.sinr_db10, sinr_db10);
    }
}

// ─── Bitrate Command Roundtrip ───────────────────────────────────────────────

fn bitrate_reason_strategy() -> impl Strategy<Value = BitrateReason> {
    prop_oneof![
        Just(BitrateReason::Capacity),
        Just(BitrateReason::Congestion),
        Just(BitrateReason::LinkFailure),
        Just(BitrateReason::Recovery),
    ]
}

proptest! {
    #[test]
    fn bitrate_cmd_roundtrip(
        target_kbps in any::<u32>(),
        reason in bitrate_reason_strategy(),
    ) {
        let cmd = BitrateCmd { target_kbps, reason };

        let mut buf = BytesMut::new();
        cmd.encode(&mut buf);
        let _ = buf.split_to(1); // skip subtype
        let decoded = BitrateCmd::decode(&mut buf).unwrap();

        prop_assert_eq!(decoded.target_kbps, target_kbps);
        prop_assert_eq!(decoded.reason, reason);
    }
}

// ─── FEC Repair Header Roundtrip ─────────────────────────────────────────────

proptest! {
    #[test]
    fn fec_repair_header_roundtrip(
        generation_id in any::<u16>(),
        symbol_index in any::<u8>(),
        k in 1u8..=255,
        r in 1u8..=255,
    ) {
        let header = FecRepairHeader {
            generation_id,
            symbol_index,
            k,
            r,
        };

        let mut buf = BytesMut::new();
        header.encode(&mut buf);
        let _ = buf.split_to(1); // skip subtype byte
        let decoded = FecRepairHeader::decode(&mut buf).unwrap();

        prop_assert_eq!(decoded.generation_id, generation_id);
        prop_assert_eq!(decoded.symbol_index, symbol_index);
        prop_assert_eq!(decoded.k, k);
        prop_assert_eq!(decoded.r, r);
    }
}
