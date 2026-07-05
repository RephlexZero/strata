//! Sender mode: encode (or pass through) and transmit over bonded links.

use gst::MessageView;
use gst::prelude::*;
use std::sync::Mutex;
use strata_bonding::metrics::MetricsServer;

use crate::cli::SenderArgs;
use crate::hotswap::{
    add_source_branch, handle_source_switch, handle_toggle_link, run_control_socket,
};
use crate::stats::{resolve_interface_for_uri, serialize_bonding_stats};
use crate::util::{configure_mpegtsmux, register_plugins};

pub(crate) fn run_sender(args: &SenderArgs) -> Result<(), Box<dyn std::error::Error>> {
    let dest_str = args.dest.as_str();
    let stats_dest = args.stats_dest.as_str();
    let bitrate_kbps = args.bitrate;
    let framerate = args.framerate;
    let add_audio = args.audio;
    let config_path = args.config.as_str();
    let source_mode = args.source.as_str();
    let device_path = args.device.as_str();
    let source_uri = args.uri.as_str();
    let control_sock_path = args.control.as_str();
    let resolution = args.resolution.as_str();
    let passthrough = args.passthrough;
    let metrics_port = args.metrics_port;
    let codec_str = args.codec.as_str();
    let min_bitrate_kbps = args.min_bitrate;
    let max_bitrate_kbps = args.max_bitrate;
    let startup_ramp_ms = args.startup_ramp_ms;
    let startup_floor_kbps = args.startup_floor_kbps;

    // Parse resolution (WxH)
    let (res_w, res_h) = {
        let parts: Vec<&str> = resolution.split('x').collect();
        if parts.len() == 2 {
            (
                parts[0].parse::<u32>().unwrap_or(1280),
                parts[1].parse::<u32>().unwrap_or(720),
            )
        } else {
            (1280, 720)
        }
    };

    register_plugins()?;

    // ── Passthrough mode: remux file source into MPEG-TS without re-encoding ──
    if passthrough {
        return run_sender_passthrough(
            source_uri,
            dest_str,
            config_path,
            stats_dest,
            control_sock_path,
        );
    }

    // ── Build pipeline with input-selector for hot-swap support ──
    //
    // Pipeline structure:
    //   videotestsrc ! capsfilter ! queue ─┐
    //                                      ├─ input-selector ! x264enc ! [audio] ! mux ! stratasink
    //   [dynamic v4l2/uri sources] ────────┘
    //
    // The initial source (--source flag) determines which branch is active.
    // Additional branches are added dynamically via the control socket.
    // IDR (keyframe) interval. Was 2 s (framerate × 2) to match the 2 s
    // HLS segment duration. On lossy bonded cellular that makes one lost
    // packet corrupt up to ~2 s of video (every P-frame references the
    // damaged frame until the next IDR — the "clear for a moment then
    // grey/blocky" symptom). A 1 s IDR halves that error-propagation
    // window while still keeping every segment keyframe-aligned (the
    // receiver's hlssink closes a segment at the first keyframe past
    // target-duration=1s, so each segment is one GOP), so YouTube ingest
    // stays happy. key-int-max is in frames, so `framerate` == 1 s.
    let key_int = framerate;

    // Parse codec type
    let codec_type = gststrata::codec::CodecType::from_str_loose(codec_str).unwrap_or_else(|| {
        eprintln!("Unknown codec '{}', defaulting to h264", codec_str);
        gststrata::codec::CodecType::H264
    });
    // Probe for the best available encoder (HW → SW fallback).
    let codec_ctrl = gststrata::codec::CodecController::with_auto_backend(codec_type);
    let codec_type = if codec_ctrl.encoder_factory_name() == "identity" {
        // Fake codec doesn't need a real encoder
        codec_type
    } else if gst::ElementFactory::find(codec_ctrl.encoder_factory_name()).is_none() {
        // Selected encoder not available — fall back to H.264 software
        eprintln!(
            "Warning: encoder '{}' not available — falling back to h264",
            codec_ctrl.encoder_factory_name()
        );
        gststrata::codec::CodecType::H264
    } else {
        codec_type
    };
    // Re-resolve if we changed codec type above
    let codec_ctrl = if codec_ctrl.codec() != codec_type {
        gststrata::codec::CodecController::with_auto_backend(codec_type)
    } else {
        codec_ctrl
    };
    eprintln!(
        "Encoder: {} ({} backend)",
        codec_ctrl.encoder_factory_name(),
        codec_ctrl.backend()
    );

    // Resolve min/max from CLI or profile defaults
    let profile = strata_protocol::profiles::lookup_profile(
        Some(resolution),
        Some(framerate),
        Some(codec_str),
    );
    let min_bitrate_kbps_val = min_bitrate_kbps.unwrap_or(profile.min_kbps);
    let max_bitrate_kbps_val = max_bitrate_kbps.unwrap_or(profile.max_kbps);

    // Probe for available AAC encoder. Preference is quality-first
    // (fdkaacenc > avenc_aac) then broadly-available fallbacks. voaacenc ships
    // in stock gstreamer1.0-plugins-bad (present on the Orange Pi 5 / Ubuntu
    // 22.04 default install), so it's the realistic fallback when the
    // libav/fdk packages aren't installed. All of these expose a `bitrate`
    // property in bits/sec, so the element name is a drop-in substitution.
    let aac_enc_element = ["fdkaacenc", "avenc_aac", "voaacenc", "faac"]
        .iter()
        .find(|&&name| gst::ElementFactory::find(name).is_some())
        .copied()
        .unwrap_or("fdkaacenc");
    eprintln!("AAC encoder: {}", aac_enc_element);

    // Build the pipeline with input-selector.
    // The test source is always available as the fallback.
    //
    // Pipeline:
    //   source → input-selector → encoder → queue → mpegtsmux → stratasink
    //   [optional] audiotestsrc → <aac_enc> → aacparse → queue → mpegtsmux
    let audio_fragment = if add_audio {
        format!(
            " audiotestsrc is-live=true wave=silence ! audioconvert ! audioresample ! {aac_enc_element} bitrate=128000 ! aacparse ! queue ! mux."
        )
    } else {
        String::new()
    };

    let video_to_mux = "! queue ! mux.".to_string();

    let enc_fragment =
        codec_ctrl.pipeline_fragment("enc", bitrate_kbps, key_int, max_bitrate_kbps_val);

    // Add a parser after the encoder to normalise stream-format to byte-stream.
    // Hardware encoders (VA-API, NVENC, Vulkan) can emit hvc1/avc1 (length-prefixed)
    // which mpegtsmux cannot accept — h265parse/h264parse converts to byte-stream.
    // For software encoders this is a cheap passthrough.
    let parser_fragment = match codec_ctrl.codec() {
        gststrata::codec::CodecType::Fake => "identity".to_string(),
        ct => format!(
            "{} config-interval=-1 disable-passthrough=true",
            ct.parser_factory()
        ),
    };

    // VA-API and Vulkan encoders negotiate 10-bit (P010_10LE) input by default when
    // upstream offers it.  mpegtsmux does not handle 10-bit H.265 properly — it
    // produces corrupt PES packets that tsdemux on the receiver cannot parse.
    // Force NV12 (8-bit YUV 4:2:0) so the encoder produces HEVC Main (8-bit) output.
    // videoconvert is cheap when the source already outputs NV12 (most webcams do).
    let hw_fmt_conv = match codec_ctrl.backend() {
        gststrata::codec::EncoderBackend::Vaapi
        | gststrata::codec::EncoderBackend::Vulkan
        | gststrata::codec::EncoderBackend::Nvenc
        // Rockchip rkmpp encoders take NV12 (8-bit 4:2:0); convert so a YUYV
        // USB camera or 10-bit source negotiates cleanly.
        | gststrata::codec::EncoderBackend::Rockchip => "! videoconvert ! video/x-raw,format=NV12 ",
        _ => "",
    };

    let pipeline_str = format!(
        "videotestsrc name=testsrc is-live=true pattern=ball \
         ! video/x-raw,width={w},height={h},framerate={fps}/1 \
         ! queue name=testq max-size-buffers=3 ! sel. \
         input-selector name=sel \
         {hw_fmt_conv}! {enc_fragment} \
         ! {parser_fragment} \
         {video_to_mux}{audio} \
         mpegtsmux name=mux alignment=7 pat-interval=9000 pmt-interval=9000 \
         ! stratasink name=rsink",
        w = res_w,
        h = res_h,
        fps = framerate,
        hw_fmt_conv = hw_fmt_conv,
        enc_fragment = enc_fragment,
        parser_fragment = parser_fragment,
        video_to_mux = video_to_mux,
        audio = audio_fragment,
    );

    eprintln!("Sender Pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "Failed to cast to pipeline")?;

    configure_mpegtsmux(&pipeline);

    // Pin the GOP / IDR interval (and per-IDR parameter sets on Rockchip) that
    // the launch string deliberately omitted for the HW encoder so a
    // parse::launch could not fail on an unknown property. Done post-launch and
    // guarded by find_property, so the IDR cadence is deterministic (~1 s)
    // instead of an unverified encoder default — a known under-investigated
    // variable in the grey/ref-loss artifact.
    if let Some(enc) = pipeline.by_name("enc") {
        codec_ctrl.configure_static_props(&enc, key_int);
        eprintln!(
            "Encoder static props applied: GOP/key-int={} frames (~{:.0} ms IDR){}",
            key_int,
            (key_int as f64 / framerate.max(1) as f64) * 1000.0,
            if codec_ctrl.backend() == gststrata::codec::EncoderBackend::Rockchip {
                ", header-mode=each-idr (if supported)"
            } else {
                ""
            }
        );
    }

    // Keep a handle to the input-selector and its test-source pad
    let selector = pipeline
        .by_name("sel")
        .ok_or("Failed to find input-selector")?;
    let test_pad = selector
        .static_pad("sink_0")
        .or_else(|| {
            // gst::parse may name it differently; find the first sink pad
            selector.iterate_sink_pads().into_iter().flatten().next()
        })
        .ok_or("Failed to find test source pad on input-selector")?;

    // If the initial source is not 'test', try to create the requested
    // source and make it active instead.
    if source_mode != "test" {
        match add_source_branch(
            &pipeline,
            &selector,
            source_mode,
            device_path,
            source_uri,
            framerate,
            res_w,
            res_h,
        ) {
            Ok(new_pad) => {
                selector.set_property("active-pad", &new_pad);
                eprintln!("Initial source: {} (active)", source_mode);
            }
            Err(e) => {
                eprintln!(
                    "Warning: failed to create {} source ({}), falling back to test",
                    source_mode, e
                );
            }
        }
    }

    // ── Configure destinations ──
    if let Some(sink) = pipeline.by_name("rsink") {
        // Build a URI→interface map from the TOML config so per-link
        // interface bindings in the config take priority over the
        // routing-table fallback below.
        let mut toml_iface_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        if !config_path.is_empty() {
            let config_toml = std::fs::read_to_string(config_path)
                .map_err(|e| format!("Failed to read config file '{}': {}", config_path, e))?;
            sink.set_property("config", &config_toml);
            eprintln!("Applied config from {}", config_path);

            // Parse [[links]] to extract uri→interface mappings.
            if let Ok(toml::Value::Table(ref tbl)) = toml::from_str::<toml::Value>(&config_toml)
                && let Some(toml::Value::Array(links)) = tbl.get("links")
            {
                for link in links {
                    if let (Some(uri_v), Some(iface_v)) = (link.get("uri"), link.get("interface"))
                        && let (Some(uri_s), Some(iface_s)) = (uri_v.as_str(), iface_v.as_str())
                    {
                        toml_iface_map.insert(uri_s.to_string(), iface_s.to_string());
                    }
                }
            }
        }

        for (idx, uri) in dest_str.split(',').enumerate() {
            let uri = uri.trim();
            if uri.is_empty() {
                continue;
            }
            let pad = sink
                .request_pad_simple("link_%u")
                .ok_or("Failed to request link pad")?;
            pad.set_property("uri", uri);

            // Use TOML-specified interface if present, otherwise fall back
            // to routing-table lookup (best-effort).
            let iface = toml_iface_map
                .get(uri)
                .cloned()
                .or_else(|| resolve_interface_for_uri(uri));
            if let Some(iface) = iface {
                pad.set_property("interface", &iface);
                eprintln!("Configured link {} -> {} (via {})", idx, uri, iface);
            } else {
                eprintln!("Configured link {} -> {}", idx, uri);
            }
        }
    } else {
        return Err("Failed to find stratasink element".into());
    }

    // ── Set bitrate adaptation envelope ──
    if let Some(sink) = pipeline.by_name("rsink") {
        let strata_sink = sink
            .downcast::<gststrata::sink::StrataSink>()
            .expect("rsink is not a StrataSink");
        strata_sink.set_adaptation_envelope(
            min_bitrate_kbps_val,
            max_bitrate_kbps_val,
            bitrate_kbps,
            startup_ramp_ms,
            startup_floor_kbps,
        );
        eprintln!(
            "Adaptation envelope: {}–{} kbps (startup ramp {} ms, floor {} kbps)",
            min_bitrate_kbps_val, max_bitrate_kbps_val, startup_ramp_ms, startup_floor_kbps
        );
    }

    // ── Stats relay (always if stats-dest is configured) ──
    let mut stats_socket = None;
    if !stats_dest.is_empty() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
        stats_socket = Some(sock);
        eprintln!("Stats relay → {}", stats_dest);
    }

    // ── Control socket for hot-swap commands ──
    // Remove stale socket file, then listen.
    let _ = std::fs::remove_file(control_sock_path);
    let control_socket_path_owned = control_sock_path.to_string();
    let pipeline_weak_ctrl = pipeline.downgrade();
    std::thread::Builder::new()
        .name("ctrl-sock".into())
        .spawn(move || {
            run_control_socket(&control_socket_path_owned, pipeline_weak_ctrl);
        })?;

    // ── Graceful shutdown ──
    let pipeline_weak = pipeline.downgrade();
    ctrlc::set_handler(move || {
        eprintln!("\nReceived shutdown signal. Sending EOS to pipeline...");
        if let Some(pipeline) = pipeline_weak.upgrade() {
            let _ = pipeline.send_event(gst::event::Eos::new());
        }
    })
    .expect("Error setting signal handler");

    pipeline.set_state(gst::State::Playing)?;
    eprintln!("Sender running (source={source_mode})... Press Ctrl+C to stop.");

    // ── Prometheus metrics server ──
    let _metrics_server = if let Some(port) = metrics_port {
        let sink_element = pipeline
            .by_name("rsink")
            .expect("Failed to find stratasink element");
        let strata_sink = sink_element
            .downcast::<gststrata::sink::StrataSink>()
            .expect("rsink is not a StrataSink");
        match strata_sink.metrics_handle() {
            Some(handle) => {
                let addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
                match MetricsServer::start(addr, handle) {
                    Ok(server) => {
                        eprintln!("Prometheus metrics → http://0.0.0.0:{}/metrics", port);
                        Some(server)
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: failed to start metrics server on port {}: {}",
                            port, e
                        );
                        None
                    }
                }
            }
            None => {
                eprintln!("Warning: metrics handle not available (pipeline not started?)");
                None
            }
        }
    } else {
        None
    };

    // ── Disabled link tracker (for toggle_link re-enable) ──
    let disabled_links: Mutex<std::collections::HashMap<String, (String, String)>> =
        Mutex::new(std::collections::HashMap::new());

    // ── Bus message loop ──
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        match msg.view() {
            MessageView::Eos(..) => {
                eprintln!("Got EOS. Pipeline finished.");
                break;
            }
            MessageView::Error(err) => {
                eprintln!("Error: {}", err.error());
                pipeline.set_state(gst::State::Null)?;
                return Err(Box::new(err.error().clone()));
            }
            MessageView::Application(app) => {
                // Hot-swap commands posted by the control socket thread.
                if let Some(s) = app.structure() {
                    if s.name() == "source-switch" {
                        handle_source_switch(
                            &pipeline, &selector, &test_pad, s, framerate, res_w, res_h,
                        );
                    } else if s.name() == "toggle-link"
                        && let Some(sink) = pipeline.by_name("rsink")
                    {
                        handle_toggle_link(&sink, s, &disabled_links);
                    }
                }
            }
            MessageView::Element(element) => {
                if let Some(s) = element.structure() {
                    if s.name() == "bitrate-command" {
                        if let Ok(target_kbps) = s.get::<u32>("target-kbps")
                            && let Some(enc) = pipeline.by_name("enc")
                        {
                            let current = codec_ctrl.get_bitrate_kbps(&enc);
                            let clamped = target_kbps.max(min_bitrate_kbps_val);
                            if (clamped as i32 - current as i32).unsigned_abs() > 50 {
                                let reason = s.get::<String>("reason").unwrap_or_default();
                                let stage = s.get::<String>("stage").unwrap_or_default();
                                eprintln!(
                                    "Bitrate: {} -> {} kbps (reason={}, stage={})",
                                    current, clamped, reason, stage
                                );
                                codec_ctrl.set_bitrate_kbps(&enc, clamped);
                            }
                        }
                        // Forward degradation stage to scheduler
                        if let Ok(stage_str) = s.get::<String>("stage")
                            && let Some(sink) = pipeline.by_name("rsink")
                        {
                            let strata_sink = sink
                                .downcast::<gststrata::sink::StrataSink>()
                                .expect("rsink is not a StrataSink");
                            let stage = match stage_str.as_str() {
                                    "DropDisposable" => strata_bonding::media::priority::DegradationStage::DropDisposable,
                                    "ReduceBitrate" => strata_bonding::media::priority::DegradationStage::ReduceBitrate,
                                    "ProtectKeyframes" => strata_bonding::media::priority::DegradationStage::ProtectKeyframes,
                                    "KeyframeOnly" => strata_bonding::media::priority::DegradationStage::KeyframeOnly,
                                    _ => strata_bonding::media::priority::DegradationStage::Normal,
                                };
                            strata_sink.set_degradation_stage(stage);

                            // Forward adaptive FEC overhead to the transport
                            // encoders so repair strength tracks the measured
                            // loss regime instead of the fixed default.
                            if let Ok(fec_overhead) = s.get::<f64>("fec-overhead") {
                                strata_sink.set_fec_overhead(fec_overhead);
                            }
                        }
                    } else if s.name() == "strata-stats"
                        && let Some(sock) = &stats_socket
                    {
                        let json = serialize_bonding_stats(s).to_string();
                        let _ = sock.send_to(json.as_bytes(), stats_dest);
                    }
                }
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;
    let _ = std::fs::remove_file(control_sock_path);

    Ok(())
}

// ── Interface resolution ────────────────────────────────────────────

/// Resolve which OS network interface routes to the host in an address.
///
/// Parses the host from `host:port` and runs
/// `ip route get <host>` to determine the outgoing interface.
/// Run the sender in passthrough mode — remux a file source into MPEG-TS
/// without decoding or re-encoding. The video and audio elementary streams
/// are parsed and fed directly into mpegtsmux.
fn run_sender_passthrough(
    source_uri: &str,
    dest_str: &str,
    config_path: &str,
    stats_dest: &str,
    control_sock_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if source_uri.is_empty() {
        return Err("passthrough mode requires --uri".into());
    }

    // Build pipeline: uridecodebin → parsebin → mpegtsmux → stratasink
    // uridecodebin with `caps` restricted to parsed formats avoids actual decoding.
    let pipeline_str = format!(
        "uridecodebin name=urisrc uri=\"{uri}\" \
         ! parsebin name=pbin \
         mpegtsmux name=mux alignment=7 pat-interval=9000 pmt-interval=9000 \
         ! stratasink name=rsink",
        uri = source_uri,
    );

    eprintln!("Passthrough Pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "Failed to cast to pipeline")?;

    configure_mpegtsmux(&pipeline);

    // Wire up parsebin's dynamic pads to mpegtsmux
    let mux = pipeline.by_name("mux").ok_or("Failed to find mux")?;
    let mux_weak = mux.downgrade();
    let pbin = pipeline.by_name("pbin").ok_or("Failed to find pbin")?;
    pbin.connect_pad_added(move |_element, pad| {
        let Some(mux) = mux_weak.upgrade() else {
            return;
        };
        let caps = pad.current_caps();
        let caps_str = caps.as_ref().map(|c| c.to_string()).unwrap_or_default();
        eprintln!("parsebin pad-added: {} (caps: {})", pad.name(), caps_str);

        // Request a sink pad on mpegtsmux and link
        if let Some(mux_pad) = mux.request_pad_simple("sink_%d") {
            if pad.link(&mux_pad).is_ok() {
                eprintln!("Linked {} → {}", pad.name(), mux_pad.name());
            } else {
                eprintln!("Failed to link {} → {}", pad.name(), mux_pad.name());
            }
        }
    });

    // Configure stratasink destinations
    if let Some(sink) = pipeline.by_name("rsink") {
        if !config_path.is_empty() {
            let config_toml = std::fs::read_to_string(config_path)
                .map_err(|e| format!("Failed to read config: {e}"))?;
            sink.set_property("config", &config_toml);
        }
        for (idx, uri) in dest_str.split(',').enumerate() {
            let uri = uri.trim();
            if uri.is_empty() {
                continue;
            }
            let pad = sink
                .request_pad_simple("link_%u")
                .ok_or("Failed to request link pad")?;
            pad.set_property("uri", uri);
            if let Some(iface) = resolve_interface_for_uri(uri) {
                pad.set_property("interface", &iface);
                eprintln!("Link {} → {} (via {})", idx, uri, iface);
            } else {
                eprintln!("Link {} → {}", idx, uri);
            }
        }
    }

    // Start pipeline
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("Failed to set pipeline to Playing: {e}"))?;

    // Start control socket for hot-swap commands
    let _ = std::fs::remove_file(control_sock_path);
    let pipeline_weak = pipeline.downgrade();
    let control_path = control_sock_path.to_string();
    std::thread::Builder::new()
        .name("ctrl-sock".into())
        .spawn(move || {
            run_control_socket(&control_path, pipeline_weak);
        })?;

    // Stats relay
    let mut stats_socket = None;
    if !stats_dest.is_empty() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
        stats_socket = Some(sock);
        eprintln!("Stats relay → {}", stats_dest);
    }

    // Graceful shutdown
    let pipeline_weak = pipeline.downgrade();
    ctrlc::set_handler(move || {
        if let Some(p) = pipeline_weak.upgrade() {
            let _ = p.send_event(gst::event::Eos::new());
        }
    })?;

    eprintln!("Passthrough sender running (uri={})...", source_uri);

    // Bus loop — handle EOS (auto-loop), errors, and stats
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        match msg.view() {
            MessageView::Error(err) => {
                eprintln!(
                    "Pipeline error: {} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                );
                break;
            }
            MessageView::Eos(_) => {
                // Loop: seek back to the beginning
                eprintln!("EOS — looping playback");
                let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT;
                if pipeline.seek_simple(flags, gst::ClockTime::ZERO).is_err() {
                    eprintln!("Seek failed, ending stream");
                    break;
                }
            }
            MessageView::Element(elem) => {
                if let Some(s) = elem.structure()
                    && s.name() == "bonding-stats"
                    && let (Some(sock), Ok(addr)) =
                        (&stats_socket, stats_dest.parse::<std::net::SocketAddr>())
                {
                    let json = serialize_bonding_stats(s).to_string();
                    let _ = sock.send_to(json.as_bytes(), addr);
                }
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null)?;
    Ok(())
}
