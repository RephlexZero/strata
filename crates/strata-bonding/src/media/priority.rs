//! # Priority Classification
//!
//! Maps NAL classification to scheduling priorities and treatment policies.
//! This bridges the media layer with the bonding scheduler.

use super::nal::{Codec, NalClass, NalInfo};
use crate::scheduler::PacketProfile;

/// Treatment policy for a given priority level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Treatment {
    /// Send on ALL links with maximum FEC. (SPS, PPS, VPS)
    Broadcast,
    /// Send on best 2 links with high FEC. (IDR, CRA, BLA)
    Redundant,
    /// Normal single-link scheduling. (P-slices, reference frames)
    Normal,
    /// Lowest priority, can be dropped under pressure. (B non-ref)
    Droppable,
}

/// FEC protection level recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecLevel {
    /// Maximum FEC overhead (e.g., 50%).
    Max,
    /// High FEC overhead (e.g., 25%).
    High,
    /// Standard FEC overhead (e.g., 10%).
    Standard,
    /// No FEC needed (droppable frames).
    None,
}

/// Priority classification result.
#[derive(Debug, Clone)]
pub struct PacketPriority {
    /// How the scheduler should treat this packet.
    pub treatment: Treatment,
    /// Recommended FEC protection level.
    pub fec_level: FecLevel,
    /// NAL classification that produced this priority.
    pub nal_class: NalClass,
}

/// Classify a NAL unit into scheduling priority.
pub fn classify(info: &NalInfo) -> PacketPriority {
    match info.class {
        NalClass::ParameterSet => PacketPriority {
            treatment: Treatment::Broadcast,
            fec_level: FecLevel::Max,
            nal_class: NalClass::ParameterSet,
        },
        NalClass::Keyframe => PacketPriority {
            treatment: Treatment::Redundant,
            fec_level: FecLevel::High,
            nal_class: NalClass::Keyframe,
        },
        NalClass::Reference => PacketPriority {
            treatment: Treatment::Normal,
            fec_level: FecLevel::Standard,
            nal_class: NalClass::Reference,
        },
        NalClass::NonReference => PacketPriority {
            treatment: Treatment::Droppable,
            fec_level: FecLevel::None,
            nal_class: NalClass::NonReference,
        },
        NalClass::Unknown => PacketPriority {
            treatment: Treatment::Normal,
            fec_level: FecLevel::Standard,
            nal_class: NalClass::Unknown,
        },
    }
}

/// Convert a NAL-based priority into the scheduler's PacketProfile.
pub fn to_packet_profile(priority: &PacketPriority, size_bytes: usize) -> PacketProfile {
    PacketProfile {
        is_critical: matches!(
            priority.treatment,
            Treatment::Broadcast | Treatment::Redundant
        ),
        can_drop: matches!(priority.treatment, Treatment::Droppable),
        size_bytes,
    }
}

/// Classify raw payload bytes directly.
///
/// Parses the NAL header and returns both the NAL info and the scheduling priority.
pub fn classify_payload(payload: &[u8], codec: Codec) -> Option<(NalInfo, PacketPriority)> {
    let info = super::nal::parse_nal(payload, codec)?;
    let priority = classify(&info);
    Some((info, priority))
}

/// Graceful degradation stage based on capacity pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DegradationStage {
    /// No degradation.
    Normal = 0,
    /// Drop non-reference B-frames.
    DropDisposable = 1,
    /// Reduce encoder bitrate.
    ReduceBitrate = 2,
    /// Drop standard frames, protect only Critical + Reference.
    ProtectKeyframes = 3,
    /// Emergency: keyframe-only mode.
    KeyframeOnly = 4,
}

impl DegradationStage {
    /// Whether a given treatment should be allowed through at this stage.
    pub fn allows(&self, treatment: Treatment) -> bool {
        match self {
            DegradationStage::Normal => true,
            DegradationStage::DropDisposable => treatment != Treatment::Droppable,
            DegradationStage::ReduceBitrate => treatment != Treatment::Droppable,
            DegradationStage::ProtectKeyframes => {
                matches!(treatment, Treatment::Broadcast | Treatment::Redundant)
            }
            DegradationStage::KeyframeOnly => treatment == Treatment::Broadcast,
        }
    }

    /// Determine degradation stage from ratio of available_capacity / required_bitrate.
    pub fn from_pressure(ratio: f64) -> Self {
        if ratio >= 1.0 {
            DegradationStage::Normal
        } else if ratio >= 0.8 {
            DegradationStage::DropDisposable
        } else if ratio >= 0.5 {
            DegradationStage::ReduceBitrate
        } else if ratio >= 0.25 {
            DegradationStage::ProtectKeyframes
        } else {
            DegradationStage::KeyframeOnly
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::nal::NalInfo;

    fn make_info(class: NalClass) -> NalInfo {
        NalInfo {
            nal_type: 0,
            codec: Codec::H264,
            class,
            is_rap: class == NalClass::Keyframe,
        }
    }

    // ─── Classification ─────────────────────────────────────────────────

    #[test]
    fn parameter_set_gets_broadcast() {
        let info = make_info(NalClass::ParameterSet);
        let priority = classify(&info);
        assert_eq!(priority.treatment, Treatment::Broadcast);
        assert_eq!(priority.fec_level, FecLevel::Max);
    }

    #[test]
    fn keyframe_gets_redundant() {
        let info = make_info(NalClass::Keyframe);
        let priority = classify(&info);
        assert_eq!(priority.treatment, Treatment::Redundant);
        assert_eq!(priority.fec_level, FecLevel::High);
    }

    #[test]
    fn reference_gets_normal() {
        let info = make_info(NalClass::Reference);
        let priority = classify(&info);
        assert_eq!(priority.treatment, Treatment::Normal);
        assert_eq!(priority.fec_level, FecLevel::Standard);
    }

    #[test]
    fn non_reference_is_droppable() {
        let info = make_info(NalClass::NonReference);
        let priority = classify(&info);
        assert_eq!(priority.treatment, Treatment::Droppable);
        assert_eq!(priority.fec_level, FecLevel::None);
    }

    // ─── PacketProfile Conversion ───────────────────────────────────────

    #[test]
    fn critical_packet_profile() {
        let info = make_info(NalClass::ParameterSet);
        let priority = classify(&info);
        let profile = to_packet_profile(&priority, 128);
        assert!(profile.is_critical);
        assert!(!profile.can_drop);
        assert_eq!(profile.size_bytes, 128);
    }

    #[test]
    fn droppable_packet_profile() {
        let info = make_info(NalClass::NonReference);
        let priority = classify(&info);
        let profile = to_packet_profile(&priority, 1400);
        assert!(!profile.is_critical);
        assert!(profile.can_drop);
    }

    // ─── Degradation Stages ─────────────────────────────────────────────

    #[test]
    fn normal_allows_everything() {
        let stage = DegradationStage::Normal;
        assert!(stage.allows(Treatment::Broadcast));
        assert!(stage.allows(Treatment::Redundant));
        assert!(stage.allows(Treatment::Normal));
        assert!(stage.allows(Treatment::Droppable));
    }

    #[test]
    fn drop_disposable_blocks_droppable() {
        let stage = DegradationStage::DropDisposable;
        assert!(stage.allows(Treatment::Broadcast));
        assert!(stage.allows(Treatment::Normal));
        assert!(!stage.allows(Treatment::Droppable));
    }

    #[test]
    fn protect_keyframes_blocks_standard() {
        let stage = DegradationStage::ProtectKeyframes;
        assert!(stage.allows(Treatment::Broadcast));
        assert!(stage.allows(Treatment::Redundant));
        assert!(!stage.allows(Treatment::Normal));
        assert!(!stage.allows(Treatment::Droppable));
    }

    #[test]
    fn keyframe_only_allows_broadcast_only() {
        let stage = DegradationStage::KeyframeOnly;
        assert!(stage.allows(Treatment::Broadcast));
        assert!(!stage.allows(Treatment::Redundant));
        assert!(!stage.allows(Treatment::Normal));
    }

    #[test]
    fn degradation_from_pressure() {
        assert_eq!(
            DegradationStage::from_pressure(1.2),
            DegradationStage::Normal
        );
        assert_eq!(
            DegradationStage::from_pressure(0.9),
            DegradationStage::DropDisposable
        );
        assert_eq!(
            DegradationStage::from_pressure(0.6),
            DegradationStage::ReduceBitrate
        );
        assert_eq!(
            DegradationStage::from_pressure(0.3),
            DegradationStage::ProtectKeyframes
        );
        assert_eq!(
            DegradationStage::from_pressure(0.1),
            DegradationStage::KeyframeOnly
        );
    }

    // ─── End-to-end classify_payload ────────────────────────────────────

    #[test]
    fn classify_h264_idr_payload() {
        let payload = [0x65]; // IDR
        let (info, priority) = classify_payload(&payload, Codec::H264).unwrap();
        assert_eq!(info.class, NalClass::Keyframe);
        assert_eq!(priority.treatment, Treatment::Redundant);
    }

    #[test]
    fn classify_h265_vps_payload() {
        let payload = [0x40, 0x01]; // VPS
        let (info, priority) = classify_payload(&payload, Codec::H265).unwrap();
        assert_eq!(info.class, NalClass::ParameterSet);
        assert_eq!(priority.treatment, Treatment::Broadcast);
    }
}
