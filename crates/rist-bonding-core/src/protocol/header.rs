use bytes::{Buf, BufMut, Bytes, BytesMut};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct BondingHeader {
    pub seq_id: u64,
}

impl BondingHeader {
    pub const SIZE: usize = 8; // u64 size

    pub fn new(seq_id: u64) -> Self {
        Self { seq_id }
    }

    /// Wraps a payload with the bonding header.
    /// Returns a new Bytes object containing [Header + Payload]
    pub fn wrap(&self, payload: Bytes) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::SIZE + payload.len());
        buf.put_u64(self.seq_id);
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
        // buf now points to the rest
        Some((Self { seq_id }, buf))
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
        let short_data = Bytes::from_static(b"\x00\x01"); // 2 bytes
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
        let buf = Bytes::from(vec![0u8; 8]);
        let result = BondingHeader::unwrap(buf);
        assert!(result.is_some());
        let (header, payload) = result.unwrap();
        assert_eq!(header.seq_id, 0);
        assert!(payload.is_empty());
    }

    #[test]
    fn test_unwrap_seven_bytes_fails() {
        let buf = Bytes::from(vec![0u8; 7]);
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
        assert_eq!(wrapped.len(), 8 + 65535);
        let (decoded, decoded_payload) = BondingHeader::unwrap(wrapped).unwrap();
        assert_eq!(decoded.seq_id, 42);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn test_header_size_constant() {
        assert_eq!(BondingHeader::SIZE, 8);
    }

    #[test]
    fn test_header_equality() {
        let h1 = BondingHeader::new(100);
        let h2 = BondingHeader::new(100);
        let h3 = BondingHeader::new(200);
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }
}
