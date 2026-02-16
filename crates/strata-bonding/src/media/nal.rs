//! # NAL Unit Parser
//!
//! Detects and classifies NAL (Network Abstraction Layer) units from
//! H.264 (AVC) and H.265 (HEVC) bitstreams.
//!
//! Focused on RTP-style framing where each packet typically starts with
//! a NAL unit header.

/// Codec type for classification context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// Classification result of a NAL unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalClass {
    /// Parameter sets (SPS, PPS, VPS) — must reach receiver.
    ParameterSet,
    /// Keyframe (IDR, CRA, BLA) — reference for subsequent frames.
    Keyframe,
    /// Reference frame (P-slice, TRAIL_R) — used by other frames.
    Reference,
    /// Non-reference frame (B-slice non-ref, TRAIL_N) — droppable.
    NonReference,
    /// Unknown or unrecognized NAL type.
    Unknown,
}

/// Parsed NAL header info.
#[derive(Debug, Clone)]
pub struct NalInfo {
    /// The raw NAL unit type number.
    pub nal_type: u8,
    /// Codec context.
    pub codec: Codec,
    /// Classification.
    pub class: NalClass,
    /// Whether this NAL represents a random access point.
    pub is_rap: bool,
}

/// Parse the first NAL unit header from a payload buffer.
///
/// For H.264, the NAL header is 1 byte: `forbidden(1) | nal_ref_idc(2) | nal_type(5)`
/// For H.265, the NAL header is 2 bytes: `forbidden(1) | nal_type(6) | layer_id(6) | tid(3)`
///
/// Returns `None` if the payload is too short.
pub fn parse_nal(payload: &[u8], codec: Codec) -> Option<NalInfo> {
    match codec {
        Codec::H264 => parse_h264_nal(payload),
        Codec::H265 => parse_h265_nal(payload),
    }
}

fn parse_h264_nal(payload: &[u8]) -> Option<NalInfo> {
    if payload.is_empty() {
        return None;
    }

    let header = payload[0];
    let nal_type = header & 0x1F;
    let nal_ref_idc = (header >> 5) & 0x03;

    let (class, is_rap) = match nal_type {
        7 => (NalClass::ParameterSet, false),  // SPS
        8 => (NalClass::ParameterSet, false),  // PPS
        13 => (NalClass::ParameterSet, false), // SPS Extension
        5 => (NalClass::Keyframe, true),       // IDR
        1 => {
            // Slice of non-IDR picture — check ref_idc
            if nal_ref_idc > 0 {
                (NalClass::Reference, false)
            } else {
                (NalClass::NonReference, false)
            }
        }
        2 => (NalClass::Reference, false), // Slice data partition A
        3 => (NalClass::Reference, false), // Slice data partition B
        4 => (NalClass::Reference, false), // Slice data partition C
        6 => (NalClass::NonReference, false), // SEI
        9 => (NalClass::NonReference, false), // AU delimiter
        _ => (NalClass::Unknown, false),
    };

    Some(NalInfo {
        nal_type,
        codec: Codec::H264,
        class,
        is_rap,
    })
}

fn parse_h265_nal(payload: &[u8]) -> Option<NalInfo> {
    if payload.len() < 2 {
        return None;
    }

    let nal_type = (payload[0] >> 1) & 0x3F;

    let (class, is_rap) = match nal_type {
        32 => (NalClass::ParameterSet, false),      // VPS
        33 => (NalClass::ParameterSet, false),      // SPS
        34 => (NalClass::ParameterSet, false),      // PPS
        19..=20 => (NalClass::Keyframe, true),      // IDR_W_RADL, IDR_N_LP
        21 => (NalClass::Keyframe, true),           // CRA
        16..=18 => (NalClass::Keyframe, true),      // BLA variants
        0 => (NalClass::NonReference, false),       // TRAIL_N
        1 => (NalClass::Reference, false),          // TRAIL_R
        2 => (NalClass::NonReference, false),       // TSA_N
        3 => (NalClass::Reference, false),          // TSA_R
        4 => (NalClass::NonReference, false),       // STSA_N
        5 => (NalClass::Reference, false),          // STSA_R
        6 => (NalClass::NonReference, false),       // RADL_N
        7 => (NalClass::Reference, false),          // RADL_R
        8 => (NalClass::NonReference, false),       // RASL_N
        9 => (NalClass::Reference, false),          // RASL_R
        35 => (NalClass::NonReference, false),      // AUD
        39 | 40 => (NalClass::NonReference, false), // SEI prefix/suffix
        _ => (NalClass::Unknown, false),
    };

    Some(NalInfo {
        nal_type,
        codec: Codec::H265,
        class,
        is_rap,
    })
}

/// Scan a byte buffer for Annex B start codes and return all NAL infos.
///
/// Start codes are `00 00 01` or `00 00 00 01`.
pub fn scan_annex_b(data: &[u8], codec: Codec) -> Vec<NalInfo> {
    let mut nals = Vec::new();
    let mut i = 0;

    while i + 2 < data.len() {
        // Look for start code
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            let (_sc_len, payload_start) = if i + 3 < data.len() && data[i + 2] == 0x01 {
                (3, i + 3)
            } else if i + 3 < data.len()
                && data[i + 2] == 0x00
                && i + 4 <= data.len()
                && data.get(i + 3) == Some(&0x01)
            {
                (4, i + 4)
            } else {
                i += 1;
                continue;
            };

            if payload_start < data.len() {
                if let Some(info) = parse_nal(&data[payload_start..], codec) {
                    nals.push(info);
                }
            }
            i = payload_start;
        } else {
            i += 1;
        }
    }

    nals
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── H.264 ──────────────────────────────────────────────────────────

    #[test]
    fn h264_sps() {
        let payload = [0x67]; // forbidden=0, ref_idc=3, type=7 (SPS)
        let info = parse_nal(&payload, Codec::H264).unwrap();
        assert_eq!(info.nal_type, 7);
        assert_eq!(info.class, NalClass::ParameterSet);
        assert!(!info.is_rap);
    }

    #[test]
    fn h264_pps() {
        let payload = [0x68]; // forbidden=0, ref_idc=3, type=8 (PPS)
        let info = parse_nal(&payload, Codec::H264).unwrap();
        assert_eq!(info.nal_type, 8);
        assert_eq!(info.class, NalClass::ParameterSet);
    }

    #[test]
    fn h264_idr() {
        let payload = [0x65]; // forbidden=0, ref_idc=3, type=5 (IDR)
        let info = parse_nal(&payload, Codec::H264).unwrap();
        assert_eq!(info.nal_type, 5);
        assert_eq!(info.class, NalClass::Keyframe);
        assert!(info.is_rap);
    }

    #[test]
    fn h264_p_slice_reference() {
        let payload = [0x41]; // forbidden=0, ref_idc=2, type=1 (coded slice)
        let info = parse_nal(&payload, Codec::H264).unwrap();
        assert_eq!(info.nal_type, 1);
        assert_eq!(info.class, NalClass::Reference);
    }

    #[test]
    fn h264_b_slice_non_reference() {
        let payload = [0x01]; // forbidden=0, ref_idc=0, type=1
        let info = parse_nal(&payload, Codec::H264).unwrap();
        assert_eq!(info.nal_type, 1);
        assert_eq!(info.class, NalClass::NonReference);
    }

    #[test]
    fn h264_empty_payload() {
        assert!(parse_nal(&[], Codec::H264).is_none());
    }

    // ─── H.265 ──────────────────────────────────────────────────────────

    #[test]
    fn h265_vps() {
        // NAL type 32 → (32 << 1) = 0x40, second byte = TID 1 → 0x01
        let payload = [0x40, 0x01];
        let info = parse_nal(&payload, Codec::H265).unwrap();
        assert_eq!(info.nal_type, 32);
        assert_eq!(info.class, NalClass::ParameterSet);
    }

    #[test]
    fn h265_sps() {
        // NAL type 33 → (33 << 1) = 0x42
        let payload = [0x42, 0x01];
        let info = parse_nal(&payload, Codec::H265).unwrap();
        assert_eq!(info.nal_type, 33);
        assert_eq!(info.class, NalClass::ParameterSet);
    }

    #[test]
    fn h265_idr_w_radl() {
        // NAL type 19 → (19 << 1) = 0x26
        let payload = [0x26, 0x01];
        let info = parse_nal(&payload, Codec::H265).unwrap();
        assert_eq!(info.nal_type, 19);
        assert_eq!(info.class, NalClass::Keyframe);
        assert!(info.is_rap);
    }

    #[test]
    fn h265_trail_n() {
        // NAL type 0 → (0 << 1) = 0x00
        let payload = [0x00, 0x01];
        let info = parse_nal(&payload, Codec::H265).unwrap();
        assert_eq!(info.nal_type, 0);
        assert_eq!(info.class, NalClass::NonReference);
    }

    #[test]
    fn h265_trail_r() {
        // NAL type 1 → (1 << 1) = 0x02
        let payload = [0x02, 0x01];
        let info = parse_nal(&payload, Codec::H265).unwrap();
        assert_eq!(info.nal_type, 1);
        assert_eq!(info.class, NalClass::Reference);
    }

    #[test]
    fn h265_too_short() {
        assert!(parse_nal(&[0x42], Codec::H265).is_none());
    }

    // ─── Annex B Scanning ───────────────────────────────────────────────

    #[test]
    fn annex_b_finds_multiple_nals() {
        // SPS + PPS + IDR with 3-byte start codes
        let data = [
            0x00, 0x00, 0x01, 0x67, 0xAA, // SPS
            0x00, 0x00, 0x01, 0x68, 0xBB, // PPS
            0x00, 0x00, 0x01, 0x65, 0xCC, 0xDD, // IDR
        ];
        let nals = scan_annex_b(&data, Codec::H264);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].class, NalClass::ParameterSet); // SPS
        assert_eq!(nals[1].class, NalClass::ParameterSet); // PPS
        assert_eq!(nals[2].class, NalClass::Keyframe); // IDR
    }

    #[test]
    fn annex_b_four_byte_start_code() {
        let data = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, // SPS with 4-byte start code
        ];
        let nals = scan_annex_b(&data, Codec::H264);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].class, NalClass::ParameterSet);
    }

    #[test]
    fn annex_b_empty() {
        let nals = scan_annex_b(&[], Codec::H264);
        assert!(nals.is_empty());
    }
}
