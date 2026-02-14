use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::cell::RefCell;

/// Thread-local pool of `BytesMut` buffers to avoid per-packet heap
/// allocation on the send hot path.  Each buffer is MTU-sized (1500 B)
/// by default; `wrap()` will grow it transparently when the payload is
/// larger.
const POOL_CAPACITY: usize = 256;
const DEFAULT_BUF_SIZE: usize = 1500;

thread_local! {
    static BUF_POOL: RefCell<Vec<BytesMut>> = RefCell::new(Vec::with_capacity(POOL_CAPACITY));
}

/// Take a `BytesMut` from the thread-local pool, or allocate a new one.
#[inline]
fn pool_take() -> BytesMut {
    BUF_POOL
        .with(|pool| pool.borrow_mut().pop())
        .unwrap_or_else(|| BytesMut::with_capacity(DEFAULT_BUF_SIZE))
}

/// Return a `BytesMut` to the thread-local pool for reuse.
#[inline]
pub fn pool_return(mut buf: BytesMut) {
    buf.clear();
    BUF_POOL.with(|pool| {
        let mut p = pool.borrow_mut();
        if p.len() < POOL_CAPACITY {
            p.push(buf);
        }
        // else: pool full, let the buffer drop normally
    });
}

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

    /// Wraps a payload with the bonding header, using a pooled buffer
    /// to avoid per-packet heap allocation.
    /// Returns a new Bytes object containing [Header + Payload]
    pub fn wrap(&self, payload: Bytes) -> Bytes {
        let needed = Self::SIZE + payload.len();
        let mut buf = pool_take();
        buf.reserve(needed);
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

    // ────────────────────────────────────────────────────────────────
    // BytesMut pool tests (#1)
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn pool_take_returns_buffer() {
        let buf = pool_take();
        assert!(buf.capacity() >= DEFAULT_BUF_SIZE);
    }

    #[test]
    fn pool_return_and_reuse() {
        // Return a buffer, then take — should get the same capacity back.
        let buf = BytesMut::with_capacity(2048);
        pool_return(buf);
        let reused = pool_take();
        // The reused buffer should have been cleared but retain capacity.
        assert!(reused.is_empty());
        assert!(reused.capacity() >= 2048);
    }

    #[test]
    fn pool_does_not_exceed_capacity() {
        // Fill pool to capacity and then try to return one more.
        for _ in 0..POOL_CAPACITY + 10 {
            pool_return(BytesMut::with_capacity(128));
        }
        // Pool should be capped.
        let count = BUF_POOL.with(|pool| pool.borrow().len());
        assert!(count <= POOL_CAPACITY);
    }

    #[test]
    fn pool_survives_concurrent_threads() {
        use std::thread;

        let handles: Vec<_> = (0..4)
            .map(|_| {
                thread::spawn(|| {
                    for _ in 0..100 {
                        let buf = pool_take();
                        pool_return(buf);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn wrap_uses_pooled_buffer() {
        // Warm up the pool
        for _ in 0..5 {
            pool_return(BytesMut::with_capacity(DEFAULT_BUF_SIZE));
        }
        let pool_size_before = BUF_POOL.with(|p| p.borrow().len());
        assert!(pool_size_before > 0);

        let header = BondingHeader::new(1);
        let _wrapped = header.wrap(Bytes::from_static(b"test"));

        let pool_size_after = BUF_POOL.with(|p| p.borrow().len());
        // Pool should have decreased by 1 (buffer taken for wrap)
        assert!(
            pool_size_after < pool_size_before,
            "Pool should shrink when wrap() takes a buffer: before={}, after={}",
            pool_size_before,
            pool_size_after
        );
    }

    #[test]
    fn wrap_large_payload_grows_buffer() {
        // Even with a default-sized pool buffer, large payloads work.
        let large = Bytes::from(vec![0xAAu8; 9000]); // jumbo frame
        let header = BondingHeader::new(42);
        let wrapped = header.wrap(large.clone());
        let (decoded, payload) = BondingHeader::unwrap(wrapped).unwrap();
        assert_eq!(decoded.seq_id, 42);
        assert_eq!(payload.len(), 9000);
    }
}
