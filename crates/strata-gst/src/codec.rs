//! # Codec Controller Abstraction
//!
//! Provides a uniform interface for controlling H.264 and H.265 GStreamer
//! encoders — both software (x264enc/x265enc) and hardware (NVENC, VA-API,
//! QSV). This allows the bitrate adaptation and pipeline construction to
//! work identically regardless of codec or encoder backend.

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

    /// GStreamer software encoder element factory name (fallback).
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

    /// GStreamer parser pipeline fragment for relay (RTMP/HLS) pipelines.
    ///
    /// `config-interval=-1` re-injects SPS/PPS/VPS headers before every
    /// keyframe so decoders and muxers that join mid-stream can initialise.
    /// `disable-passthrough=true` forces the parser to actively process every
    /// buffer even when upstream (e.g. tsdemux) marks data as passthrough,
    /// which would otherwise bypass the header injection.
    pub fn relay_parser_fragment(&self) -> &'static str {
        match self {
            CodecType::H264 => "h264parse config-interval=-1 disable-passthrough=true",
            // NOTE: disable-passthrough=true causes h265parse in GStreamer ≤1.24 to fail
            // keyframe detection for IDR frames after the first GOP boundary, stalling
            // splitmuxsink at segment 1.  Without it, h265parse honours the keyframe flags
            // set by tsdemux (from the TS random-access-indicator bit) and segments cut
            // normally.  config-interval=-1 is kept so each HLS segment starts with
            // VPS/SPS/PPS (required for independent seek/playback of each segment).
            CodecType::H265 => "h265parse config-interval=-1",
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

/// Detected encoder backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    /// Software encoder (x264enc / x265enc).
    Software,
    /// NVIDIA NVENC hardware encoder.
    Nvenc,
    /// VA-API hardware encoder (Intel/AMD — both old `gstreamer-vaapi` and new `va` plugin).
    Vaapi,
    /// Intel Quick Sync Video.
    Qsv,
    /// Vulkan Video hardware encoder (AMD RADV / any Vulkan 1.3 GPU).
    Vulkan,
    /// SVT-HEVC: fast software H.265 encoder (multi-threaded, much faster than x265enc).
    SvtHevc,
    /// Rockchip MPP hardware encoder (RK3588 / RK35xx SoCs — e.g. Orange Pi 5).
    /// Covers both the mainline `rkmpph26xenc` (gst-plugins-bad ≥1.24) and the
    /// Rockchip BSP `mpph26xenc` (gstreamer-rockchip).
    Rockchip,
}

impl std::fmt::Display for EncoderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncoderBackend::Software => write!(f, "software"),
            EncoderBackend::Nvenc => write!(f, "NVENC"),
            EncoderBackend::Vaapi => write!(f, "VA-API"),
            EncoderBackend::Qsv => write!(f, "QSV"),
            EncoderBackend::Vulkan => write!(f, "Vulkan"),
            EncoderBackend::SvtHevc => write!(f, "SVT-HEVC"),
            EncoderBackend::Rockchip => write!(f, "Rockchip-MPP"),
        }
    }
}

/// Probe GStreamer for the best available encoder for the given codec.
///
/// Priority: NVENC → VA-API (new `va` plugin) → VA-API (old `gstreamer-vaapi` plugin)
///           → QSV → Vulkan (H.264 only) → SVT-HEVC (H.265 fast-software)
///           → x264enc / x265enc.
/// Returns `(factory_name, backend)`.
fn resolve_encoder(codec: CodecType) -> (&'static str, EncoderBackend) {
    let candidates: &[(&str, EncoderBackend)] = match codec {
        CodecType::H264 => &[
            ("nvh264enc", EncoderBackend::Nvenc),
            ("vah264enc", EncoderBackend::Vaapi), // new va plugin (gst-plugins-bad 1.20+)
            ("vaapih264enc", EncoderBackend::Vaapi), // old gstreamer-vaapi plugin
            ("qsvh264enc", EncoderBackend::Qsv),
            ("vulkanh264enc", EncoderBackend::Vulkan), // AMD RADV / any Vulkan 1.3 GPU
            ("rkmpph264enc", EncoderBackend::Rockchip), // RK3588 mainline rkmpp
            ("mpph264enc", EncoderBackend::Rockchip),  // RK3588 Rockchip BSP
            ("x264enc", EncoderBackend::Software),
        ],
        CodecType::H265 => &[
            ("nvh265enc", EncoderBackend::Nvenc),
            ("vah265enc", EncoderBackend::Vaapi), // new va plugin
            ("vaapih265enc", EncoderBackend::Vaapi), // old gstreamer-vaapi plugin
            ("qsvh265enc", EncoderBackend::Qsv),
            ("rkmpph265enc", EncoderBackend::Rockchip), // RK3588 mainline rkmpp
            ("mpph265enc", EncoderBackend::Rockchip),   // RK3588 Rockchip BSP
            ("svthevcenc", EncoderBackend::SvtHevc), // fast multi-threaded SW (much faster than x265)
            ("x265enc", EncoderBackend::Software),
        ],
        CodecType::Fake => return ("identity", EncoderBackend::Software),
    };
    for &(factory, backend) in candidates {
        if gst::ElementFactory::find(factory).is_some() {
            return (factory, backend);
        }
    }
    // Last-resort fallback (may fail at pipeline creation if not installed).
    (codec.encoder_factory(), EncoderBackend::Software)
}

/// Set an integer rate property (`bitrate`/`bps`) using the property's actual
/// value type, so we never panic setting a u32 into an i32 property (or vice
/// versa) on an unfamiliar HW encoder.
fn set_rate_prop(enc: &gst::Element, name: &str, spec: &gst::glib::ParamSpec, value: u32) {
    match spec.value_type() {
        gst::glib::Type::U32 => enc.set_property(name, value),
        gst::glib::Type::I32 => enc.set_property(name, value as i32),
        gst::glib::Type::U64 => enc.set_property(name, value as u64),
        gst::glib::Type::I64 => enc.set_property(name, value as i64),
        _ => {}
    }
}

/// Read an integer rate property (`bitrate`/`bps`) regardless of its int type.
fn get_rate_prop(enc: &gst::Element, name: &str, spec: &gst::glib::ParamSpec) -> u32 {
    match spec.value_type() {
        gst::glib::Type::U32 => enc.property::<u32>(name),
        gst::glib::Type::I32 => enc.property::<i32>(name).max(0) as u32,
        gst::glib::Type::U64 => enc.property::<u64>(name).min(u32::MAX as u64) as u32,
        gst::glib::Type::I64 => enc.property::<i64>(name).clamp(0, u32::MAX as i64) as u32,
        _ => 0,
    }
}

/// Uniform codec controller for encoder runtime operations.
pub struct CodecController {
    codec: CodecType,
    encoder_factory: &'static str,
    backend: EncoderBackend,
}

impl CodecController {
    /// Create with software encoder (no hardware probing).
    /// Suitable for tests or when GStreamer may not be initialised.
    pub fn new(codec: CodecType) -> Self {
        Self {
            codec,
            encoder_factory: codec.encoder_factory(),
            backend: EncoderBackend::Software,
        }
    }

    /// Create with automatic hardware encoder detection.
    ///
    /// Probes GStreamer for available hardware encoders (NVENC, VA-API, QSV)
    /// and selects the highest-priority one. Falls back to software if none
    /// are found.
    pub fn with_auto_backend(codec: CodecType) -> Self {
        let (factory, backend) = resolve_encoder(codec);
        Self {
            codec,
            encoder_factory: factory,
            backend,
        }
    }

    pub fn codec(&self) -> CodecType {
        self.codec
    }

    pub fn backend(&self) -> EncoderBackend {
        self.backend
    }

    /// The GStreamer element factory name of the selected encoder.
    pub fn encoder_factory_name(&self) -> &'static str {
        self.encoder_factory
    }

    /// Set the encoder bitrate in kbps.
    ///
    /// Most backends expose `bitrate` in kbps. The Rockchip BSP `mpph26xenc`
    /// instead exposes `bps` (bits/sec). Pick whichever the element actually
    /// has, matching the property's value type so we never panic on a
    /// type/property mismatch on an unfamiliar HW encoder.
    pub fn set_bitrate_kbps(&self, enc: &gst::Element, kbps: u32) {
        if self.codec == CodecType::Fake {
            return;
        }
        // Rockchip BSP/BELABOX `mpph26xenc` names its property `bitrate` but
        // interprets it as BPS (bits/sec); everything else (incl. mainline
        // `rkmpph26xenc`) uses kbps.
        let rockchip_bps =
            self.backend == EncoderBackend::Rockchip && self.encoder_factory.starts_with("mpp");
        if let Some(spec) = enc.find_property("bitrate") {
            let v = if rockchip_bps {
                kbps.saturating_mul(1000)
            } else {
                kbps
            };
            set_rate_prop(enc, "bitrate", &spec, v);
        } else if let Some(spec) = enc.find_property("bps") {
            set_rate_prop(enc, "bps", &spec, kbps.saturating_mul(1000));
        }
    }

    /// Get the current encoder bitrate in kbps.
    pub fn get_bitrate_kbps(&self, enc: &gst::Element) -> u32 {
        if self.codec == CodecType::Fake {
            return 0;
        }
        let rockchip_bps =
            self.backend == EncoderBackend::Rockchip && self.encoder_factory.starts_with("mpp");
        if let Some(spec) = enc.find_property("bitrate") {
            let raw = get_rate_prop(enc, "bitrate", &spec);
            if rockchip_bps { raw / 1000 } else { raw }
        } else if let Some(spec) = enc.find_property("bps") {
            get_rate_prop(enc, "bps", &spec) / 1000
        } else {
            0
        }
    }

    /// Apply static encoder properties that [`Self::pipeline_fragment`]
    /// intentionally omits from the launch string for the Rockchip HW encoder.
    ///
    /// The Rockchip `mpph26xenc` fragment sets only `bitrate` so that
    /// `gst::parse::launch` can never fail on an unknown property — which left
    /// the GOP / IDR interval at the encoder default. That default *is* the
    /// framerate (~1 s IDR) on the BELABOX/rockchip-linux BSP, but it was never
    /// pinned, so the IDR cadence was an unverified assumption. This sets the
    /// GOP explicitly *after* launch, guarded by `find_property`, so an encoder
    /// lacking a property is simply skipped — keeping the launch robust while
    /// making the IDR cadence deterministic.
    ///
    /// - **GOP / IDR interval**: BSP `mpph26xenc` exposes `gop` (in frames);
    ///   mainline `rkmpph26xenc` and the SW/other HW encoders use `key-int-max`.
    ///   Whichever exists is pinned to `key_int_max` frames. (For the SW/NVENC/
    ///   VA-API paths the launch string already set this; re-setting the same
    ///   value is idempotent.)
    /// - **`header-mode=each-idr`** (Rockchip only): the BSP default is
    ///   "first-frame" — VPS/SPS/PPS are emitted only in the very first access
    ///   unit. Pinning each-idr makes every IDR independently decodable in the
    ///   elementary stream itself, instead of relying solely on downstream
    ///   `h265parse config-interval=-1` re-injection (a single point of failure
    ///   for mid-stream HLS segment joins and YouTube ingest).
    pub fn configure_static_props(&self, enc: &gst::Element, key_int_max: u32) {
        if self.codec == CodecType::Fake {
            return;
        }
        for name in ["gop", "key-int-max"] {
            if let Some(spec) = enc.find_property(name) {
                set_rate_prop(enc, name, &spec, key_int_max);
            }
        }
        // The "each-idr" nick is specific to GstMppEncHeaderMode, and
        // set_property_from_str panics on an unknown nick, so guard to the
        // Rockchip backend where the property and nick are known to exist.
        if self.backend == EncoderBackend::Rockchip && enc.find_property("header-mode").is_some() {
            enc.set_property_from_str("header-mode", "each-idr");
        }
        // Pin constant-bitrate rate control on the Rockchip MPP encoder. Its
        // default rc-mode lets the encoder overshoot the target heavily on
        // high-complexity / high-motion content — field-measured at ~2.7×
        // (6.8 Mbps emitted against a 2.5 Mbps target), which overflows the
        // per-link paced-queue AQM budget and turns into self-inflicted loss
        // (holes the receiver reports as discontinuities). CBR makes the
        // emitted rate track the target the adapter sets, matching the
        // transport's paced-rate model. The "cbr" nick is GstMppEncRcMode-
        // specific, so guard to Rockchip + property presence like header-mode.
        if self.backend == EncoderBackend::Rockchip && enc.find_property("rc-mode").is_some() {
            enc.set_property_from_str("rc-mode", "cbr");
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

    /// Build the GStreamer pipeline fragment string for this codec + backend.
    ///
    /// Returns the encoder element, e.g.:
    /// `x264enc name=enc tune=zerolatency speed-preset=ultrafast bitrate=2000 key-int-max=60`
    pub fn pipeline_fragment(
        &self,
        name: &str,
        bitrate_kbps: u32,
        key_int_max: u32,
        max_bitrate_kbps: u32,
    ) -> String {
        let factory = self.encoder_factory;
        match (self.backend, self.codec) {
            // ── NVENC (NVIDIA GPU) ──────────────────────────────────────
            (EncoderBackend::Nvenc, _) => {
                format!(
                    "{factory} name={name} bitrate={bps} gop-size={ki} \
                     preset=low-latency-hq rc-mode=cbr zerolatency=true",
                    factory = factory,
                    name = name,
                    bps = bitrate_kbps,
                    ki = key_int_max,
                )
            }
            // ── VA-API (Intel/AMD) ──────────────────────────────────────
            (EncoderBackend::Vaapi, _) => {
                // target-usage=7: fastest encode speed (good for live streaming).
                // cpb-size=0: let the driver auto-size the coded picture buffer.
                // rate-control=cbr: constant bitrate for predictable transport load.
                format!(
                    "{factory} name={name} bitrate={bps} key-int-max={ki} \
                     rate-control=cbr target-usage=7",
                    factory = factory,
                    name = name,
                    bps = bitrate_kbps,
                    ki = key_int_max,
                )
            }
            // ── QSV (Intel Quick Sync) ──────────────────────────────────
            (EncoderBackend::Qsv, _) => {
                format!(
                    "{factory} name={name} bitrate={bps} gop-size={ki} low-latency=true",
                    factory = factory,
                    name = name,
                    bps = bitrate_kbps,
                    ki = key_int_max,
                )
            }
            // ── Software H.264 (x264enc) ────────────────────────────────
            (EncoderBackend::Software, CodecType::H264) => {
                format!(
                    "x264enc name={name} tune=zerolatency speed-preset=ultrafast bitrate={bps} \
                     key-int-max={ki}",
                    name = name,
                    bps = bitrate_kbps,
                    ki = key_int_max,
                )
            }
            // ── Software H.265 (x265enc) ────────────────────────────────
            (EncoderBackend::Software, CodecType::H265) => {
                // tune=4 = zerolatency (must be numeric, not string).
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
            // ── Fake / identity ─────────────────────────────────────────
            (_, CodecType::Fake) => {
                format!("identity name={name} drop-probability=0.0")
            }
            // ── Vulkan H.264 (AMD RADV / any Vulkan 1.3 GPU) ─────────────
            (EncoderBackend::Vulkan, _) => {
                format!(
                    "{factory} name={name} bitrate={bps} rate-control=cbr",
                    factory = factory,
                    name = name,
                    bps = bitrate_kbps,
                )
            }
            // ── SVT-HEVC (fast multi-threaded software H.265) ─────────────
            (EncoderBackend::SvtHevc, _) => {
                // speed=9: fastest preset. enable-open-gop=false: IDR keyframes for HLS.
                format!(
                    "svthevcenc name={name} bitrate={bps} key-int-max={ki} speed=9 \
                     enable-open-gop=false",
                    name = name,
                    bps = bitrate_kbps,
                    ki = key_int_max,
                )
            }
            // ── Rockchip MPP (RK3588 etc.) ───────────────────────────────
            (EncoderBackend::Rockchip, _) => {
                // BSP/BELABOX `mpph26xenc`: the `bitrate` property is "Target
                // BPS" (bits/sec, NOT kbps); the mainline gst-plugins-bad
                // `rkmpph26xenc` follows the GstVideoEncoder convention
                // (`bitrate` in kbps). Branch on the factory name. The GOP/IDR
                // interval (BSP default `gop` == FPS, ~1 s IDR; verified in the
                // rockchip-linux gstmppenc.c source, DEFAULT_PROP_GOP=-1 ->
                // FPS) is NOT set here on purpose: keep the launch fragment to
                // just the bitrate knob so `parse::launch` can't fail on an
                // unknown property. It is pinned explicitly post-launch by
                // `configure_static_props()` (gop + header-mode), so the IDR
                // cadence is deterministic rather than an unverified default.
                // The adapter retunes the rate via set_bitrate_kbps.
                let bitrate_val = if factory.starts_with("mpp") {
                    bitrate_kbps.saturating_mul(1000) // BSP/BELABOX: BPS
                } else {
                    bitrate_kbps // mainline rkmpp: kbps
                };
                format!(
                    "{factory} name={name} bitrate={v}",
                    factory = factory,
                    name = name,
                    v = bitrate_val
                )
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
    fn relay_parser_fragments_inject_headers() {
        let h264 = CodecType::H264.relay_parser_fragment();
        assert!(h264.starts_with("h264parse"));
        assert!(h264.contains("config-interval=-1"));
        assert!(h264.contains("disable-passthrough=true"));

        let h265 = CodecType::H265.relay_parser_fragment();
        assert!(h265.starts_with("h265parse"));
        assert!(h265.contains("config-interval=-1"));
        // NOTE: disable-passthrough=true is intentionally absent for H.265 — see
        // relay_parser_fragment() comment for why it breaks GStreamer ≤1.24 receivers.
        assert!(!h265.contains("disable-passthrough=true"));
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
    fn new_defaults_to_software() {
        let ctrl = CodecController::new(CodecType::H264);
        assert_eq!(ctrl.backend(), EncoderBackend::Software);
        assert_eq!(ctrl.encoder_factory_name(), "x264enc");
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

    #[test]
    fn encoder_backend_display() {
        assert_eq!(format!("{}", EncoderBackend::Software), "software");
        assert_eq!(format!("{}", EncoderBackend::Nvenc), "NVENC");
        assert_eq!(format!("{}", EncoderBackend::Vaapi), "VA-API");
        assert_eq!(format!("{}", EncoderBackend::Qsv), "QSV");
    }
}
