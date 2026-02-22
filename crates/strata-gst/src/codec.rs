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
    Fake,
}

impl CodecType {
    /// Parse from CLI string (case-insensitive).
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "h264" | "x264" | "avc" => Some(CodecType::H264),
            "h265" | "x265" | "hevc" => Some(CodecType::H265),
            "fake" | "identity" => Some(CodecType::Fake),
            _ => None,
        }
    }

    /// GStreamer encoder element factory name.
    pub fn encoder_factory(&self) -> &'static str {
        match self {
            CodecType::H264 => "x264enc",
            CodecType::H265 => "x265enc",
            CodecType::Fake => "identity",
        }
    }

    /// GStreamer parser element factory name.
    pub fn parser_factory(&self) -> &'static str {
        match self {
            CodecType::H264 => "h264parse",
            CodecType::H265 => "h265parse",
            CodecType::Fake => "identity",
        }
    }

    /// Media type caps string for the encoded output.
    pub fn caps_media_type(&self) -> &'static str {
        match self {
            CodecType::H264 => "video/x-h264",
            CodecType::H265 => "video/x-h265",
            CodecType::Fake => "video/x-raw",
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
            CodecType::Fake => {
                // identity doesn't have a bitrate property
            }
        }
    }

    /// Get the current encoder bitrate in kbps.
    pub fn get_bitrate_kbps(&self, enc: &gst::Element) -> u32 {
        match self.codec {
            CodecType::Fake => 0,
            _ => enc.property::<u32>("bitrate"),
        }
    }

    /// Force a keyframe on the encoder via a GStreamer force-keyunit event.
    pub fn force_keyframe(&self, enc: &gst::Element) {
        if self.codec == CodecType::Fake {
            return;
        }
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
                    "x264enc name={name} tune=zerolatency speed-preset=ultrafast bitrate={bps} \
                     key-int-max={ki}",
                    name = name,
                    bps = bitrate_kbps,
                    ki = key_int_max,
                )
            }
            CodecType::H265 => {
                // tune must be set as a native GStreamer property (tune=4 = zerolatency).
                // Passing tune=zerolatency via option-string conflicts with the GStreamer
                // property wrapper and causes x265 encoder init to fail.
                // vbv-bufsize and vbv-maxrate (kbps) are only valid via option-string.
                // speed-preset=ultrafast is required so the encoder produces frames fast
                // enough to prevent hlssink2's splitmuxsink from starving on the video
                // stream while audio fills its internal queues.
                format!(
                    "x265enc name={name} bitrate={bps} \
                     key-int-max={ki} tune=4 speed-preset=ultrafast \
                     option-string=\"vbv-bufsize={max_bps}:vbv-maxrate={max_bps}\"",
                    name = name,
                    bps = bitrate_kbps,
                    max_bps = max_bitrate_kbps,
                    ki = key_int_max,
                )
            }
            CodecType::Fake => {
                format!("identity name={name} drop-probability=0.0")
            }
        }
    }

    /// Build the muxer pipeline fragment for the output format.
    /// For RTMP relay, H.264 uses flvmux, H.265 uses eflvmux (Enhanced FLV).
    pub fn relay_muxer_fragment(&self) -> &'static str {
        match self.codec {
            CodecType::H264 => "flvmux name=fmux streamable=true",
            // Enhanced FLV (eflvmux) supports H.265 FourCC — available in GStreamer 1.24+.
            CodecType::H265 => "eflvmux name=fmux streamable=true",
            CodecType::Fake => "identity name=fmux",
        }
    }

    /// Return just the factory name of the relay muxer element (for availability checks).
    pub fn relay_muxer_factory_name(&self) -> &'static str {
        match self.codec {
            CodecType::H264 => "flvmux",
            CodecType::H265 => "eflvmux",
            CodecType::Fake => "identity",
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
        assert!(frag.contains("key-int-max=60"));
        assert!(frag.contains("tune=zerolatency"));
        assert!(frag.contains("speed-preset=ultrafast"));
    }

    #[test]
    fn pipeline_fragment_h265() {
        let ctrl = CodecController::new(CodecType::H265);
        let frag = ctrl.pipeline_fragment("enc", 2000, 60, 25000);
        assert!(frag.contains("x265enc"));
        assert!(frag.contains("bitrate=2000"));
        assert!(frag.contains("vbv-bufsize=25000"));
        assert!(frag.contains("vbv-maxrate=25000"));
        // x265enc uses tune=4 (numeric) for zerolatency, not the string form
        assert!(frag.contains("tune=4"));
        assert!(frag.contains("speed-preset=ultrafast"));
    }

    #[test]
    fn caps_media_types() {
        assert_eq!(CodecType::H264.caps_media_type(), "video/x-h264");
        assert_eq!(CodecType::H265.caps_media_type(), "video/x-h265");
    }

    #[test]
    fn relay_muxer_fragment_codecs() {
        let h264 = CodecController::new(CodecType::H264);
        assert_eq!(
            h264.relay_muxer_fragment(),
            "flvmux name=fmux streamable=true"
        );

        let h265 = CodecController::new(CodecType::H265);
        assert_eq!(
            h265.relay_muxer_fragment(),
            "eflvmux name=fmux streamable=true"
        );
    }
}
