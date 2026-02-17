#![no_main]

use libfuzzer_sys::fuzz_target;
use strata_transport::wire::VarInt;

/// Fuzz VarInt decode/encode roundtrip.
///
/// If decode succeeds, the re-encoded form must decode to the same value.
fuzz_target!(|data: &[u8]| {
    if let Some(vi) = VarInt::decode(&mut &data[..]) {
        // Value must be within valid range
        assert!(vi.value() <= VarInt::MAX);

        // Roundtrip: encode then decode must produce same value
        let mut buf = bytes::BytesMut::new();
        vi.encode(&mut buf);
        let decoded = VarInt::decode(&mut &buf[..]).expect("re-decode must succeed");
        assert_eq!(decoded.value(), vi.value());

        // Encoded length must match prediction
        assert_eq!(buf.len(), vi.encoded_len());
    }
});
