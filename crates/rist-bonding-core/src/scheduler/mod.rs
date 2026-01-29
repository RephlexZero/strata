pub mod bonding;
pub mod dwrr;
pub mod ewma;
pub mod wrr;

#[derive(Debug, Clone, Copy, Default)]
pub struct PacketProfile {
    /// If true, this packet is critical (e.g. video Keyframe, Audio, or Headers)
    /// and should be delivered with maximum reliability (e.g. broadcast).
    pub is_critical: bool,
    /// If true, this packet can be seemingly dropped if congestion occurs
    /// (e.g. non-reference B-frames), to preserve latency for other packets.
    pub can_drop: bool,
}

pub fn init() {}
