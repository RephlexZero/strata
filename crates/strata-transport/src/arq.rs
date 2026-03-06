//! # ARQ — Automatic Repeat reQuest
//!
//! NACK-based loss detection and retransmission tracking.
//!
//! The receiver detects gaps in the sequence space and generates NACK control
//! packets. The sender responds with coded repair symbols (not raw retransmits)
//! when FEC is available, falling back to plain retransmission otherwise.
//!
//! ## Key design decisions
//!
//! - **Range-based NACKs**: efficient for burst losses
//! - **NACK deduplication**: coalesce adjacent gaps before sending
//! - **NACK suppression**: don't NACK packets past playout deadline
//! - **Retry budget**: max retransmission attempts per packet (default 3)

use quanta::Instant;
use std::collections::BTreeSet;
use std::time::Duration;

use crate::wire::{NackPacket, NackRange, VarInt};

// ─── Loss Detector (Receiver-Side) ──────────────────────────────────────────

/// Tracks received sequence numbers and detects gaps.
pub struct LossDetector {
    /// Highest contiguous sequence received (everything <= this is received).
    highest_contiguous: u64,
    /// Set of received sequences above highest_contiguous (out-of-order).
    received_above: BTreeSet<u64>,
    /// Sequences we've already NACKed, with timestamp (to avoid re-NACKing too fast).
    nacked: std::collections::HashMap<u64, NackState>,
    /// Minimum time between re-NACKing the same sequence.
    rearm_interval: Duration,
    /// Maximum sequence gap before we assume it's a reset (not a burst loss).
    max_gap: u64,
    /// Playout deadline offset: don't NACK packets older than this from now.
    playout_deadline: Duration,
    /// Whether the detector has been initialized (received first packet).
    initialized: bool,
    /// Maximum NACK retries per sequence.
    max_nacks_per_seq: u8,
    /// Total unique (non-duplicate) packets received. Incremented exactly
    /// once per packet in `record_received()`, providing a smooth counter
    /// for delivery-rate measurement that avoids the bursty jumps caused
    /// by cumulative-sequence advancement past irrecoverable gaps.
    total_received: u64,
}

#[derive(Debug, Clone)]
struct NackState {
    first_nacked_at: Instant,
    last_nacked_at: Instant,
    nack_count: u8,
    max_nacks: u8,
}

impl LossDetector {
    pub fn new() -> Self {
        LossDetector {
            highest_contiguous: 0,
            received_above: BTreeSet::new(),
            nacked: std::collections::HashMap::new(),
            rearm_interval: Duration::from_millis(50),
            max_gap: 10_000,
            playout_deadline: Duration::from_secs(2),
            initialized: false,
            max_nacks_per_seq: 3,
            total_received: 0,
        }
    }

    /// Set NACK suppression playout deadline.
    pub fn set_playout_deadline(&mut self, deadline: Duration) {
        self.playout_deadline = deadline;
    }

    /// Set rearm interval (minimum time between re-NACKs for the same seq).
    pub fn set_rearm_interval(&mut self, interval: Duration) {
        self.rearm_interval = interval;
    }

    /// Set the maximum number of NACKs per sequence before giving up.
    pub fn set_max_nacks(&mut self, max_nacks: u8) {
        self.max_nacks_per_seq = max_nacks;
    }

    /// Record a received sequence number. Call this for every received packet.
    pub fn record_received(&mut self, seq: u64) {
        if !self.initialized {
            self.highest_contiguous = seq;
            self.initialized = true;
            self.total_received += 1;
            return;
        }

        if seq <= self.highest_contiguous {
            // Duplicate or already accounted for
            return;
        }

        // Not a duplicate — count it
        if seq == self.highest_contiguous + 1 {
            // Next expected — advance contiguous pointer
            self.highest_contiguous = seq;
            self.total_received += 1;
            // Advance past any buffered out-of-order packets
            while self.received_above.contains(&(self.highest_contiguous + 1)) {
                self.received_above.remove(&(self.highest_contiguous + 1));
                self.highest_contiguous += 1;
            }
            // Remove NACK entries we no longer need
            self.nacked.retain(|&s, _| s > self.highest_contiguous);
        } else {
            // Out of order — gap detected
            if self.received_above.insert(seq) {
                self.total_received += 1;
            }
            // Remove from NACK tracking if we already NACKed it and it arrived
            self.nacked.remove(&seq);
        }
    }

    /// Detect missing sequences and generate NACK ranges.
    /// Call periodically (e.g., every 10-50ms).
    pub fn generate_nacks(&mut self) -> Option<NackPacket> {
        if !self.initialized {
            return None;
        }

        let now = Instant::now();
        let mut missing = Vec::new();

        // Find gaps between highest_contiguous and the highest out-of-order received
        let ceiling = match self.received_above.iter().next_back() {
            Some(&max_seq) => max_seq,
            None => return None, // No out-of-order packets, no gaps
        };

        // Don't NACK enormous gaps (likely a reset)
        if ceiling.saturating_sub(self.highest_contiguous) > self.max_gap {
            return None;
        }

        for seq in (self.highest_contiguous + 1)..ceiling {
            if self.received_above.contains(&seq) {
                continue; // Already received (out of order)
            }

            // Check NACK suppression
            if let Some(state) = self.nacked.get(&seq) {
                if state.nack_count >= state.max_nacks {
                    continue; // Exceeded retry budget
                }
                if now.duration_since(state.last_nacked_at) < self.rearm_interval {
                    continue; // Too soon to re-NACK
                }
            }

            missing.push(seq);
        }

        if missing.is_empty() {
            return None;
        }

        // Update NACK state
        for &seq in &missing {
            let state = self.nacked.entry(seq).or_insert_with(|| NackState {
                first_nacked_at: now,
                last_nacked_at: now,
                nack_count: 0,
                max_nacks: self.max_nacks_per_seq,
            });
            state.last_nacked_at = now;
            state.nack_count += 1;
        }

        // Coalesce into ranges
        let ranges = coalesce_ranges(&missing);
        Some(NackPacket { ranges })
    }

    /// Get the highest contiguous sequence number received.
    pub fn highest_contiguous(&self) -> u64 {
        self.highest_contiguous
    }

    /// Total unique (non-duplicate) packets received on this link.
    pub fn total_received(&self) -> u64 {
        self.total_received
    }

    /// Advance past irrecoverably lost packets.
    ///
    /// When a sequence has exhausted its NACK budget and will never be
    /// recovered, keeping `highest_contiguous` stuck behind it prevents
    /// the cumulative ACK from ever advancing.  This poisons delivery-rate
    /// measurement on the sender side because the 64-bit SACK bitmap
    /// can only report a tiny window beyond the stalled cumulative.
    ///
    /// Call this periodically (e.g. after `generate_nacks`).  It skips
    /// past sequences whose NACK budget is exhausted and that are present
    /// in `received_above`, catching up the contiguous frontier.
    pub fn advance_past_irrecoverable(&mut self) {
        let now = Instant::now();
        loop {
            let next = self.highest_contiguous + 1;
            if self.received_above.contains(&next) {
                // Already received — advance normally
                self.received_above.remove(&next);
                self.highest_contiguous = next;
            } else if let Some(state) = self.nacked.get(&next) {
                if state.nack_count >= state.max_nacks {
                    // Exhausted NACK budget — skip this packet
                    self.nacked.remove(&next);
                    self.highest_contiguous = next;
                } else if now.duration_since(state.first_nacked_at) >= self.playout_deadline {
                    // Packet has been missing longer than the playout
                    // deadline — even though NACKs remain, it's too late
                    // to be useful.  Skip to prevent frontier stall.
                    self.nacked.remove(&next);
                    self.highest_contiguous = next;
                } else {
                    break; // Still waiting for recovery
                }
            } else if !self.received_above.is_empty() {
                // Packet was never NACKed (gap appeared between NACK
                // cycles).  Seed it into the NACK tracker so the next
                // generate_nacks() / advance cycle can process it, and
                // immediately mark it with max_nacks-1 so it has one
                // remaining attempt before being declared irrecoverable.
                self.nacked.insert(
                    next,
                    NackState {
                        first_nacked_at: now,
                        last_nacked_at: now,
                        nack_count: self.max_nacks_per_seq.saturating_sub(1),
                        max_nacks: self.max_nacks_per_seq,
                    },
                );
                break; // Give it one more cycle to recover
            } else {
                break; // No evidence of gap
            }
        }
        // Clean up stale NACK state below the new contiguous point
        self.nacked.retain(|&s, _| s > self.highest_contiguous);
    }

    /// Number of out-of-order packets buffered above the contiguous point.
    pub fn pending_count(&self) -> usize {
        self.received_above.len()
    }

    /// Number of sequences currently being tracked for NACKing.
    pub fn nack_tracking_count(&self) -> usize {
        self.nacked.len()
    }

    /// Cleanup old NACK entries past the playout deadline.
    pub fn cleanup_stale(&mut self) {
        let now = Instant::now();
        self.nacked
            .retain(|_, state| now.duration_since(state.first_nacked_at) < self.playout_deadline);
    }
}

impl Default for LossDetector {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Retransmission Tracker (Sender-Side) ───────────────────────────────────

/// Tracks retransmission state on the sender side.
pub struct RetransmitTracker {
    /// Sequences pending retransmission.
    pending: BTreeSet<u64>,
    /// Per-sequence retry count.
    retry_counts: std::collections::HashMap<u64, u8>,
    /// Max retries before giving up.
    pub max_retries: u8,
}

impl RetransmitTracker {
    pub fn new(max_retries: u8) -> Self {
        RetransmitTracker {
            pending: BTreeSet::new(),
            retry_counts: std::collections::HashMap::new(),
            max_retries,
        }
    }

    /// Mark a sequence for retransmission (from NACK).
    /// Returns false if retry budget is exhausted.
    pub fn request_retransmit(&mut self, seq: u64) -> bool {
        let count = self.retry_counts.entry(seq).or_insert(0);
        if *count >= self.max_retries {
            return false;
        }
        *count += 1;
        self.pending.insert(seq);
        true
    }

    /// Drain all pending retransmit requests.
    pub fn drain_pending(&mut self) -> Vec<u64> {
        let pending: Vec<u64> = self.pending.iter().copied().collect();
        self.pending.clear();
        pending
    }

    /// Mark a sequence as successfully acknowledged (no more retransmits needed).
    pub fn mark_acked(&mut self, seq: u64) {
        self.pending.remove(&seq);
        self.retry_counts.remove(&seq);
    }

    /// Cleanup entries below a given sequence (cumulative ACK).
    pub fn cleanup_below(&mut self, seq: u64) {
        self.pending = self.pending.split_off(&seq);
        self.retry_counts.retain(|&s, _| s >= seq);
    }

    /// Number of sequences pending retransmission.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Default for RetransmitTracker {
    fn default() -> Self {
        Self::new(3)
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Coalesce a sorted list of sequence numbers into contiguous NACK ranges.
fn coalesce_ranges(seqs: &[u64]) -> Vec<NackRange> {
    if seqs.is_empty() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut start = seqs[0];
    let mut count = 1u64;

    for &seq in &seqs[1..] {
        if seq == start + count {
            count += 1;
        } else {
            ranges.push(NackRange {
                start: VarInt::from_u64(start),
                count: VarInt::from_u64(count),
            });
            start = seq;
            count = 1;
        }
    }

    ranges.push(NackRange {
        start: VarInt::from_u64(start),
        count: VarInt::from_u64(count),
    });

    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Coalesce Tests ─────────────────────────────────────────────────

    #[test]
    fn coalesce_empty() {
        let ranges = coalesce_ranges(&[]);
        assert!(ranges.is_empty());
    }

    #[test]
    fn coalesce_single() {
        let ranges = coalesce_ranges(&[5]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start.value(), 5);
        assert_eq!(ranges[0].count.value(), 1);
    }

    #[test]
    fn coalesce_contiguous_run() {
        let ranges = coalesce_ranges(&[10, 11, 12, 13]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start.value(), 10);
        assert_eq!(ranges[0].count.value(), 4);
    }

    #[test]
    fn coalesce_multiple_runs() {
        let ranges = coalesce_ranges(&[5, 6, 7, 20, 21, 100]);
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0].start.value(), 5);
        assert_eq!(ranges[0].count.value(), 3);
        assert_eq!(ranges[1].start.value(), 20);
        assert_eq!(ranges[1].count.value(), 2);
        assert_eq!(ranges[2].start.value(), 100);
        assert_eq!(ranges[2].count.value(), 1);
    }

    // ─── Loss Detector Tests ────────────────────────────────────────────

    #[test]
    fn detector_in_order_no_nack() {
        let mut det = LossDetector::new();
        for seq in 0..100 {
            det.record_received(seq);
        }
        assert_eq!(det.highest_contiguous(), 99);
        assert!(det.generate_nacks().is_none());
    }

    #[test]
    fn detector_single_gap() {
        let mut det = LossDetector::new();
        det.record_received(0);
        det.record_received(1);
        // Skip 2
        det.record_received(3);
        det.record_received(4);

        let nack = det.generate_nacks().unwrap();
        assert_eq!(nack.ranges.len(), 1);
        assert_eq!(nack.ranges[0].start.value(), 2);
        assert_eq!(nack.ranges[0].count.value(), 1);
    }

    #[test]
    fn detector_burst_gap() {
        let mut det = LossDetector::new();
        det.record_received(0);
        // Skip 1, 2, 3
        det.record_received(4);

        let nack = det.generate_nacks().unwrap();
        assert_eq!(nack.ranges.len(), 1);
        assert_eq!(nack.ranges[0].start.value(), 1);
        assert_eq!(nack.ranges[0].count.value(), 3);
    }

    #[test]
    fn detector_gap_fills_advance_contiguous() {
        let mut det = LossDetector::new();
        det.record_received(0);
        det.record_received(2);
        det.record_received(3);
        assert_eq!(det.highest_contiguous(), 0);

        // Fill the gap
        det.record_received(1);
        assert_eq!(
            det.highest_contiguous(),
            3,
            "should advance through buffered"
        );
    }

    #[test]
    fn detector_duplicate_ignored() {
        let mut det = LossDetector::new();
        det.record_received(0);
        det.record_received(1);
        det.record_received(1); // duplicate
        det.record_received(1); // duplicate
        assert_eq!(det.highest_contiguous(), 1);
    }

    #[test]
    fn detector_nack_rearm_suppression() {
        let mut det = LossDetector::new();
        det.set_rearm_interval(Duration::from_secs(10)); // very long rearm

        det.record_received(0);
        det.record_received(2); // gap at 1

        // First NACK
        let nack1 = det.generate_nacks();
        assert!(nack1.is_some());

        // Second NACK — should be suppressed (too soon)
        let nack2 = det.generate_nacks();
        assert!(
            nack2.is_none(),
            "should suppress re-NACK within rearm interval"
        );
    }

    #[test]
    fn detector_nack_retry_budget() {
        let mut det = LossDetector::new();
        det.set_rearm_interval(Duration::from_millis(0)); // instant rearm

        det.record_received(0);
        det.record_received(2); // gap at 1

        // NACK 3 times (default max_nacks=3)
        for _ in 0..3 {
            let nack = det.generate_nacks();
            assert!(nack.is_some());
        }

        // 4th NACK should be suppressed (budget exhausted)
        let nack = det.generate_nacks();
        assert!(nack.is_none(), "should exhaust NACK retry budget");
    }

    #[test]
    fn detector_large_gap_skipped() {
        let mut det = LossDetector::new();
        det.record_received(0);
        det.record_received(100_000); // huge gap — likely a reset

        let nack = det.generate_nacks();
        assert!(nack.is_none(), "should not NACK enormous gaps");
    }

    // ─── advance_past_irrecoverable regression tests ─────────────────────

    /// Regression: gap that appeared between two generate_nacks() calls was
    /// never NACKed, so the old code hit the `else { break }` branch and the
    /// frontier stalled permanently.
    ///
    /// Fix: the un-NACKed branch seeds the sequence into `nacked` at
    /// max_nacks-1 so the next advance cycle can declare it irrecoverable.
    #[test]
    fn irrecoverable_gap_un_nacked_advances_frontier() {
        let mut det = LossDetector::new();
        det.set_max_nacks(3);
        det.set_rearm_interval(Duration::from_millis(0));

        // Receive seq 0 then seq 2 (gap at 1), but never call generate_nacks()
        // before advance_past_irrecoverable — so seq 1 is un-NACKed.
        det.record_received(0);
        det.record_received(2); // received_above = {2}, gap at 1

        // highest_contiguous is still 0; seq 1 was never NACKed.
        assert_eq!(det.highest_contiguous(), 0);
        assert_eq!(det.nack_tracking_count(), 0);

        // First advance call: seq 1 is un-NACKed but received_above is
        // non-empty → seed into nacked at max_nacks-1 (one retry left).
        det.advance_past_irrecoverable();
        assert_eq!(
            det.nack_tracking_count(),
            1,
            "seq 1 should be seeded into nacked"
        );
        // Frontier should NOT have advanced yet (one retry remaining)
        assert_eq!(det.highest_contiguous(), 0);

        // generate_nacks() fires the last NACK attempt — now nack_count == max_nacks
        let nack = det.generate_nacks();
        assert!(nack.is_some(), "should produce one last NACK for seq 1");

        // Second advance call: nack_count >= max_nacks → skip and advance
        det.advance_past_irrecoverable();
        assert_eq!(
            det.highest_contiguous(),
            2,
            "frontier should advance past irrecoverable seq 1 and through buffered seq 2"
        );
        assert_eq!(
            det.pending_count(),
            0,
            "received_above should be empty after advancing through seq 2"
        );
    }

    /// When a packet has been NACKed and the playout deadline has elapsed,
    /// advance_past_irrecoverable() should skip it even though retries remain.
    #[test]
    fn advance_skips_nacked_packet_past_playout_deadline() {
        let mut det = LossDetector::new();
        det.set_max_nacks(10); // many retries remaining
        det.set_rearm_interval(Duration::from_millis(0));
        // Deadline of zero means any elapsed time qualifies
        det.set_playout_deadline(Duration::from_nanos(0));

        det.record_received(0);
        det.record_received(2); // gap at 1

        // NACK seq 1 (adds it to the nacked map with nack_count=1)
        let nack = det.generate_nacks();
        assert!(nack.is_some());
        assert_eq!(det.nack_tracking_count(), 1);

        // With deadline=0 the elapsed time is always >= deadline.
        // advance() should skip seq 1 (deadline expired) and advance through seq 2.
        det.advance_past_irrecoverable();

        assert_eq!(
            det.highest_contiguous(),
            2,
            "frontier must advance past deadline-expired seq 1 and through buffered seq 2"
        );
    }

    // ─── Retransmit Tracker Tests ───────────────────────────────────────

    #[test]
    fn retransmit_request_and_drain() {
        let mut rt = RetransmitTracker::new(3);
        assert!(rt.request_retransmit(10));
        assert!(rt.request_retransmit(11));
        assert_eq!(rt.pending_count(), 2);

        let seqs = rt.drain_pending();
        assert_eq!(seqs, vec![10, 11]);
        assert_eq!(rt.pending_count(), 0);
    }

    #[test]
    fn retransmit_retry_budget() {
        let mut rt = RetransmitTracker::new(2);
        assert!(rt.request_retransmit(5));
        assert!(rt.request_retransmit(5));
        assert!(
            !rt.request_retransmit(5),
            "should exhaust after max_retries"
        );
    }

    #[test]
    fn retransmit_ack_clears() {
        let mut rt = RetransmitTracker::new(3);
        rt.request_retransmit(10);
        rt.mark_acked(10);
        assert_eq!(rt.pending_count(), 0);
    }

    #[test]
    fn retransmit_cleanup_below() {
        let mut rt = RetransmitTracker::new(3);
        rt.request_retransmit(5);
        rt.request_retransmit(10);
        rt.request_retransmit(15);

        rt.cleanup_below(10);
        assert_eq!(rt.pending_count(), 2); // 10 and 15 remain
    }
}
