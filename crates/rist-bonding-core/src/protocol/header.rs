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
        
        let (decoded_header, decoded_payload) = BondingHeader::unwrap(wrapped).expect("Should unwrap successfully");
        
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
        
        let (decoded_header, decoded_payload) = BondingHeader::unwrap(wrapped).expect("Should unwrap");
        assert_eq!(decoded_header.seq_id, 42);
        assert!(decoded_payload.is_empty());
    }
}
