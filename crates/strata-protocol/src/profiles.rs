//! Video profile presets — maps resolution + framerate + codec to bitrate envelope.
//!
//! Based on YouTube's recommended H.265 upload bitrates.
//! Used by the control plane to fill smart defaults when no explicit
//! bitrate values are provided.

use serde::{Deserialize, Serialize};

/// A bitrate envelope for a given resolution/framerate/codec combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoProfile {
    pub min_kbps: u32,
    pub default_kbps: u32,
    pub max_kbps: u32,
}

/// Compute smart-default bitrate envelope from resolution + framerate + codec.
///
/// Resolution is expected as "WIDTHxHEIGHT" (e.g. "1920x1080").
/// Codec defaults to "h265" when `None`.
/// Framerate defaults to 30 when `None`.
pub fn lookup_profile(
    resolution: Option<&str>,
    framerate: Option<u32>,
    codec: Option<&str>,
) -> VideoProfile {
    let res = resolution.unwrap_or("1920x1080");
    let fps = framerate.unwrap_or(30);
    let codec = codec.unwrap_or("h265");

    let height = res
        .split('x')
        .nth(1)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1080);

    let hfr = fps > 30;

    // H.265 bitrate table (kbps) — YouTube recommended ranges
    let (min, default, max) = match (height, hfr) {
        (0..=540, false) => (800, 1500, 3000),
        (0..=540, true) => (1000, 2000, 4000),
        (541..=720, false) => (1500, 3000, 4000),
        (541..=720, true) => (2000, 4000, 6000),
        (721..=1080, false) => (3000, 5000, 6000),
        (721..=1080, true) => (4000, 7000, 10000),
        (1081..=1440, false) => (6000, 10000, 13000),
        (1081..=1440, true) => (8000, 14000, 20000),
        (1441..=2160, false) => (10000, 20000, 30000),
        (1441..=2160, true) => (13000, 27000, 40000),
        _ => (13000, 27000, 40000), // 4K+ defaults to 4K
    };

    // H.264 uses ~1.5x the bitrate for equivalent quality
    let scale = if codec == "h264" { 1.5 } else { 1.0 };

    VideoProfile {
        min_kbps: (min as f64 * scale) as u32,
        default_kbps: (default as f64 * scale) as u32,
        max_kbps: (max as f64 * scale) as u32,
    }
}

/// Available test source patterns for the dashboard picker.
pub const TEST_PATTERNS: &[(&str, &str)] = &[
    ("smpte", "SMPTE Colour Bars"),
    ("ball", "Bouncing Ball"),
    ("snow", "Random Noise"),
    ("black", "Black Screen"),
];

/// Common resolutions for the dashboard picker.
pub const RESOLUTIONS: &[(&str, &str)] = &[
    ("1280x720", "720p"),
    ("1920x1080", "1080p"),
    ("2560x1440", "1440p"),
    ("3840x2160", "4K"),
];

/// Common framerates for the dashboard picker.
pub const FRAMERATES: &[u32] = &[24, 25, 30, 50, 60];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_720p30_h265() {
        let p = lookup_profile(Some("1280x720"), Some(30), Some("h265"));
        assert_eq!(p.min_kbps, 1500);
        assert_eq!(p.default_kbps, 3000);
        assert_eq!(p.max_kbps, 4000);
    }

    #[test]
    fn profile_1080p30_h265() {
        let p = lookup_profile(Some("1920x1080"), Some(30), Some("h265"));
        assert_eq!(p.min_kbps, 3000);
        assert_eq!(p.default_kbps, 5000);
        assert_eq!(p.max_kbps, 6000);
    }

    #[test]
    fn profile_1080p60_h265() {
        let p = lookup_profile(Some("1920x1080"), Some(60), Some("h265"));
        assert_eq!(p.min_kbps, 4000);
        assert_eq!(p.default_kbps, 7000);
        assert_eq!(p.max_kbps, 10000);
    }

    #[test]
    fn profile_4k30_h265() {
        let p = lookup_profile(Some("3840x2160"), Some(30), Some("h265"));
        assert_eq!(p.min_kbps, 10000);
        assert_eq!(p.default_kbps, 20000);
        assert_eq!(p.max_kbps, 30000);
    }

    #[test]
    fn profile_1080p30_h264_higher_than_h265() {
        let h265 = lookup_profile(Some("1920x1080"), Some(30), Some("h265"));
        let h264 = lookup_profile(Some("1920x1080"), Some(30), Some("h264"));
        assert!(h264.default_kbps > h265.default_kbps);
    }

    #[test]
    fn profile_defaults_to_1080p30_h265() {
        let p = lookup_profile(None, None, None);
        assert_eq!(p, lookup_profile(Some("1920x1080"), Some(30), Some("h265")));
    }

    #[test]
    fn profile_hfr_higher_than_sfr() {
        let sfr = lookup_profile(Some("1920x1080"), Some(30), Some("h265"));
        let hfr = lookup_profile(Some("1920x1080"), Some(60), Some("h265"));
        assert!(hfr.default_kbps > sfr.default_kbps);
        assert!(hfr.min_kbps > sfr.min_kbps);
        assert!(hfr.max_kbps > sfr.max_kbps);
    }
}
