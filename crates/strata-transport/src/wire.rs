//! # Strata Wire Format
//!
//! Custom lightweight packet header — no RTP dependency.
//!
//! ## Data Packet Header (variable 7-15 bytes)
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |V=1|T| F |K|C|R|          Payload Length (16)                   |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                    Sequence Number (VarInt, 1-8 bytes)         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                    Timestamp (32-bit, µs)                      |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! ## Control packets (T=1) carry a 1-byte subtype after the base header.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::fmt;

// ─── Constants ───────────────────────────────────────────────────────────────

/// Protocol version.
pub const PROTOCOL_VERSION: u8 = 1;

/// Minimum header size: 1 (flags) + 2 (payload len) + 1 (min varint) + 4 (timestamp) = 8.
pub const MIN_HEADER_SIZE: usize = 8;

/// Maximum header size: 1 + 2 + 8 + 4 = 15.
pub const MAX_HEADER_SIZE: usize = 15;

/// Maximum payload in a single packet (64 KiB - 1).
pub const MAX_PAYLOAD_LEN: usize = u16::MAX as usize;

// ─── VarInt (QUIC-style, RFC 9000 §16) ──────────────────────────────────────

/// A 62-bit variable-length integer encoded in 1, 2, 4, or 8 bytes.
///
/// Encoding:
/// - `0x00..0x3F` → 1 byte  (6 bits)
/// - `0x40..0x3FFF` → 2 bytes (14 bits), prefix `01`
/// - `0x4000..0x3FFF_FFFF` → 4 bytes (30 bits), prefix `10`
/// - `0x4000_0000..0x3FFF_FFFF_FFFF_FFFF` → 8 bytes (62 bits), prefix `11`
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VarInt(u64);

impl VarInt {
    /// Maximum representable value: 2^62 - 1.
    pub const MAX: u64 = (1 << 62) - 1;

    /// Create a VarInt, returning `None` if the value exceeds 62 bits.
    #[inline]
    pub fn new(val: u64) -> Option<Self> {
        if val <= Self::MAX {
            Some(VarInt(val))
        } else {
            None
        }
    }

    /// Create a VarInt from a u64, panicking if out of range.
    #[inline]
    pub fn from_u64(val: u64) -> Self {
        Self::new(val).expect("VarInt value exceeds 62-bit limit")
    }

    /// Get the underlying u64 value.
    #[inline]
    pub fn value(self) -> u64 {
        self.0
    }

    /// Number of bytes this value encodes to.
    #[inline]
    pub fn encoded_len(self) -> usize {
        if self.0 < 0x40 {
            1
        } else if self.0 < 0x4000 {
            2
        } else if self.0 < 0x4000_0000 {
            4
        } else {
            8
        }
    }

    /// Encode into a mutable buffer. Panics if insufficient space.
    pub fn encode(&self, buf: &mut impl BufMut) {
        match self.encoded_len() {
            1 => buf.put_u8(self.0 as u8),
            2 => buf.put_u16(0x4000 | self.0 as u16),
            4 => buf.put_u32(0x8000_0000 | self.0 as u32),
            8 => buf.put_u64(0xC000_0000_0000_0000 | self.0),
            _ => unreachable!(),
        }
    }

    /// Decode from a buffer. Returns `None` if buffer is too short or value is invalid.
    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if !buf.has_remaining() {
            return None;
        }

        let first = buf.chunk()[0];
        let prefix = first >> 6;

        let len = 1usize << prefix;
        if buf.remaining() < len {
            return None;
        }

        let val = match len {
            1 => {
                buf.advance(1);
                (first & 0x3F) as u64
            }
            2 => {
                let raw = buf.get_u16();
                (raw & 0x3FFF) as u64
            }
            4 => {
                let raw = buf.get_u32();
                (raw & 0x3FFF_FFFF) as u64
            }
            8 => {
                let raw = buf.get_u64();
                raw & 0x3FFF_FFFF_FFFF_FFFF
            }
            _ => unreachable!(),
        };

        Some(VarInt(val))
    }
}

impl fmt::Debug for VarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VarInt({})", self.0)
    }
}

impl fmt::Display for VarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<u32> for VarInt {
    fn from(v: u32) -> Self {
        VarInt(v as u64)
    }
}

impl From<u16> for VarInt {
    fn from(v: u16) -> Self {
        VarInt(v as u64)
    }
}

impl From<u8> for VarInt {
    fn from(v: u8) -> Self {
        VarInt(v as u64)
    }
}

// ─── Packet Type ─────────────────────────────────────────────────────────────

/// Whether the packet carries data or control information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Data = 0,
    Control = 1,
}

// ─── Fragment Flags ──────────────────────────────────────────────────────────

/// Fragmentation status of a data packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Fragment {
    /// Complete packet (not fragmented).
    Complete = 0b00,
    /// First fragment.
    Start = 0b01,
    /// Middle fragment.
    Middle = 0b10,
    /// Last fragment.
    End = 0b11,
}

impl Fragment {
    fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b00 => Fragment::Complete,
            0b01 => Fragment::Start,
            0b10 => Fragment::Middle,
            0b11 => Fragment::End,
            _ => unreachable!(),
        }
    }
}

// ─── Control Subtypes ────────────────────────────────────────────────────────

/// Control packet sub-types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlType {
    Ack = 0x01,
    Nack = 0x02,
    FecRepair = 0x03,
    LinkReport = 0x04,
    BitrateCmd = 0x05,
    Ping = 0x06,
    Pong = 0x07,
    Session = 0x08,
    ReceiverReport = 0x09,
}

impl ControlType {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(ControlType::Ack),
            0x02 => Some(ControlType::Nack),
            0x03 => Some(ControlType::FecRepair),
            0x04 => Some(ControlType::LinkReport),
            0x05 => Some(ControlType::BitrateCmd),
            0x06 => Some(ControlType::Ping),
            0x07 => Some(ControlType::Pong),
            0x08 => Some(ControlType::Session),
            0x09 => Some(ControlType::ReceiverReport),
            _ => None,
        }
    }
}

// ─── Packet Header ──────────────────────────────────────────────────────────

/// Decoded packet header — present on every Strata packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketHeader {
    /// Protocol version (must be 1).
    pub version: u8,
    /// Data or control packet.
    pub packet_type: PacketType,
    /// Fragment status (meaningful for data packets).
    pub fragment: Fragment,
    /// Whether this packet contains a keyframe.
    pub is_keyframe: bool,
    /// Whether this packet contains codec config (SPS/PPS/VPS).
    pub is_config: bool,
    /// Payload length in bytes (after header).
    pub payload_len: u16,
    /// 62-bit sequence number.
    pub sequence: VarInt,
    /// Microsecond timestamp (wraps every ~71 min).
    pub timestamp_us: u32,
}

impl PacketHeader {
    /// Encode the header into a buffer.
    pub fn encode(&self, buf: &mut BytesMut) {
        // Flags byte: VV T FF K C R
        let flags: u8 = ((self.version & 0x03) << 6)
            | ((self.packet_type as u8) << 5)
            | ((self.fragment as u8) << 3)
            | ((self.is_keyframe as u8) << 2)
            | ((self.is_config as u8) << 1);
        buf.put_u8(flags);

        // Payload length (16-bit big endian)
        buf.put_u16(self.payload_len);

        // Sequence number (VarInt)
        self.sequence.encode(buf);

        // Timestamp (32-bit µs)
        buf.put_u32(self.timestamp_us);
    }

    /// Decode a header from a buffer. Returns `None` if buffer is too short or invalid.
    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < MIN_HEADER_SIZE {
            return None;
        }

        let flags = buf.get_u8();
        let version = (flags >> 6) & 0x03;
        if version != PROTOCOL_VERSION {
            return None;
        }

        let packet_type = if (flags >> 5) & 1 == 1 {
            PacketType::Control
        } else {
            PacketType::Data
        };
        let fragment = Fragment::from_bits((flags >> 3) & 0x03);
        let is_keyframe = (flags >> 2) & 1 == 1;
        let is_config = (flags >> 1) & 1 == 1;

        let payload_len = buf.get_u16();
        let sequence = VarInt::decode(buf)?;
        if buf.remaining() < 4 {
            return None;
        }
        let timestamp_us = buf.get_u32();

        Some(PacketHeader {
            version,
            packet_type,
            fragment,
            is_keyframe,
            is_config,
            payload_len,
            sequence,
            timestamp_us,
        })
    }

    /// Total encoded size of this header.
    pub fn encoded_len(&self) -> usize {
        1 + 2 + self.sequence.encoded_len() + 4
    }

    /// Create a new data packet header.
    pub fn data(sequence: u64, timestamp_us: u32, payload_len: u16) -> Self {
        PacketHeader {
            version: PROTOCOL_VERSION,
            packet_type: PacketType::Data,
            fragment: Fragment::Complete,
            is_keyframe: false,
            is_config: false,
            payload_len,
            sequence: VarInt::from_u64(sequence),
            timestamp_us,
        }
    }

    /// Create a new control packet header.
    pub fn control(sequence: u64, timestamp_us: u32, payload_len: u16) -> Self {
        PacketHeader {
            version: PROTOCOL_VERSION,
            packet_type: PacketType::Control,
            fragment: Fragment::Complete,
            is_keyframe: false,
            is_config: false,
            payload_len,
            sequence: VarInt::from_u64(sequence),
            timestamp_us,
        }
    }

    /// Set this as a keyframe packet.
    pub fn with_keyframe(mut self) -> Self {
        self.is_keyframe = true;
        self
    }

    /// Set this as a codec config packet.
    pub fn with_config(mut self) -> Self {
        self.is_config = true;
        self
    }

    /// Set fragmentation.
    pub fn with_fragment(mut self, frag: Fragment) -> Self {
        self.fragment = frag;
        self
    }
}

// ─── Control Packet Bodies ──────────────────────────────────────────────────

/// ACK packet: cumulative acknowledgment + selective ACK bitmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckPacket {
    /// Highest contiguously received sequence number.
    pub cumulative_seq: VarInt,
    /// Bitmap of received packets beyond cumulative_seq (up to 64 bits).
    /// Bit 0 = cumulative_seq + 1, Bit 1 = cumulative_seq + 2, etc.
    pub sack_bitmap: u64,
}

impl AckPacket {
    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(ControlType::Ack as u8);
        self.cumulative_seq.encode(buf);
        buf.put_u64(self.sack_bitmap);
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        let cumulative_seq = VarInt::decode(buf)?;
        if buf.remaining() < 8 {
            return None;
        }
        let sack_bitmap = buf.get_u64();
        Some(AckPacket {
            cumulative_seq,
            sack_bitmap,
        })
    }

    /// Iterate the specific sequence numbers acknowledged by the SACK bitmap.
    pub fn sacked_sequences(&self) -> impl Iterator<Item = u64> + '_ {
        (0..64).filter_map(move |i| {
            if self.sack_bitmap & (1u64 << i) != 0 {
                Some(self.cumulative_seq.value() + 1 + i)
            } else {
                None
            }
        })
    }
}

/// NACK packet: range-based loss report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackPacket {
    /// List of (start_seq, count) ranges of missing packets.
    pub ranges: Vec<NackRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackRange {
    pub start: VarInt,
    pub count: VarInt,
}

impl NackPacket {
    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(ControlType::Nack as u8);
        VarInt::from_u64(self.ranges.len() as u64).encode(buf);
        for range in &self.ranges {
            range.start.encode(buf);
            range.count.encode(buf);
        }
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        let num_ranges = VarInt::decode(buf)?.value() as usize;
        if num_ranges > 256 {
            return None; // sanity limit
        }
        let mut ranges = Vec::with_capacity(num_ranges);
        for _ in 0..num_ranges {
            let start = VarInt::decode(buf)?;
            let count = VarInt::decode(buf)?;
            ranges.push(NackRange { start, count });
        }
        Some(NackPacket { ranges })
    }
}

/// FEC Repair packet extension header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FecRepairHeader {
    /// Which FEC generation this repair belongs to.
    pub generation_id: u16,
    /// Index of this repair symbol within the generation.
    pub symbol_index: u8,
    /// Number of source symbols in this generation.
    pub k: u8,
    /// Total repair symbols generated.
    pub r: u8,
}

impl FecRepairHeader {
    pub const ENCODED_LEN: usize = 5;

    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(ControlType::FecRepair as u8);
        buf.put_u16(self.generation_id);
        buf.put_u8(self.symbol_index);
        buf.put_u8(self.k);
        buf.put_u8(self.r);
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < Self::ENCODED_LEN {
            return None;
        }
        Some(FecRepairHeader {
            generation_id: buf.get_u16(),
            symbol_index: buf.get_u8(),
            k: buf.get_u8(),
            r: buf.get_u8(),
        })
    }
}

/// Link quality report sent from receiver to sender.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkReport {
    /// Link identifier.
    pub link_id: u8,
    /// Round-trip time in microseconds.
    pub rtt_us: u32,
    /// Observed loss rate (0.0 - 1.0), encoded as u16 (0-10000 = 0.00% - 100.00%).
    pub loss_rate: u16,
    /// Estimated link capacity in kbps.
    pub capacity_kbps: u32,
    /// SINR in dB × 10 (signed). E.g., -50 = -5.0 dB.
    pub sinr_db10: i16,
}

impl LinkReport {
    pub const ENCODED_LEN: usize = 13;

    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(ControlType::LinkReport as u8);
        buf.put_u8(self.link_id);
        buf.put_u32(self.rtt_us);
        buf.put_u16(self.loss_rate);
        buf.put_u32(self.capacity_kbps);
        buf.put_i16(self.sinr_db10);
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < Self::ENCODED_LEN {
            return None;
        }
        Some(LinkReport {
            link_id: buf.get_u8(),
            rtt_us: buf.get_u32(),
            loss_rate: buf.get_u16(),
            capacity_kbps: buf.get_u32(),
            sinr_db10: buf.get_i16(),
        })
    }
}

/// Encoder bitrate adaptation command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitrateCmd {
    /// Target bitrate in kbps.
    pub target_kbps: u32,
    /// Reason code.
    pub reason: BitrateReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BitrateReason {
    /// Normal adaptation.
    Capacity = 0,
    /// Congestion detected.
    Congestion = 1,
    /// Link failure.
    LinkFailure = 2,
    /// Recovery — capacity available.
    Recovery = 3,
}

impl BitrateReason {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(BitrateReason::Capacity),
            1 => Some(BitrateReason::Congestion),
            2 => Some(BitrateReason::LinkFailure),
            3 => Some(BitrateReason::Recovery),
            _ => None,
        }
    }
}

impl BitrateCmd {
    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(ControlType::BitrateCmd as u8);
        buf.put_u32(self.target_kbps);
        buf.put_u8(self.reason as u8);
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < 5 {
            return None;
        }
        let target_kbps = buf.get_u32();
        let reason = BitrateReason::from_byte(buf.get_u8())?;
        Some(BitrateCmd {
            target_kbps,
            reason,
        })
    }
}

/// PING packet for RTT measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingPacket {
    /// Sender's timestamp in µs when the ping was sent.
    pub origin_timestamp_us: u32,
    /// Ping sequence (for matching with pong).
    pub ping_id: u16,
}

impl PingPacket {
    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(ControlType::Ping as u8);
        buf.put_u32(self.origin_timestamp_us);
        buf.put_u16(self.ping_id);
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < 6 {
            return None;
        }
        Some(PingPacket {
            origin_timestamp_us: buf.get_u32(),
            ping_id: buf.get_u16(),
        })
    }
}

/// PONG response to a PING.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PongPacket {
    /// Echoed origin timestamp from the PING.
    pub origin_timestamp_us: u32,
    /// Echoed ping ID.
    pub ping_id: u16,
    /// Receiver's timestamp when the ping was received.
    pub receive_timestamp_us: u32,
}

impl PongPacket {
    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(ControlType::Pong as u8);
        buf.put_u32(self.origin_timestamp_us);
        buf.put_u16(self.ping_id);
        buf.put_u32(self.receive_timestamp_us);
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < 10 {
            return None;
        }
        Some(PongPacket {
            origin_timestamp_us: buf.get_u32(),
            ping_id: buf.get_u16(),
            receive_timestamp_us: buf.get_u32(),
        })
    }
}

/// Session handshake / teardown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPacket {
    pub action: SessionAction,
    /// Unique 64-bit session identifier.
    pub session_id: u64,
    /// Link-specific identifier for LINK_JOIN/LINK_LEAVE.
    pub link_id: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionAction {
    /// Client hello — initiate session.
    Hello = 0,
    /// Server accept — session established.
    Accept = 1,
    /// Graceful teardown.
    Teardown = 2,
    /// New link joining the session.
    LinkJoin = 3,
    /// Link leaving the session.
    LinkLeave = 4,
}

impl SessionAction {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(SessionAction::Hello),
            1 => Some(SessionAction::Accept),
            2 => Some(SessionAction::Teardown),
            3 => Some(SessionAction::LinkJoin),
            4 => Some(SessionAction::LinkLeave),
            _ => None,
        }
    }
}

impl SessionPacket {
    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(ControlType::Session as u8);
        buf.put_u8(self.action as u8);
        buf.put_u64(self.session_id);
        match self.link_id {
            Some(id) => {
                buf.put_u8(1); // has link_id
                buf.put_u8(id);
            }
            None => {
                buf.put_u8(0);
            }
        }
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < 10 {
            return None;
        }
        let action = SessionAction::from_byte(buf.get_u8())?;
        let session_id = buf.get_u64();
        let has_link = buf.get_u8();
        let link_id = if has_link == 1 {
            if !buf.has_remaining() {
                return None;
            }
            Some(buf.get_u8())
        } else {
            None
        };
        Some(SessionPacket {
            action,
            session_id,
            link_id,
        })
    }
}

/// Receiver report: aggregate receiver-side metrics sent back to the sender
/// so the sender's BitrateAdapter can incorporate ground-truth feedback.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiverReportPacket {
    /// Total recovered video goodput (bits/sec).
    pub goodput_bps: u64,
    /// Fraction of packets recovered by FEC (0.0–1.0), encoded as u16 (0–10000).
    pub fec_repair_rate: u16,
    /// Current jitter buffer depth in milliseconds.
    pub jitter_buffer_ms: u32,
    /// Residual loss after FEC recovery (0.0–1.0), encoded as u16 (0–10000).
    pub loss_after_fec: u16,
}

impl ReceiverReportPacket {
    pub const ENCODED_LEN: usize = 16; // 8 + 2 + 4 + 2

    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(ControlType::ReceiverReport as u8);
        buf.put_u64(self.goodput_bps);
        buf.put_u16(self.fec_repair_rate);
        buf.put_u32(self.jitter_buffer_ms);
        buf.put_u16(self.loss_after_fec);
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < Self::ENCODED_LEN {
            return None;
        }
        Some(ReceiverReportPacket {
            goodput_bps: buf.get_u64(),
            fec_repair_rate: buf.get_u16(),
            jitter_buffer_ms: buf.get_u32(),
            loss_after_fec: buf.get_u16(),
        })
    }

    /// FEC repair rate as a float (0.0–1.0).
    pub fn fec_repair_rate_f32(&self) -> f32 {
        self.fec_repair_rate as f32 / 10000.0
    }

    /// Residual loss after FEC as a float (0.0–1.0).
    pub fn loss_after_fec_f32(&self) -> f32 {
        self.loss_after_fec as f32 / 10000.0
    }
}

// ─── Full Packet Serialization ──────────────────────────────────────────────

/// A fully serialized Strata packet (header + payload).
#[derive(Debug, Clone)]
pub struct Packet {
    pub header: PacketHeader,
    pub payload: Bytes,
}

impl Packet {
    /// Serialize the entire packet (header + payload) into a new `BytesMut`.
    pub fn encode(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(self.header.encoded_len() + self.payload.len());
        self.header.encode(&mut buf);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode a complete packet from raw bytes.
    pub fn decode(data: &mut impl Buf) -> Option<Self> {
        let header = PacketHeader::decode(data)?;
        let payload_len = header.payload_len as usize;
        if data.remaining() < payload_len {
            return None;
        }
        let payload = data.copy_to_bytes(payload_len);
        Some(Packet { header, payload })
    }

    /// Create a new data packet.
    pub fn new_data(sequence: u64, timestamp_us: u32, payload: Bytes) -> Self {
        Packet {
            header: PacketHeader::data(sequence, timestamp_us, payload.len() as u16),
            payload,
        }
    }
}

// ─── Decoded Control Packet ─────────────────────────────────────────────────

/// A decoded control packet with its typed body.
#[derive(Debug, Clone)]
pub enum ControlBody {
    Ack(AckPacket),
    Nack(NackPacket),
    FecRepair(FecRepairHeader),
    LinkReport(LinkReport),
    BitrateCmd(BitrateCmd),
    Ping(PingPacket),
    Pong(PongPacket),
    Session(SessionPacket),
    ReceiverReport(ReceiverReportPacket),
}

impl ControlBody {
    /// Decode a control body from a buffer. The first byte is the subtype.
    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if !buf.has_remaining() {
            return None;
        }
        let subtype = buf.get_u8();
        let ct = ControlType::from_byte(subtype)?;
        match ct {
            ControlType::Ack => AckPacket::decode(buf).map(ControlBody::Ack),
            ControlType::Nack => NackPacket::decode(buf).map(ControlBody::Nack),
            ControlType::FecRepair => FecRepairHeader::decode(buf).map(ControlBody::FecRepair),
            ControlType::LinkReport => LinkReport::decode(buf).map(ControlBody::LinkReport),
            ControlType::BitrateCmd => BitrateCmd::decode(buf).map(ControlBody::BitrateCmd),
            ControlType::Ping => PingPacket::decode(buf).map(ControlBody::Ping),
            ControlType::Pong => PongPacket::decode(buf).map(ControlBody::Pong),
            ControlType::Session => SessionPacket::decode(buf).map(ControlBody::Session),
            ControlType::ReceiverReport => {
                ReceiverReportPacket::decode(buf).map(ControlBody::ReceiverReport)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ─── proptest: VarInt encode/decode roundtrip ─────────────────────────

    /// Strategy that generates values at VarInt encoding boundaries.
    fn varint_boundary_strategy() -> impl Strategy<Value = u64> {
        prop_oneof![
            // 1-byte range: 0..=0x3F
            0..=0x3Fu64,
            // 2-byte range: 0x40..=0x3FFF
            0x40u64..=0x3FFFu64,
            // 4-byte range: 0x4000..=0x3FFF_FFFF
            0x4000u64..=0x3FFF_FFFFu64,
            // 8-byte range: 0x4000_0000..=VarInt::MAX
            0x4000_0000u64..=VarInt::MAX,
        ]
    }

    proptest! {
        #[test]
        fn proptest_varint_roundtrip(val in varint_boundary_strategy()) {
            let vi = VarInt::from_u64(val);
            let mut buf = BytesMut::new();
            vi.encode(&mut buf);
            prop_assert_eq!(buf.len(), vi.encoded_len());
            let decoded = VarInt::decode(&mut buf.freeze()).unwrap();
            prop_assert_eq!(decoded.value(), val);
        }

        #[test]
        fn proptest_varint_out_of_range(val in (VarInt::MAX + 1)..=u64::MAX) {
            prop_assert!(VarInt::new(val).is_none());
        }

        #[test]
        fn proptest_varint_encoded_len_consistent(val in varint_boundary_strategy()) {
            let vi = VarInt::from_u64(val);
            let expected = if val < 0x40 { 1 }
                else if val < 0x4000 { 2 }
                else if val < 0x4000_0000 { 4 }
                else { 8 };
            prop_assert_eq!(vi.encoded_len(), expected);
        }
    }

    #[test]
    fn varint_roundtrip_boundaries() {
        let values = [
            0u64,
            1,
            0x3F,        // max 1-byte
            0x40,        // min 2-byte
            0x3FFF,      // max 2-byte
            0x4000,      // min 4-byte
            0x3FFF_FFFF, // max 4-byte
            0x4000_0000, // min 8-byte
            VarInt::MAX, // max 8-byte
        ];
        for &val in &values {
            let vi = VarInt::from_u64(val);
            let mut buf = BytesMut::new();
            vi.encode(&mut buf);
            assert_eq!(
                buf.len(),
                vi.encoded_len(),
                "encoded len mismatch for {val}"
            );
            let decoded = VarInt::decode(&mut buf.freeze()).unwrap();
            assert_eq!(decoded.value(), val, "roundtrip failed for {val}");
        }
    }

    #[test]
    fn varint_encoded_lengths() {
        assert_eq!(VarInt::from_u64(0).encoded_len(), 1);
        assert_eq!(VarInt::from_u64(63).encoded_len(), 1);
        assert_eq!(VarInt::from_u64(64).encoded_len(), 2);
        assert_eq!(VarInt::from_u64(16383).encoded_len(), 2);
        assert_eq!(VarInt::from_u64(16384).encoded_len(), 4);
        assert_eq!(VarInt::from_u64(0x3FFF_FFFF).encoded_len(), 4);
        assert_eq!(VarInt::from_u64(0x4000_0000).encoded_len(), 8);
    }

    #[test]
    fn varint_max_plus_one_fails() {
        assert!(VarInt::new(VarInt::MAX + 1).is_none());
    }

    #[test]
    fn header_roundtrip_data() {
        let hdr = PacketHeader::data(42, 1_000_000, 1400)
            .with_keyframe()
            .with_fragment(Fragment::Start);

        let mut buf = BytesMut::new();
        hdr.encode(&mut buf);
        let decoded = PacketHeader::decode(&mut buf).unwrap();
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert_eq!(decoded.packet_type, PacketType::Data);
        assert_eq!(decoded.fragment, Fragment::Start);
        assert!(decoded.is_keyframe);
        assert!(!decoded.is_config);
        assert_eq!(decoded.payload_len, 1400);
        assert_eq!(decoded.sequence.value(), 42);
        assert_eq!(decoded.timestamp_us, 1_000_000);
    }

    #[test]
    fn header_roundtrip_control() {
        let hdr = PacketHeader::control(999_999, 5_000_000, 64);
        let mut buf = BytesMut::new();
        hdr.encode(&mut buf);
        let decoded = PacketHeader::decode(&mut buf).unwrap();
        assert_eq!(decoded.packet_type, PacketType::Control);
        assert_eq!(decoded.sequence.value(), 999_999);
    }

    #[test]
    fn full_packet_roundtrip() {
        let payload = Bytes::from_static(b"hello strata");
        let pkt = Packet::new_data(100, 42_000, payload.clone());
        let encoded = pkt.encode();
        let decoded = Packet::decode(&mut encoded.freeze()).unwrap();
        assert_eq!(decoded.header.sequence.value(), 100);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn ack_roundtrip() {
        let ack = AckPacket {
            cumulative_seq: VarInt::from_u64(10000),
            sack_bitmap: 0b1010_0101,
        };
        let mut buf = BytesMut::new();
        ack.encode(&mut buf);
        let _ = buf.get_u8(); // skip subtype
        let decoded = AckPacket::decode(&mut buf).unwrap();
        assert_eq!(decoded.cumulative_seq.value(), 10000);
        assert_eq!(decoded.sack_bitmap, 0b1010_0101);
    }

    #[test]
    fn nack_roundtrip() {
        let nack = NackPacket {
            ranges: vec![
                NackRange {
                    start: VarInt::from_u64(100),
                    count: VarInt::from_u64(5),
                },
                NackRange {
                    start: VarInt::from_u64(200),
                    count: VarInt::from_u64(1),
                },
            ],
        };
        let mut buf = BytesMut::new();
        nack.encode(&mut buf);
        let _ = buf.get_u8(); // skip subtype
        let decoded = NackPacket::decode(&mut buf).unwrap();
        assert_eq!(decoded.ranges.len(), 2);
        assert_eq!(decoded.ranges[0].start.value(), 100);
        assert_eq!(decoded.ranges[0].count.value(), 5);
    }

    #[test]
    fn ping_pong_roundtrip() {
        let ping = PingPacket {
            origin_timestamp_us: 12345,
            ping_id: 7,
        };
        let mut buf = BytesMut::new();
        ping.encode(&mut buf);
        let _ = buf.get_u8();
        let decoded = PingPacket::decode(&mut buf).unwrap();
        assert_eq!(decoded.origin_timestamp_us, 12345);
        assert_eq!(decoded.ping_id, 7);

        let pong = PongPacket {
            origin_timestamp_us: 12345,
            ping_id: 7,
            receive_timestamp_us: 12400,
        };
        let mut buf = BytesMut::new();
        pong.encode(&mut buf);
        let _ = buf.get_u8();
        let decoded = PongPacket::decode(&mut buf).unwrap();
        assert_eq!(decoded.origin_timestamp_us, 12345);
        assert_eq!(decoded.receive_timestamp_us, 12400);
    }

    #[test]
    fn session_roundtrip() {
        let session = SessionPacket {
            action: SessionAction::LinkJoin,
            session_id: 0xDEAD_BEEF_CAFE_BABE,
            link_id: Some(3),
        };
        let mut buf = BytesMut::new();
        session.encode(&mut buf);
        let _ = buf.get_u8();
        let decoded = SessionPacket::decode(&mut buf).unwrap();
        assert_eq!(decoded.action, SessionAction::LinkJoin);
        assert_eq!(decoded.session_id, 0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(decoded.link_id, Some(3));
    }

    #[test]
    fn sack_iterator() {
        let ack = AckPacket {
            cumulative_seq: VarInt::from_u64(100),
            sack_bitmap: 0b0000_0101, // bits 0 and 2
        };
        let sacked: Vec<u64> = ack.sacked_sequences().collect();
        assert_eq!(sacked, vec![101, 103]);
    }

    #[test]
    fn receiver_report_roundtrip() {
        let report = ReceiverReportPacket {
            goodput_bps: 5_000_000,
            fec_repair_rate: 250, // 2.5%
            jitter_buffer_ms: 120,
            loss_after_fec: 50, // 0.5%
        };
        let mut buf = BytesMut::new();
        report.encode(&mut buf);
        assert_eq!(buf.len(), ReceiverReportPacket::ENCODED_LEN + 1); // +1 for type byte
        let _ = buf.get_u8(); // skip type byte
        let decoded = ReceiverReportPacket::decode(&mut buf).unwrap();
        assert_eq!(decoded.goodput_bps, 5_000_000);
        assert_eq!(decoded.fec_repair_rate, 250);
        assert_eq!(decoded.jitter_buffer_ms, 120);
        assert_eq!(decoded.loss_after_fec, 50);
    }

    #[test]
    fn receiver_report_float_conversions() {
        let report = ReceiverReportPacket {
            goodput_bps: 0,
            fec_repair_rate: 1000, // 10%
            jitter_buffer_ms: 0,
            loss_after_fec: 10000, // 100%
        };
        assert!((report.fec_repair_rate_f32() - 0.10).abs() < 1e-5);
        assert!((report.loss_after_fec_f32() - 1.0).abs() < 1e-5);
    }
}
