//! Packet scheduling engine for bonded RIST links.
//!
//! The scheduler distributes outgoing packets across multiple network links
//! using a Deficit Weighted Round Robin (DWRR) algorithm. It supports:
//! - Capacity-proportional load balancing with quality-aware credit accrual
//! - Critical packet broadcast (e.g. keyframes sent to all links)
//! - Adaptive redundancy (duplicate important packets when spare capacity exists)
//! - Fast-failover (broadcast all traffic when link instability is detected)

pub mod blest;
pub mod bonding;
pub mod dwrr;
pub mod ewma;
pub mod fec;
pub mod iods;
pub mod kalman;
pub mod sbd;
pub mod thompson;

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
}
