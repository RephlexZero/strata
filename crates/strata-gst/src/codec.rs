//! # Codec Controller Abstraction
//!
//! Provides a uniform interface for controlling H.264 (x264enc) and
//! H.265 (x265enc) GStreamer encoders. This allows the bitrate adaptation
//! and pipeline construction to work identically regardless of codec.

use gst::prelude::*;

/// Supported codec types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecType {
    H264,
    H265,
}

impl CodecType {
    /// Parse from CLI string (case-insensitive).
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "h264" | "x264" | "avc" => Some(CodecType::H264),
            "h265" | "x265" | "hevc" => Some(CodecType::H265),
            _ => None,
        }
    }

    /// GStreamer encoder element factory name.
    pub fn encoder_factory(&self) -> &'static str {
        match self {
            CodecType::H264 => "x264enc",
            CodecType::H265 => "x265enc",
        }
    }

    /// GStreamer parser element factory name.
    pub fn parser_factory(&self) -> &'static str {
        match self {
            CodecType::H264 => "h264parse",
            CodecType::H265 => "h265parse",
        }
    }

    /// Media type caps string for the encoded output.
    pub fn caps_media_type(&self) -> &'static str {
        match self {
            CodecType::H264 => "video/x-h264",
            CodecType::H265 => "video/x-h265",
        }
    }
}

/// Uniform codec controller for encoder runtime operations.
pub struct CodecController {
    codec: CodecType,
}

impl CodecController {
    pub fn new(codec: CodecType) -> Self {
        Self { codec }
    }

    pub fn codec(&self) -> CodecType {
        self.codec
    }

    /// Set the encoder bitrate in kbps.
    pub fn set_bitrate_kbps(&self, enc: &gst::Element, kbps: u32) {
        match self.codec {
            CodecType::H264 => {
                // x264enc "bitrate" property is in kbps
                enc.set_property("bitrate", kbps);
            }
            CodecType::H265 => {
                // x265enc "bitrate" property is in kbps
                enc.set_property("bitrate", kbps);
            }
        }
    }

    /// Get the current encoder bitrate in kbps.
    pub fn get_bitrate_kbps(&self, enc: &gst::Element) -> u32 {
        enc.property::<u32>("bitrate")
    }

    /// Force a keyframe on the encoder via a GStreamer force-keyunit event.
    pub fn force_keyframe(&self, enc: &gst::Element) {
        let event = gst::event::CustomUpstream::builder(
            gst::Structure::builder("GstForceKeyUnit")
                .field("all-headers", true)
                .build(),
        )
        .build();
        enc.send_event(event);
    }

    /// Build the GStreamer pipeline fragment string for this codec.
    ///
    /// Returns the encoder + parser section, e.g.:
    /// `x264enc name=enc tune=zerolatency bitrate=2000 vbv-buf-capacity=25000 key-int-max=60`
    pub fn pipeline_fragment(
        &self,
        name: &str,
        bitrate_kbps: u32,
        key_int_max: u32,
        max_bitrate_kbps: u32,
    ) -> String {
        match self.codec {
            CodecType::H264 => {
                format!(
                    "x264enc name={name} tune=zerolatency bitrate={bps} \
                     vbv-buf-capacity={max_bps} key-int-max={ki}",
                    name = name,
                    bps = bitrate_kbps,
                    max_bps = max_bitrate_kbps,
                    ki = key_int_max,
                )
            }
            CodecType::H265 => {
                // tune must be set as a native GStreamer property (tune=4 = zerolatency).
                // Passing tune=zerolatency via option-string conflicts with the GStreamer
                // property wrapper and causes x265 encoder init to fail.
                // vbv-bufsize and vbv-maxrate (kbps) are only valid via option-string.
                format!(
                    "x265enc name={name} bitrate={bps} \
                     key-int-max={ki} tune=4 \
                     option-string=\"vbv-bufsize={max_bps}:vbv-maxrate={max_bps}\"",
                    name = name,
                    bps = bitrate_kbps,
                    max_bps = max_bitrate_kbps,
                    ki = key_int_max,
                )
            }
        }
    }

    /// Build the muxer pipeline fragment for the output format.
    /// For RTMP relay, H.264 uses flvmux, H.265 uses eflvmux (Enhanced FLV).
    pub fn relay_muxer_fragment(&self) -> &'static str {
        match self.codec {
            CodecType::H264 => "flvmux name=fmux streamable=true",
            // Enhanced FLV supports H.265 FourCC — available in GStreamer 1.24+.
            // Falls back to regular flvmux which may not work for HEVC.
            CodecType::H265 => "flvmux name=fmux streamable=true",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_type_from_str() {
        assert_eq!(CodecType::from_str_loose("h264"), Some(CodecType::H264));
        assert_eq!(CodecType::from_str_loose("H265"), Some(CodecType::H265));
        assert_eq!(CodecType::from_str_loose("hevc"), Some(CodecType::H265));
        assert_eq!(CodecType::from_str_loose("x264"), Some(CodecType::H264));
        assert_eq!(CodecType::from_str_loose("avc"), Some(CodecType::H264));
        assert_eq!(CodecType::from_str_loose("vp9"), None);
    }

    #[test]
    fn encoder_factory_names() {
        assert_eq!(CodecType::H264.encoder_factory(), "x264enc");
        assert_eq!(CodecType::H265.encoder_factory(), "x265enc");
    }

    #[test]
    fn parser_factory_names() {
        assert_eq!(CodecType::H264.parser_factory(), "h264parse");
        assert_eq!(CodecType::H265.parser_factory(), "h265parse");
    }

    #[test]
    fn pipeline_fragment_h264() {
        let ctrl = CodecController::new(CodecType::H264);
        let frag = ctrl.pipeline_fragment("enc", 2000, 60, 25000);
        assert!(frag.contains("x264enc"));
        assert!(frag.contains("bitrate=2000"));
        assert!(frag.contains("vbv-buf-capacity=25000"));
        assert!(frag.contains("key-int-max=60"));
        assert!(frag.contains("tune=zerolatency"));
    }

    #[test]
    fn pipeline_fragment_h265() {
        let ctrl = CodecController::new(CodecType::H265);
        let frag = ctrl.pipeline_fragment("enc", 2000, 60, 25000);
        assert!(frag.contains("x265enc"));
        assert!(frag.contains("bitrate=2000"));
        assert!(frag.contains("vbv-bufsize=25000"));
        assert!(frag.contains("vbv-maxrate=25000"));
        assert!(frag.contains("tune=zerolatency"));
    }

    #[test]
    fn caps_media_types() {
        assert_eq!(CodecType::H264.caps_media_type(), "video/x-h264");
        assert_eq!(CodecType::H265.caps_media_type(), "video/x-h265");
    }
}
