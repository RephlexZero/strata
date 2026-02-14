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

/// Generate an enrollment token: `enr_<uuid7>`
pub fn enrollment_token() -> String {
    prefixed_id("enr")
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
        assert!(enrollment_token().starts_with("enr_"));
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
