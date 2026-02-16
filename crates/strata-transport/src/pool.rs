//! # Packet Buffer Pool
//!
//! Pre-allocated slab-based packet pool for O(1) insert/remove with zero heap
//! churn on the hot path. Based on the master plan's buffer management design.
//!
//! At 10 Mbps with 1500-byte packets: ~833 packets/sec. A 4096-slot pool
//! (6 MB) provides ~5 seconds of buffer — trivial memory footprint.

use bytes::Bytes;
use slab::Slab;
use std::time::Instant;

use crate::wire::{Fragment, VarInt};

// ─── Priority ────────────────────────────────────────────────────────────────

/// Media-aware packet priority classification.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Priority {
    /// Disposable — B-frames, can be dropped under pressure.
    Disposable = 0,
    /// Standard — P-frames, normal scheduling.
    #[default]
    Standard = 1,
    /// Reference — IDR/I-frames, high FEC, best 2 links.
    Reference = 2,
    /// Critical — SPS/PPS/VPS, broadcast on ALL links, max FEC.
    Critical = 3,
}

// ─── PacketContext ───────────────────────────────────────────────────────────

/// Metadata associated with each packet in the pool.
#[derive(Debug, Clone)]
pub struct PacketContext {
    /// Global sequence number.
    pub sequence: u64,
    /// Microsecond timestamp.
    pub timestamp_us: u32,
    /// Media priority classification.
    pub priority: Priority,
    /// FEC generation this packet belongs to.
    pub fec_generation: u16,
    /// When this packet was enqueued/created.
    pub enqueue_time: Instant,
    /// Number of times this packet was retransmitted.
    pub retry_count: u8,
    /// Fragment status.
    pub fragment: Fragment,
    /// Whether this is a keyframe.
    pub is_keyframe: bool,
    /// Whether this is codec config.
    pub is_config: bool,
    /// Link ID this packet was sent on (set after scheduling).
    pub sent_on_link: Option<u8>,
    /// Whether this packet has been acknowledged.
    pub acked: bool,
}

impl PacketContext {
    /// Create a new context for a data packet.
    pub fn new(sequence: u64, timestamp_us: u32) -> Self {
        PacketContext {
            sequence,
            timestamp_us,
            priority: Priority::Standard,
            fec_generation: 0,
            enqueue_time: Instant::now(),
            retry_count: 0,
            fragment: Fragment::Complete,
            is_keyframe: false,
            is_config: false,
            sent_on_link: None,
            acked: false,
        }
    }

    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_fec_generation(mut self, gen: u16) -> Self {
        self.fec_generation = gen;
        self
    }
}

// ─── PacketEntry ─────────────────────────────────────────────────────────────

/// A single entry in the packet pool: payload + metadata.
#[derive(Debug, Clone)]
pub struct PacketEntry {
    pub context: PacketContext,
    pub payload: Bytes,
}

// ─── PacketPool ──────────────────────────────────────────────────────────────

/// Slab-based pre-allocated packet pool.
///
/// Provides O(1) insert, O(1) remove by key, zero heap churn after initial
/// allocation (assuming the slab doesn't need to grow beyond `capacity`).
pub struct PacketPool {
    entries: Slab<PacketEntry>,
    capacity: usize,
}

/// Handle to a packet in the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PacketHandle(pub usize);

impl PacketPool {
    /// Create a pool with the given capacity. The slab pre-allocates.
    pub fn new(capacity: usize) -> Self {
        PacketPool {
            entries: Slab::with_capacity(capacity),
            capacity,
        }
    }

    /// Insert a packet into the pool. Returns a handle for later retrieval.
    /// Returns `None` if the pool is full.
    pub fn insert(&mut self, context: PacketContext, payload: Bytes) -> Option<PacketHandle> {
        if self.entries.len() >= self.capacity {
            return None;
        }
        let key = self.entries.insert(PacketEntry { context, payload });
        Some(PacketHandle(key))
    }

    /// Get an immutable reference to a packet by handle.
    pub fn get(&self, handle: PacketHandle) -> Option<&PacketEntry> {
        self.entries.get(handle.0)
    }

    /// Get a mutable reference to a packet by handle.
    pub fn get_mut(&mut self, handle: PacketHandle) -> Option<&mut PacketEntry> {
        self.entries.get_mut(handle.0)
    }

    /// Remove a packet from the pool, returning it.
    pub fn remove(&mut self, handle: PacketHandle) -> Option<PacketEntry> {
        if self.entries.contains(handle.0) {
            Some(self.entries.remove(handle.0))
        } else {
            None
        }
    }

    /// Check if a handle is still valid.
    pub fn contains(&self, handle: PacketHandle) -> bool {
        self.entries.contains(handle.0)
    }

    /// Number of packets currently in the pool.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the pool is at capacity.
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// Pool capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Iterate all entries.
    pub fn iter(&self) -> impl Iterator<Item = (PacketHandle, &PacketEntry)> {
        self.entries.iter().map(|(k, v)| (PacketHandle(k), v))
    }

    /// Drain packets older than the given cutoff time.
    pub fn drain_expired(&mut self, cutoff: Instant) -> Vec<PacketEntry> {
        let expired_keys: Vec<usize> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.context.enqueue_time < cutoff)
            .map(|(key, _)| key)
            .collect();

        expired_keys
            .into_iter()
            .map(|key| self.entries.remove(key))
            .collect()
    }

    /// Mark a packet as acknowledged.
    pub fn mark_acked(&mut self, handle: PacketHandle) -> bool {
        if let Some(entry) = self.entries.get_mut(handle.0) {
            entry.context.acked = true;
            true
        } else {
            false
        }
    }

    /// Remove all acknowledged packets and return count purged.
    pub fn purge_acked(&mut self) -> usize {
        let acked_keys: Vec<usize> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.context.acked)
            .map(|(key, _)| key)
            .collect();
        let count = acked_keys.len();
        for key in acked_keys {
            self.entries.remove(key);
        }
        count
    }
}

// ─── Sequence Generator ─────────────────────────────────────────────────────

/// Thread-safe monotonic sequence number generator.
pub struct SequenceGenerator {
    next: u64,
}

impl SequenceGenerator {
    pub fn new() -> Self {
        SequenceGenerator { next: 0 }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        let seq = self.next;
        self.next = self.next.wrapping_add(1);
        // Clamp to VarInt max
        if self.next > VarInt::MAX {
            self.next = 0;
        }
        seq
    }

    pub fn current(&self) -> u64 {
        self.next
    }
}

impl Default for SequenceGenerator {
    fn default() -> Self {
        Self::new()
    }
}

// ─── TimestampClock ─────────────────────────────────────────────────────────

/// Microsecond wall clock for packet timestamps.
/// Wraps every ~71 minutes (u32::MAX µs).
pub struct TimestampClock {
    epoch: Instant,
}

impl TimestampClock {
    pub fn new() -> Self {
        TimestampClock {
            epoch: Instant::now(),
        }
    }

    /// Get current timestamp in µs since epoch.
    pub fn now_us(&self) -> u32 {
        let elapsed = self.epoch.elapsed();
        // Wrapping is intentional — matches the 32-bit timestamp field.
        (elapsed.as_micros() as u64 & 0xFFFF_FFFF) as u32
    }
}

impl Default for TimestampClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_insert_remove() {
        let mut pool = PacketPool::new(4);
        let ctx = PacketContext::new(1, 1000);
        let handle = pool.insert(ctx, Bytes::from_static(b"data")).unwrap();
        assert_eq!(pool.len(), 1);

        let entry = pool.get(handle).unwrap();
        assert_eq!(entry.context.sequence, 1);
        assert_eq!(entry.payload, &b"data"[..]);

        let removed = pool.remove(handle).unwrap();
        assert_eq!(removed.context.sequence, 1);
        assert!(pool.is_empty());
    }

    #[test]
    fn pool_capacity_limit() {
        let mut pool = PacketPool::new(2);
        let h1 = pool.insert(PacketContext::new(1, 0), Bytes::new());
        let h2 = pool.insert(PacketContext::new(2, 0), Bytes::new());
        let h3 = pool.insert(PacketContext::new(3, 0), Bytes::new());

        assert!(h1.is_some());
        assert!(h2.is_some());
        assert!(h3.is_none()); // full
        assert!(pool.is_full());
    }

    #[test]
    fn pool_ack_purge() {
        let mut pool = PacketPool::new(4);
        let h1 = pool.insert(PacketContext::new(1, 0), Bytes::new()).unwrap();
        let h2 = pool.insert(PacketContext::new(2, 0), Bytes::new()).unwrap();
        let _h3 = pool.insert(PacketContext::new(3, 0), Bytes::new()).unwrap();

        pool.mark_acked(h1);
        pool.mark_acked(h2);
        let purged = pool.purge_acked();
        assert_eq!(purged, 2);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn sequence_generator() {
        let mut gen = SequenceGenerator::new();
        assert_eq!(gen.next(), 0);
        assert_eq!(gen.next(), 1);
        assert_eq!(gen.next(), 2);
        assert_eq!(gen.current(), 3);
    }

    #[test]
    fn timestamp_clock_monotonic() {
        let clock = TimestampClock::new();
        let t1 = clock.now_us();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let t2 = clock.now_us();
        assert!(t2 >= t1);
    }

    #[test]
    fn priority_ordering() {
        assert!(Priority::Critical > Priority::Reference);
        assert!(Priority::Reference > Priority::Standard);
        assert!(Priority::Standard > Priority::Disposable);
    }
}
