//! Prefixed ID generation.
//!
//! All entity IDs use a `prefix_` followed by a UUIDv7 (time-ordered).
//! This makes IDs globally unique, sortable by creation time, and instantly
//! identifiable by type when reading logs or database rows.

use uuid::Uuid;

/// Generate a prefixed ID using UUIDv7.
fn prefixed_id(prefix: &str) -> String {
    let id = Uuid::now_v7();
    format!("{}_{}", prefix, id.as_simple())
}

/// Generate a user ID: `usr_<uuid7>`
pub fn user_id() -> String {
    prefixed_id("usr")
}

/// Generate a sender (device) ID: `snd_<uuid7>`
pub fn sender_id() -> String {
    prefixed_id("snd")
}

/// Generate a stream ID: `str_<uuid7>`
pub fn stream_id() -> String {
    prefixed_id("str")
}

/// Generate a destination ID: `dst_<uuid7>`
pub fn destination_id() -> String {
    prefixed_id("dst")
}

/// Generate a short, human-readable enrollment token: `XXXX-XXXX`.
///
/// Uses an unambiguous character set (no 0/O, 1/I/l confusion).
/// 32^8 ≈ 1.1 trillion combinations — more than sufficient for
/// single-use, rate-limited enrollment tokens.
pub fn enrollment_token() -> String {
    use rand::Rng;
    // Unambiguous charset: digits 2-9, letters A-Z minus I and O
    const CHARSET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut rng = rand::rng();
    let mut token = String::with_capacity(9);
    for i in 0..8 {
        if i == 4 {
            token.push('-');
        }
        let idx = rng.random_range(0..CHARSET.len());
        token.push(CHARSET[idx] as char);
    }
    token
}

/// Normalize an enrollment token for comparison: uppercase, strip dashes/spaces.
pub fn normalize_enrollment_token(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_correct_prefix() {
        assert!(user_id().starts_with("usr_"));
        assert!(sender_id().starts_with("snd_"));
        assert!(stream_id().starts_with("str_"));
        assert!(destination_id().starts_with("dst_"));
    }

    #[test]
    fn enrollment_token_format() {
        let token = enrollment_token();
        assert_eq!(token.len(), 9, "XXXX-XXXX = 9 chars");
        assert_eq!(&token[4..5], "-");
        // All chars should be from unambiguous set
        for c in token.chars() {
            if c == '-' {
                continue;
            }
            assert!(
                "23456789ABCDEFGHJKLMNPQRSTUVWXYZ".contains(c),
                "unexpected char: {c}"
            );
        }
    }

    #[test]
    fn enrollment_tokens_are_unique() {
        let a = enrollment_token();
        let b = enrollment_token();
        assert_ne!(a, b);
    }

    #[test]
    fn normalize_token() {
        assert_eq!(normalize_enrollment_token("abcd-1234"), "ABCD1234");
        assert_eq!(normalize_enrollment_token("ABCD 1234"), "ABCD1234");
        assert_eq!(normalize_enrollment_token("abcd1234"), "ABCD1234");
    }

    #[test]
    fn ids_are_unique() {
        let a = user_id();
        let b = user_id();
        assert_ne!(a, b);
    }

    #[test]
    fn ids_are_sortable_by_time() {
        let a = user_id();
        let b = user_id();
        // UUIDv7 are time-ordered, so b > a lexicographically
        // (same prefix, later timestamp)
        assert!(b > a, "Expected {b} > {a}");
    }
}
