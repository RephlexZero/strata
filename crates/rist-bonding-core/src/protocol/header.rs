use bytes::{Buf, BufMut, Bytes, BytesMut};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct BondingHeader {
    pub seq_id: u64,
    /// Sender-side timestamp in microseconds (monotonic).
    /// Used by the receiver to compute one-way delay (OWD).
    /// Set to 0 if OWD measurement is not available.
    pub send_time_us: u64,
}

impl BondingHeader {
    pub const SIZE: usize = 16; // u64 seq_id + u64 send_time_us

    pub fn new(seq_id: u64) -> Self {
        Self {
            seq_id,
            send_time_us: 0,
        }
    }

    /// Creates a new header with both sequence ID and send timestamp.
    pub fn with_timestamp(seq_id: u64, send_time_us: u64) -> Self {
        Self {
            seq_id,
            send_time_us,
        }
    }

    /// Wraps a payload with the bonding header.
    /// Returns a new Bytes object containing [Header + Payload]
    pub fn wrap(&self, payload: Bytes) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::SIZE + payload.len());
        buf.put_u64(self.seq_id);
        buf.put_u64(self.send_time_us);
        buf.put(payload);
        buf.freeze()
    }

    /// Parses the bonding header from a buffer.
    /// Returns (Header, Remaining Payload)
    pub fn unwrap(mut buf: Bytes) -> Option<(Self, Bytes)> {
        if buf.len() < Self::SIZE {
            return None;
        }
        let seq_id = buf.get_u64();
        let send_time_us = buf.get_u64();
        Some((
            Self {
                seq_id,
                send_time_us,
            },
            buf,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip() {
        let original_payload = Bytes::from_static(b"Hello World");
        let header = BondingHeader::new(123456789);

        let wrapped = header.wrap(original_payload.clone());
        assert_eq!(wrapped.len(), BondingHeader::SIZE + original_payload.len());

        let (decoded_header, decoded_payload) =
            BondingHeader::unwrap(wrapped).expect("Should unwrap successfully");

        assert_eq!(decoded_header, header);
        assert_eq!(decoded_payload, original_payload);
    }

    #[test]
    fn test_header_too_short() {
        let short_data = Bytes::from(vec![0u8; 8]); // only 8 bytes, need 16
        let result = BondingHeader::unwrap(short_data);
        assert!(result.is_none());
    }

    #[test]
    fn test_empty_payload() {
        let payload = Bytes::new();
        let header = BondingHeader::new(42);

        let wrapped = header.wrap(payload.clone());
        assert_eq!(wrapped.len(), BondingHeader::SIZE);

        let (decoded_header, decoded_payload) =
            BondingHeader::unwrap(wrapped).expect("Should unwrap");
        assert_eq!(decoded_header.seq_id, 42);
        assert!(decoded_payload.is_empty());
    }

    #[test]
    fn test_max_seq_id() {
        let header = BondingHeader::new(u64::MAX);
        let payload = Bytes::from_static(b"test");
        let wrapped = header.wrap(payload.clone());
        let (decoded, decoded_payload) = BondingHeader::unwrap(wrapped).unwrap();
        assert_eq!(decoded.seq_id, u64::MAX);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn test_zero_seq_id() {
        let header = BondingHeader::new(0);
        let payload = Bytes::from_static(b"test");
        let wrapped = header.wrap(payload.clone());
        let (decoded, decoded_payload) = BondingHeader::unwrap(wrapped).unwrap();
        assert_eq!(decoded.seq_id, 0);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn test_exact_header_size_buffer_no_payload() {
        let buf = Bytes::from(vec![0u8; 16]);
        let result = BondingHeader::unwrap(buf);
        assert!(result.is_some());
        let (header, payload) = result.unwrap();
        assert_eq!(header.seq_id, 0);
        assert_eq!(header.send_time_us, 0);
        assert!(payload.is_empty());
    }

    #[test]
    fn test_unwrap_fifteen_bytes_fails() {
        let buf = Bytes::from(vec![0u8; 15]);
        assert!(BondingHeader::unwrap(buf).is_none());
    }

    #[test]
    fn test_unwrap_zero_bytes_fails() {
        let buf = Bytes::new();
        assert!(BondingHeader::unwrap(buf).is_none());
    }

    #[test]
    fn test_large_payload() {
        let payload = Bytes::from(vec![0xABu8; 65535]);
        let header = BondingHeader::new(42);
        let wrapped = header.wrap(payload.clone());
        assert_eq!(wrapped.len(), 16 + 65535);
        let (decoded, decoded_payload) = BondingHeader::unwrap(wrapped).unwrap();
        assert_eq!(decoded.seq_id, 42);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn test_send_time_round_trip() {
        let payload = Bytes::from_static(b"test");
        let header = BondingHeader::with_timestamp(99, 1_000_000);
        let wrapped = header.wrap(payload.clone());
        let (decoded, decoded_payload) = BondingHeader::unwrap(wrapped).unwrap();
        assert_eq!(decoded.seq_id, 99);
        assert_eq!(decoded.send_time_us, 1_000_000);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn test_header_size_constant() {
        assert_eq!(BondingHeader::SIZE, 16);
    }

    #[test]
    fn test_header_equality() {
        let h1 = BondingHeader::new(100);
        let h2 = BondingHeader::new(100);
        let h3 = BondingHeader::new(200);
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }

    // ────────────────────────────────────────────────────────────────
    // Data integrity end-to-end through header wrap/unwrap
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn data_integrity_through_wrap_unwrap() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let num_packets = 200;
        let mut sender_hasher = DefaultHasher::new();
        let mut receiver_hasher = DefaultHasher::new();

        let mut wrapped = Vec::new();
        for i in 0..num_packets {
            let payload: Vec<u8> = (0..100).map(|j| ((i * 7 + j * 3) % 256) as u8).collect();
            payload.hash(&mut sender_hasher);
            let header = BondingHeader::with_timestamp(i as u64, i as u64 * 1000);
            wrapped.push(header.wrap(Bytes::from(payload)));
        }

        for w in wrapped {
            let (_hdr, body) = BondingHeader::unwrap(w).unwrap();
            body.to_vec().hash(&mut receiver_hasher);
        }

        assert_eq!(
            sender_hasher.finish(),
            receiver_hasher.finish(),
            "Sender and receiver hashes must match through header wrap/unwrap"
        );
    }
}
