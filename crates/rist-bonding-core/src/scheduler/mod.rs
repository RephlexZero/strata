//! Packet scheduling engine for bonded RIST links.
//!
//! The scheduler distributes outgoing packets across multiple network links
//! using a Deficit Weighted Round Robin (DWRR) algorithm. It supports:
//! - Capacity-proportional load balancing with quality-aware credit accrual
//! - Critical packet broadcast (e.g. keyframes sent to all links)
//! - Adaptive redundancy (duplicate important packets when spare capacity exists)
//! - Fast-failover (broadcast all traffic when link instability is detected)

pub mod bonding;
pub mod dwrr;
pub mod ewma;
pub mod fec;
pub mod sbd;

/// Describes the importance and characteristics of a packet for scheduling decisions.
///
/// The scheduler uses this profile to decide whether to broadcast (critical),
/// allow dropping (expendable), or apply adaptive redundancy.
#[derive(Debug, Clone, Copy, Default)]
pub struct PacketProfile {
    /// If true, this packet is critical (e.g. video Keyframe, Audio, or Headers)
    /// and should be delivered with maximum reliability (e.g. broadcast).
    pub is_critical: bool,
    /// If true, this packet can be seemingly dropped if congestion occurs
    /// (e.g. non-reference B-frames), to preserve latency for other packets.
    pub can_drop: bool,
    /// Size of the packet in bytes (used for size-aware redundancy decisions).
    pub size_bytes: usize,
    /// Per-packet deadline in milliseconds from now.  If the estimated
    /// one-way delay to the receiver exceeds this budget at send time,
    /// the packet is discarded at the sender ("fire-and-forget" discard
    /// primitive).  A value of `0` means **no deadline** — the packet
    /// is always sent.  Only packets with `can_drop == true` are
    /// eligible for deadline-based discard; critical packets are never
    /// discarded.
    pub deadline_ms: u64,
}

/// STORM dual-queue classification.
///
/// Packets are classified into one of two queues:
/// - **Reliable**: critical / non-droppable packets that require guaranteed
///   delivery.  These get higher scheduling weight, broadcast, and redundancy.
/// - **Unreliable**: droppable, latency-sensitive packets that prefer
///   deadline-aware best-effort delivery.  Limited retries, age-based discard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueClass {
    Reliable,
    Unreliable,
}

impl PacketProfile {
    /// Classify this packet into a STORM queue class.
    pub fn queue_class(&self) -> QueueClass {
        if self.is_critical || !self.can_drop {
            QueueClass::Reliable
        } else {
            QueueClass::Unreliable
        }
    }
}
