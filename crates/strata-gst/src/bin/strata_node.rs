use gst::MessageView;
use gst::prelude::*;
use std::env;
use std::sync::{Arc, Mutex};
use strata_bonding::metrics::MetricsServer;

use gststrata::hls_upload;

const SENDER_HELP: &str = r#"
USAGE: strata-node sender [OPTIONS] --dest <ADDR[,ADDR...]>

OPTIONS:
  --dest <addrs>      Comma-separated destination addresses (required)
                      e.g. 192.168.1.100:5000,10.0.0.100:5000
  --source <mode>     Initial video source mode (default: test)
                        test   - SMPTE colour bars (videotestsrc)
                        v4l2   - Camera / HDMI capture card (v4l2src)
                        uri    - File or network stream (uridecodebin)
  --device <path>     V4L2 device path (default: /dev/video0, used with --source v4l2)
  --uri <uri>         Media URI for uridecodebin (used with --source uri)
                      e.g. file:///home/user/video.mp4
  --bitrate <kbps>    Target encoder bitrate in kbps (default: 2000)
  --codec <codec>      Video codec: h265 (default) or h264
  --min-bitrate <kbps> Minimum bitrate for adaptation (default: from profile)
  --max-bitrate <kbps> Maximum bitrate for adaptation (default: from profile)
  --framerate <fps>   Video framerate (default: 30)
  --audio             Add silent AAC audio track (required for RTMP targets)
  --config <path>     Path to TOML config file (see Configuration Reference)
  --stats-dest <addr> UDP address to relay stats JSON (e.g. 127.0.0.1:9100)
  --relay-url <url>   RTMP/RTMPS URL to relay encoded stream to in parallel
                      (tees output: one copy goes via Strata, another via RTMP)
  --metrics-port <port> Start Prometheus metrics endpoint on this port
                        (serves /metrics on 0.0.0.0:<port>)
  --control <path>    Unix socket path for hot-swap commands
                      (default: /tmp/strata-pipeline.sock)
  --help              Show this help

SOURCE HOT-SWAP:
  While the pipeline is running, send JSON commands to the control socket
  to switch video sources without stopping the stream:

    echo '{"cmd":"switch_source","mode":"test","pattern":"ball"}' | \
      socat - UNIX:/tmp/strata-pipeline.sock

  Supported commands:
    {"cmd":"switch_source","mode":"test","pattern":"<pattern>"}
      Patterns: smpte, ball, snow, black, white, red, green, blue
    {"cmd":"switch_source","mode":"v4l2","device":"/dev/video0"}
    {"cmd":"switch_source","mode":"uri","uri":"file:///path/to/video.mp4"}

EXAMPLES:
  # Test pattern over two cellular links
  strata-node sender --dest server:5000,server:5002 \
    --config sender.toml

  # 1080p30 with audio for YouTube relay via receiver
  strata-node sender --source test --framerate 30 --audio --bitrate 2000 \
    --dest receiver:5000,receiver:5002,receiver:5004

  # Direct RTMP relay to YouTube (sender tees output to Strata + RTMP)
  strata-node sender --source test --bitrate 2000 \
    --dest receiver:5000,receiver:5002 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"

  # HDMI capture card to cloud receiver
  strata-node sender --source v4l2 --device /dev/video0 \
    --dest cloud.example.com:5000,cloud.example.com:5002 \
    --bitrate 2000 --config sender.toml
"#;

const RECEIVER_HELP: &str = r#"
USAGE: strata-node receiver [OPTIONS] --bind <ADDR>

OPTIONS:
  --bind <addr>       Bind address (required), e.g. 0.0.0.0:5000
                      Multiple bind addresses: 0.0.0.0:5000,0.0.0.0:5002
  --output <path>     Record to file (.ts = raw MPEG-TS, .mp4 = remuxed)
  --relay-url <url>   RTMP/RTMPS URL to relay the received stream to
                      e.g. rtmp://a.rtmp.youtube.com/live2/STREAM_KEY
  --codec <codec>     Codec of incoming stream: h265 (default) or h264
  --config <path>     Path to TOML config file (see Configuration Reference)
  --metrics-port <port> Start Prometheus metrics endpoint on this port
                        (serves /metrics on 0.0.0.0:<port>)
  --help              Show this help

EXAMPLES:
  # Receive and monitor (no file output)
  strata-node receiver --bind 0.0.0.0:5000

  # Receive bonded stream and relay to YouTube
  strata-node receiver --bind 0.0.0.0:5000,0.0.0.0:5002,0.0.0.0:5004 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"

  # Receive H.265 stream and relay
  strata-node receiver --bind 0.0.0.0:5000 --codec h265 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"

  # Receive and record to MPEG-TS file
  strata-node receiver --bind 0.0.0.0:5000 --output capture.ts

  # Receive with config
  strata-node receiver --bind 0.0.0.0:5000 --config receiver.toml
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging for production use.
    // Controlled by RUST_LOG env var (e.g., RUST_LOG=info,strata_bonding=debug).
    strata_bonding::init();

    gst::init()?;

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <sender|receiver> [--help]", args[0]);
        eprintln!("Run with --help after the mode for detailed usage.");
        std::process::exit(1);
    }

    let mode = &args[1];

    match mode.as_str() {
        "sender" => run_sender(&args[2..]),
        "receiver" => run_receiver(&args[2..]),
        "--help" | "-h" | "help" => {
            eprintln!("Usage: {} <sender|receiver> [args]\n", args[0]);
            eprintln!("Modes:");
            eprintln!("  sender    Encode and transmit video over bonded Strata links");
            eprintln!("  receiver  Receive and reassemble bonded Strata stream\n");
            eprintln!(
                "Run `{} sender --help` or `{} receiver --help` for mode-specific options.",
                args[0], args[0]
            );
            Ok(())
        }
        _ => {
            eprintln!("Unknown mode: {}", mode);
            std::process::exit(1);
        }
    }
}

fn register_plugins() -> Result<(), gst::glib::BoolError> {
    gststrata::sink::register(None)?;
    gststrata::src::register(None)?;
    Ok(())
}

/// Disable mpegtsmux skew corrections when the property is available (GStreamer ≥1.28).
///
/// In a bonding transport the receiver remuxes or writes to file, so we want to
/// preserve original timestamps rather than correcting for clock drift.
fn configure_mpegtsmux(pipeline: &gst::Pipeline) {
    if let Some(mux) = pipeline.by_name("mux")
        && mux.find_property("skew-corrections").is_some()
    {
        mux.set_property("skew-corrections", false);
        eprintln!("mpegtsmux: disabled skew-corrections (GStreamer ≥1.28)");
    }
}

fn run_sender(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut dest_str = "";
    let mut stats_dest = "";
    let mut bitrate_kbps = 1000u32;
    let mut framerate = 30u32;
    let mut add_audio = false;
    let mut config_path = "";
    let mut source_mode = "test";
    let mut device_path = "/dev/video0";
    let mut source_uri = "";
    let mut control_sock_path = "/tmp/strata-pipeline.sock";
    let mut resolution = "1280x720";
    let mut relay_url = "";
    let mut passthrough = false;
    let mut metrics_port: Option<u16> = None;
    let mut codec_str = "h265";
    let mut min_bitrate_kbps: Option<u32> = None;
    let mut max_bitrate_kbps: Option<u32> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                eprint!("{SENDER_HELP}");
                return Ok(());
            }
            "--dest" if i + 1 < args.len() => {
                dest_str = &args[i + 1];
                i += 1;
            }
            "--stats-dest" if i + 1 < args.len() => {
                stats_dest = &args[i + 1];
                i += 1;
            }
            "--bitrate" if i + 1 < args.len() => {
                bitrate_kbps = args[i + 1].parse().unwrap_or(2000);
                i += 1;
            }
            "--codec" if i + 1 < args.len() => {
                codec_str = &args[i + 1];
                i += 1;
            }
            "--min-bitrate" if i + 1 < args.len() => {
                min_bitrate_kbps = Some(args[i + 1].parse().unwrap_or(500));
                i += 1;
            }
            "--max-bitrate" if i + 1 < args.len() => {
                max_bitrate_kbps = Some(args[i + 1].parse().unwrap_or(25000));
                i += 1;
            }
            "--framerate" if i + 1 < args.len() => {
                framerate = args[i + 1].parse().unwrap_or(30);
                i += 1;
            }
            "--audio" => {
                add_audio = true;
            }
            "--config" if i + 1 < args.len() => {
                config_path = &args[i + 1];
                i += 1;
            }
            "--source" if i + 1 < args.len() => {
                source_mode = &args[i + 1];
                i += 1;
            }
            "--device" if i + 1 < args.len() => {
                device_path = &args[i + 1];
                i += 1;
            }
            "--uri" if i + 1 < args.len() => {
                source_uri = &args[i + 1];
                i += 1;
            }
            "--control" if i + 1 < args.len() => {
                control_sock_path = &args[i + 1];
                i += 1;
            }
            "--resolution" if i + 1 < args.len() => {
                resolution = &args[i + 1];
                i += 1;
            }
            "--relay-url" if i + 1 < args.len() => {
                relay_url = &args[i + 1];
                i += 1;
            }
            "--passthrough" => {
                passthrough = true;
            }
            "--metrics-port" if i + 1 < args.len() => {
                metrics_port = Some(args[i + 1].parse().unwrap_or_else(|_| {
                    eprintln!("Invalid metrics port: {}", args[i + 1]);
                    std::process::exit(1);
                }));
                i += 1;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                eprint!("{SENDER_HELP}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    if dest_str.is_empty() {
        eprintln!("Missing --dest <url>");
        eprint!("{SENDER_HELP}");
        std::process::exit(1);
    }

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
            relay_url,
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
    let key_int = framerate * 2;

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
    let profile =
        strata_common::profiles::lookup_profile(Some(resolution), Some(framerate), Some(codec_str));
    let min_bitrate_kbps_val = min_bitrate_kbps.unwrap_or(profile.min_kbps);
    let max_bitrate_kbps_val = max_bitrate_kbps.unwrap_or(profile.max_kbps);

    // Build the pipeline with input-selector.
    // The test source is always available as the fallback.
    //
    // When --relay-url is set, the encoded video and audio are teed:
    //   encoder → tee → queue → mpegtsmux → stratasink   (Strata)
    //                 └→ queue → parser → [e]flvmux → rtmpsink (RTMP)
    //   voaacenc → tee → aacparse → queue → mpegtsmux         (Strata)
    //                 └→ queue → aacparse → [e]flvmux          (RTMP)
    //
    // For HLS relay (YouTube HLS ingest):
    //   encoder → tee → queue → mpegtsmux → stratasink         (Strata)
    //                 └→ queue → parser → hlssink2.video        (HLS)
    //   voaacenc → tee → aacparse → queue → mpegtsmux          (Strata)
    //                 └→ queue → aacparse → hlssink2.audio      (HLS)
    let mut use_relay = !relay_url.is_empty();
    let use_hls_relay = use_relay && hls_upload::is_hls_url(relay_url);

    // Validate that the relay muxer element is actually available before trying.
    // eflvmux (for H.265) requires GStreamer 1.24+. If it's missing, disable the
    // relay and log a warning rather than failing the whole pipeline.
    if use_relay && !use_hls_relay {
        let muxer_factory = codec_ctrl.relay_muxer_factory_name();
        if gst::ElementFactory::find(muxer_factory).is_none() {
            eprintln!(
                "Warning: relay muxer '{}' not available (requires GStreamer >= 1.24) — \
                 RTMP relay disabled; stream will still start without relay",
                muxer_factory
            );
            use_relay = false;
        }
    }
    if use_hls_relay && gst::ElementFactory::find("hlssink2").is_none() {
        eprintln!(
            "Warning: hlssink2 not available — HLS relay disabled; \
             stream will still start without relay"
        );
        use_relay = false;
    }

    // When relay is enabled, force audio on (RTMP requires it, HLS expects it)
    if use_relay {
        add_audio = true;
    }

    // For HLS, create a temp directory for segment files.
    // Prefer /dev/shm (RAM-backed tmpfs) to avoid flash/eMMC wear on SBCs.
    let hls_tmp_dir = if use_hls_relay {
        let dir = hls_upload::tmpfs_segment_dir(&format!("strata-hls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("failed to create HLS temp dir");
        eprintln!(
            "HLS temp dir: {} (tmpfs={})",
            dir.display(),
            dir.starts_with("/dev/shm")
        );
        Some(dir)
    } else {
        None
    };

    let (audio_fragment, relay_fragment) = if use_hls_relay {
        let hls_dir = hls_tmp_dir.as_ref().unwrap();
        let seg_location = hls_dir.join("segment%05d.ts");
        let pl_location = hls_dir.join("playlist.m3u8");
        // Audio with tee — one path to Strata mux, another to hlssink2
        let audio = " audiotestsrc is-live=true wave=silence \
            ! audioconvert ! audioresample ! voaacenc bitrate=128000 \
            ! tee name=atee \
            atee. ! queue ! aacparse ! mux. \
            atee. ! queue ! aacparse ! hls.audio";
        let hls = format!(
            " hlssink2 name=hls location=\"{seg}\" playlist-location=\"{pl}\" \
             target-duration=2 max-files=10 send-keyframe-requests=true",
            seg = seg_location.display(),
            pl = pl_location.display(),
        );
        (audio.to_string(), hls)
    } else if use_relay {
        // Audio with tee — one path to Strata mux, another to FLV mux
        let audio = " audiotestsrc is-live=true wave=silence \
            ! audioconvert ! audioresample ! voaacenc bitrate=128000 \
            ! tee name=atee \
            atee. ! queue ! aacparse ! mux. \
            atee. ! queue ! aacparse ! fmux.";
        // RTMP mux + sink — use codec-appropriate muxer (eflvmux for H.265, flvmux for H.264)
        let rtmp = format!(
            " {mux} ! rtmpsink location=\"{url}\" sync=false",
            mux = codec_ctrl.relay_muxer_fragment(),
            url = relay_url
        );
        (audio.to_string(), rtmp)
    } else if add_audio {
        (
            " audiotestsrc is-live=true wave=silence ! audioconvert ! audioresample ! voaacenc bitrate=128000 ! aacparse ! queue ! mux.".to_string(),
            String::new(),
        )
    } else {
        (String::new(), String::new())
    };

    // Video path: with relay, tee after encoder; without, straight to mux
    let parser = codec_type.parser_factory();
    let video_to_mux = if use_hls_relay {
        format!(
            "! tee name=vtee \
             vtee. ! queue ! mux. \
             vtee. ! queue ! {parser} ! hls.video"
        )
    } else if use_relay {
        format!(
            "! tee name=vtee \
             vtee. ! queue ! mux. \
             vtee. ! queue ! {parser} ! fmux."
        )
    } else {
        "! queue ! mux.".to_string()
    };

    let enc_fragment =
        codec_ctrl.pipeline_fragment("enc", bitrate_kbps, key_int, max_bitrate_kbps_val);

    let pipeline_str = format!(
        "videotestsrc name=testsrc is-live=true pattern=ball \
         ! video/x-raw,width={w},height={h},framerate={fps}/1 \
         ! queue name=testq max-size-buffers=3 ! sel. \
         input-selector name=sel \
         ! {enc_fragment} \
         {video_to_mux}{audio} \
         mpegtsmux name=mux alignment=7 pat-interval=10 pmt-interval=10 \
         ! stratasink name=rsink{relay}",
        w = res_w,
        h = res_h,
        fps = framerate,
        enc_fragment = enc_fragment,
        video_to_mux = video_to_mux,
        audio = audio_fragment,
        relay = relay_fragment,
    );

    eprintln!("Sender Pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "Failed to cast to pipeline")?;

    configure_mpegtsmux(&pipeline);

    // Start HLS segment uploader if in HLS relay mode
    let _hls_uploader = if use_hls_relay {
        let hls_dir = hls_tmp_dir.as_ref().unwrap().clone();
        let base_url = hls_upload::hls_base_url(relay_url).to_string();
        Some(hls_upload::start_hls_uploader(
            hls_upload::HlsUploaderConfig {
                segment_dir: hls_dir,
                base_url,
                playlist_filename: "playlist.m3u8".into(),
            },
        ))
    } else {
        None
    };

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
        if !config_path.is_empty() {
            let config_toml = std::fs::read_to_string(config_path)
                .map_err(|e| format!("Failed to read config file '{}': {}", config_path, e))?;
            sink.set_property("config", &config_toml);
            eprintln!("Applied config from {}", config_path);
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

            // Resolve which OS interface routes to this destination
            if let Some(iface) = resolve_interface_for_uri(uri) {
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
        strata_sink.set_adaptation_envelope(min_bitrate_kbps_val, max_bitrate_kbps_val);
        eprintln!(
            "Adaptation envelope: {}–{} kbps",
            min_bitrate_kbps_val, max_bitrate_kbps_val
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
                            // Limit step to +50% to avoid VBV shock
                            let max_step = current + current / 2;
                            let clamped = target_kbps.min(max_step).max(500);
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
                        }
                    } else if s.name() == "strata-stats"
                        && let Some(sock) = &stats_socket
                    {
                        let json = serialize_bonding_stats(s);
                        let _ = sock.send_to(json.as_bytes(), stats_dest);
                    }
                }
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;
    let _ = std::fs::remove_file(control_sock_path);

    // Clean up HLS temp directory
    if let Some(ref dir) = hls_tmp_dir {
        let _ = std::fs::remove_dir_all(dir);
    }

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
    _relay_url: &str,
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
         mpegtsmux name=mux alignment=7 pat-interval=10 pmt-interval=10 \
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
                    let json = serialize_bonding_stats(s);
                    let _ = sock.send_to(json.as_bytes(), addr);
                }
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

/// Convert a serde_json::Value to TOML string for stratasink's config property.
fn json_to_toml(value: &serde_json::Value) -> Result<String, String> {
    // serde_json::Value → toml::Value via intermediate serialization
    let toml_value: toml::Value =
        serde_json::from_value(value.clone()).map_err(|e| format!("json→toml conversion: {e}"))?;
    toml::to_string(&toml_value).map_err(|e| format!("toml serialization: {e}"))
}

fn resolve_interface_for_uri(uri: &str) -> Option<String> {
    // Strip strata:// or legacy rist:// prefix for backwards compat
    let stripped = uri
        .strip_prefix("strata://")
        .or_else(|| uri.strip_prefix("strata://@"))
        .or_else(|| uri.strip_prefix("rist://"))
        .or_else(|| uri.strip_prefix("rist://@"))
        .unwrap_or(uri);
    let host = stripped.split(':').next()?;
    if host.is_empty() {
        return None;
    }

    let output = std::process::Command::new("ip")
        .args(["route", "get", host])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse: "172.30.0.20 dev eth2 src 172.30.0.10 ..."
    for part in stdout.split_whitespace().collect::<Vec<_>>().windows(2) {
        if part[0] == "dev" {
            return Some(part[1].to_string());
        }
    }
    None
}

// ── Link toggling ───────────────────────────────────────────────────

/// Handle a `toggle-link` command from the control socket.
///
/// Finds the sink pad whose `interface` property matches the requested
/// interface name and either removes or re-adds the corresponding
/// link in the bonding runtime.
///
/// **Disable:** releases the pad from the element, which triggers
/// `release_pad` → `remove_link_by_pad_name` internally.
///
/// **Enable:** requests a new pad, copies the URI and interface from
/// the disabled pad info, and triggers `add_link_from_pad`.
///
/// We maintain a small map of disabled interfaces to their (URI, iface)
/// so they can be re-enabled.
fn handle_toggle_link(
    sink: &gst::Element,
    structure: &gst::StructureRef,
    disabled_links: &Mutex<std::collections::HashMap<String, (String, String)>>,
) {
    let iface = match structure.get::<&str>("interface") {
        Ok(s) => s.to_string(),
        Err(_) => {
            eprintln!("toggle-link: missing 'interface' field");
            return;
        }
    };
    let enabled = structure.get::<bool>("enabled").unwrap_or(true);

    if enabled {
        // Re-enable: retrieve stored URI and create a new pad
        let stored = disabled_links.lock().unwrap().remove(&iface);
        if let Some((uri, iface_name)) = stored {
            if let Some(pad) = sink.request_pad_simple("link_%u") {
                pad.set_property("uri", &uri);
                pad.set_property("interface", &iface_name);
                eprintln!(
                    "toggle-link: re-enabled {} → {} (pad {})",
                    iface_name,
                    uri,
                    pad.name()
                );
            } else {
                eprintln!("toggle-link: failed to request new pad for {}", iface);
                // Put it back so user can retry
                disabled_links
                    .lock()
                    .unwrap()
                    .insert(iface, (uri, iface_name));
            }
        } else {
            eprintln!("toggle-link: interface '{}' is not disabled", iface);
        }
    } else {
        // Disable: find the pad, store its info, then release it
        let mut target_pad: Option<gst::Pad> = None;
        for pad in sink.pads() {
            if pad.direction() != gst::PadDirection::Sink {
                continue;
            }
            let pad_iface: String = pad.property("interface");
            if pad_iface == iface {
                target_pad = Some(pad);
                break;
            }
        }

        if let Some(pad) = target_pad {
            let uri: String = pad.property("uri");
            let pad_iface: String = pad.property("interface");
            disabled_links
                .lock()
                .unwrap()
                .insert(iface.clone(), (uri, pad_iface));
            sink.release_request_pad(&pad);
            eprintln!("toggle-link: disabled link on {} (released pad)", iface);
        } else {
            eprintln!("toggle-link: no pad found for interface '{}'", iface);
        }
    }
}

// ── Stats serialization ─────────────────────────────────────────────

/// Serialize the `strata-stats` GStreamer structure into JSON
/// that the agent telemetry module can parse.
///
/// Includes ALL links (alive and dead) with full metadata so the
/// dashboard can show link state transitions and technology type.
fn serialize_bonding_stats(s: &gst::StructureRef) -> String {
    let alive_links = s.get::<u64>("alive_links").unwrap_or(0);
    let wall_time_ms = s.get::<u64>("wall_time_ms").unwrap_or(0);
    let mut links = Vec::new();

    // Probe link IDs — not necessarily 0..N contiguous
    let max_probe = alive_links.max(8) as u32;
    for id in 0..max_probe {
        let rtt_key = format!("link_{}_rtt", id);
        if let Ok(rtt_ms) = s.get::<f64>(&rtt_key) {
            let capacity = s
                .get::<f64>(&format!("link_{}_capacity", id))
                .unwrap_or(0.0);
            let loss = s.get::<f64>(&format!("link_{}_loss", id)).unwrap_or(0.0);
            let observed_bytes = s
                .get::<u64>(&format!("link_{}_observed_bytes", id))
                .unwrap_or(0);
            let observed_bps = s
                .get::<f64>(&format!("link_{}_observed_bps", id))
                .unwrap_or(0.0);
            let iface = s
                .get::<&str>(&format!("link_{}_iface", id))
                .unwrap_or("unknown");
            let alive = s
                .get::<bool>(&format!("link_{}_alive", id))
                .unwrap_or(false);
            let phase = s
                .get::<&str>(&format!("link_{}_phase", id))
                .unwrap_or("unknown");
            let os_up = s.get::<i32>(&format!("link_{}_os_up", id)).unwrap_or(-1);
            let kind = s.get::<&str>(&format!("link_{}_kind", id)).unwrap_or("");

            links.push(serde_json::json!({
                "id": id,
                "rtt_us": (rtt_ms * 1000.0) as u64,
                "loss_rate": loss,
                "capacity_bps": capacity.round() as u64,
                "sent_bytes": observed_bytes,
                "observed_bps": observed_bps.round() as u64,
                "interface": iface,
                "alive": alive,
                "phase": phase,
                "os_up": os_up,
                "link_kind": kind,
            }));
        }
    }

    serde_json::json!({
        "links": links,
        "timestamp_ms": wall_time_ms,
    })
    .to_string()
}

// ── Hot-swap helpers ────────────────────────────────────────────────

/// Add a new source branch to the pipeline and link it to input-selector.
/// Returns the new sink pad on the selector that this branch feeds into.
#[allow(clippy::too_many_arguments)]
fn add_source_branch(
    pipeline: &gst::Pipeline,
    selector: &gst::Element,
    mode: &str,
    device: &str,
    uri: &str,
    framerate: u32,
    width: u32,
    height: u32,
) -> Result<gst::Pad, Box<dyn std::error::Error>> {
    let caps = gst::Caps::builder("video/x-raw")
        .field("width", width as i32)
        .field("height", height as i32)
        .field("framerate", gst::Fraction::new(framerate as i32, 1))
        .build();

    let (elements, src_element_name): (Vec<gst::Element>, String) = match mode {
        "v4l2" => {
            let src = gst::ElementFactory::make("v4l2src")
                .property("device", device)
                .build()?;
            let conv = gst::ElementFactory::make("videoconvert").build()?;
            let scale = gst::ElementFactory::make("videoscale").build()?;
            let filter = gst::ElementFactory::make("capsfilter")
                .property("caps", &caps)
                .build()?;
            let queue = gst::ElementFactory::make("queue")
                .property("max-size-buffers", 3u32)
                .build()?;
            let name = src.name().to_string();
            (vec![src, conv, scale, filter, queue], name)
        }
        "uri" => {
            if uri.is_empty() {
                return Err("URI source requires a non-empty URI".into());
            }
            let src = gst::ElementFactory::make("uridecodebin")
                .property("uri", uri)
                .build()?;
            let conv = gst::ElementFactory::make("videoconvert").build()?;
            let scale = gst::ElementFactory::make("videoscale").build()?;
            let filter = gst::ElementFactory::make("capsfilter")
                .property("caps", &caps)
                .build()?;
            let queue = gst::ElementFactory::make("queue")
                .property("max-size-buffers", 3u32)
                .build()?;
            let name = src.name().to_string();
            // uridecodebin has dynamic pads — connect later via pad-added
            let conv_weak = conv.downgrade();
            src.connect_pad_added(move |_, pad| {
                if let Some(conv) = conv_weak.upgrade()
                    && let Some(sink_pad) = conv.static_pad("sink")
                    && !sink_pad.is_linked()
                {
                    let _ = pad.link(&sink_pad);
                }
            });
            // Link conv→scale→filter→queue (src→conv linked via pad-added)
            (vec![src, conv, scale, filter, queue], name)
        }
        other => {
            return Err(format!("Unknown source mode: {other}").into());
        }
    };

    // Add all elements to the pipeline
    for el in &elements {
        pipeline.add(el)?;
    }

    // Link chain: elements[0] → elements[1] → ... → elements[N-1]
    // For v4l2: src → conv → scale → filter → queue
    // For uri: conv → scale → filter → queue (src→conv via pad-added)
    if mode == "v4l2" {
        gst::Element::link_many(&elements)?;
    } else {
        // Skip the first element (uridecodebin) — linked via pad-added
        gst::Element::link_many(&elements[1..])?;
    }

    // Request a new pad on input-selector and link
    let sel_pad = selector
        .request_pad_simple("sink_%u")
        .ok_or("Failed to request selector pad")?;
    let last = elements.last().unwrap();
    let src_pad = last.static_pad("src").ok_or("No src pad on last element")?;
    src_pad.link(&sel_pad)?;

    // Sync states with parent
    for el in &elements {
        el.sync_state_with_parent()?;
    }

    eprintln!(
        "Added {} source branch ({}) → selector pad {}",
        mode,
        src_element_name,
        sel_pad.name()
    );
    Ok(sel_pad)
}

/// Handle a source-switch Application message from the control socket.
fn handle_source_switch(
    pipeline: &gst::Pipeline,
    selector: &gst::Element,
    test_pad: &gst::Pad,
    structure: &gst::StructureRef,
    framerate: u32,
    res_w: u32,
    res_h: u32,
) {
    let mode = structure.get::<&str>("mode").unwrap_or("test");

    match mode {
        "test" => {
            // Switch back to test source and optionally change pattern
            selector.set_property("active-pad", test_pad);
            if let Ok(pattern) = structure.get::<&str>("pattern") {
                if let Some(testsrc) = pipeline.by_name("testsrc") {
                    // GStreamer's videotestsrc `pattern` property is a GstVideoTestSrcPattern
                    // enum.  set_property_from_str lets GStreamer do the string→enum conversion
                    // so we don't need to match on integer values (which panics with newer bindings).
                    let gst_name = match pattern {
                        "black" | "solid-color" => "black",
                        "bar" | "colors" => "bar",
                        other => other, // smpte, snow, ball, smpte100, etc. match GStreamer names
                    };
                    testsrc.set_property_from_str("pattern", gst_name);
                    eprintln!("Switched to test source (pattern={})", pattern);
                }
            } else {
                eprintln!("Switched to test source");
            }
        }
        "v4l2" => {
            let device = structure.get::<&str>("device").unwrap_or("/dev/video0");
            match add_source_branch(
                pipeline, selector, "v4l2", device, "", framerate, res_w, res_h,
            ) {
                Ok(new_pad) => {
                    selector.set_property("active-pad", &new_pad);
                    eprintln!("Switched to v4l2 source (device={})", device);
                }
                Err(e) => {
                    eprintln!("Failed to switch to v4l2: {}", e);
                }
            }
        }
        "uri" => {
            let uri = structure.get::<&str>("uri").unwrap_or("");
            if uri.is_empty() {
                eprintln!("Source switch to URI requires 'uri' field");
                return;
            }
            match add_source_branch(pipeline, selector, "uri", "", uri, framerate, res_w, res_h) {
                Ok(new_pad) => {
                    selector.set_property("active-pad", &new_pad);
                    eprintln!("Switched to URI source (uri={})", uri);
                }
                Err(e) => {
                    eprintln!("Failed to switch to URI source: {}", e);
                }
            }
        }
        other => {
            eprintln!("Unknown source mode in switch command: {other}");
        }
    }
}

/// Run the Unix domain socket listener for hot-swap control commands.
/// Posts `source-switch` Application messages to the pipeline's bus.
fn run_control_socket(path: &str, pipeline_weak: gst::glib::WeakRef<gst::Pipeline>) {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    // Remove stale socket file from previous run (crash recovery)
    let _ = std::fs::remove_file(path);

    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind control socket at {}: {}", path, e);
            return;
        }
    };
    eprintln!("Control socket listening on {}", path);

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Control socket accept error: {}", e);
                continue;
            }
        };

        let pipeline = match pipeline_weak.upgrade() {
            Some(p) => p,
            None => return, // Pipeline gone, exit thread
        };

        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }

            // Parse JSON command
            let cmd: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Control: invalid JSON: {} — {}", line, e);
                    continue;
                }
            };

            if cmd.get("cmd").and_then(|v| v.as_str()) == Some("switch_source") {
                let mut builder = gst::Structure::builder("source-switch");
                if let Some(mode) = cmd.get("mode").and_then(|v| v.as_str()) {
                    builder = builder.field("mode", mode);
                }
                if let Some(pattern) = cmd.get("pattern").and_then(|v| v.as_str()) {
                    builder = builder.field("pattern", pattern);
                }
                if let Some(device) = cmd.get("device").and_then(|v| v.as_str()) {
                    builder = builder.field("device", device);
                }
                if let Some(uri) = cmd.get("uri").and_then(|v| v.as_str()) {
                    builder = builder.field("uri", uri);
                }

                let structure = builder.build();
                let msg = gst::message::Application::new(structure);
                let _ = pipeline.post_message(msg);
                eprintln!("Control: queued source-switch command");
            } else if cmd.get("cmd").and_then(|v| v.as_str()) == Some("toggle_link") {
                // Enable/disable a bonding link by OS interface name.
                // Posts a "toggle-link" Application message processed in the bus loop.
                let iface = cmd.get("interface").and_then(|v| v.as_str()).unwrap_or("");
                let enabled = cmd.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                if iface.is_empty() {
                    eprintln!("Control: toggle_link missing 'interface'");
                } else {
                    let structure = gst::Structure::builder("toggle-link")
                        .field("interface", iface)
                        .field("enabled", enabled)
                        .build();
                    let msg = gst::message::Application::new(structure);
                    let _ = pipeline.post_message(msg);
                    eprintln!(
                        "Control: queued toggle-link iface={} enabled={}",
                        iface, enabled
                    );
                }
            } else if cmd.get("cmd").and_then(|v| v.as_str()) == Some("set_encoder") {
                // Hot-update encoder properties (bitrate, tune, keyint)
                if let Some(enc) = pipeline.by_name("enc") {
                    if let Some(bps) = cmd.get("bitrate_kbps").and_then(|v| v.as_u64()) {
                        let bps = bps as u32;
                        enc.set_property("bitrate", bps);
                        eprintln!("Control: set encoder bitrate to {} kbps", bps);
                    }
                    if let Some(tune) = cmd.get("tune").and_then(|v| v.as_str()) {
                        enc.set_property_from_str("tune", tune);
                        eprintln!("Control: set encoder tune to {}", tune);
                    }
                    if let Some(ki) = cmd.get("keyint_max").and_then(|v| v.as_u64()) {
                        enc.set_property("key-int-max", ki as u32);
                        eprintln!("Control: set encoder keyint-max to {}", ki);
                    }
                } else {
                    eprintln!("Control: set_encoder — encoder element 'enc' not found");
                }
            } else if cmd.get("cmd").and_then(|v| v.as_str()) == Some("set_bonding_config") {
                // Hot-update bonding/scheduler config via stratasink's "config" property
                if let Some(config_val) = cmd.get("config") {
                    if let Some(sink) = pipeline.by_name("rsink") {
                        // Convert JSON config to TOML (stratasink expects TOML)
                        match json_to_toml(config_val) {
                            Ok(toml_str) => {
                                sink.set_property("config", &toml_str);
                                eprintln!("Control: applied bonding config update");
                            }
                            Err(e) => {
                                eprintln!(
                                    "Control: set_bonding_config — failed to convert config: {}",
                                    e
                                );
                            }
                        }
                    } else {
                        eprintln!("Control: set_bonding_config — sink element 'rsink' not found");
                    }
                }
            } else {
                eprintln!("Control: unknown command: {}", line);
            }
        }
    }
}

fn run_receiver(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut bind_str = "";
    let mut output_file = "";
    let mut config_path = "";
    let mut relay_url = "";
    let mut codec_str = "h265";
    let mut metrics_port: Option<u16> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                eprint!("{RECEIVER_HELP}");
                return Ok(());
            }
            "--bind" if i + 1 < args.len() => {
                bind_str = &args[i + 1];
                i += 1;
            }
            "--output" if i + 1 < args.len() => {
                output_file = &args[i + 1];
                i += 1;
            }
            "--config" if i + 1 < args.len() => {
                config_path = &args[i + 1];
                i += 1;
            }
            "--relay-url" if i + 1 < args.len() => {
                relay_url = &args[i + 1];
                i += 1;
            }
            "--codec" if i + 1 < args.len() => {
                codec_str = &args[i + 1];
                i += 1;
            }
            "--metrics-port" if i + 1 < args.len() => {
                metrics_port = Some(args[i + 1].parse().unwrap_or_else(|_| {
                    eprintln!("Invalid metrics port: {}", args[i + 1]);
                    std::process::exit(1);
                }));
                i += 1;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                eprint!("{RECEIVER_HELP}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    if bind_str.is_empty() {
        eprintln!("Missing --bind <url>");
        eprint!("{RECEIVER_HELP}");
        std::process::exit(1);
    }

    register_plugins()?;

    let codec_type = gststrata::codec::CodecType::from_str_loose(codec_str)
        .unwrap_or(gststrata::codec::CodecType::H264);
    let video_parser = codec_type.parser_factory();

    // Pipeline construction:
    //
    // --relay-url (RTMP relay):
    //   stratasrc ! tsdemux ! {parser} ! flvmux streamable=true ! rtmpsink
    //   (audio pads are connected dynamically via pad-added signal)
    //
    // --output (recording):
    //   stratasrc ! tee ! appsink + filesink (or tsdemux ! mp4mux ! filesink)
    //
    // Default (monitor only):
    //   stratasrc ! appsink

    let use_relay = !relay_url.is_empty();
    let use_hls_relay = use_relay && hls_upload::is_hls_url(relay_url);

    // For HLS receiver relay, create a temp directory for segment files.
    // Prefer /dev/shm (RAM-backed tmpfs) to avoid flash/eMMC wear on SBCs.
    let hls_tmp_dir = if use_hls_relay {
        let dir = hls_upload::tmpfs_segment_dir(&format!("strata-hls-rx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("failed to create HLS temp dir");
        eprintln!(
            "HLS temp dir: {} (tmpfs={})",
            dir.display(),
            dir.starts_with("/dev/shm")
        );
        Some(dir)
    } else {
        None
    };

    let pipeline_str = if use_hls_relay {
        let hls_dir = hls_tmp_dir.as_ref().unwrap();
        let seg_location = hls_dir.join("segment%05d.ts");
        let pl_location = hls_dir.join("playlist.m3u8");
        format!(
            "stratasrc links=\"{bind}\" name=src latency=200 ! \
             queue max-size-buffers=0 max-size-bytes=0 max-size-time=5000000000 ! \
             tsdemux name=d \
             d. ! queue ! {parser} ! hls.video \
             d. ! queue ! aacparse ! hls.audio \
             hlssink2 name=hls location=\"{seg}\" playlist-location=\"{pl}\" \
             target-duration=2 max-files=10 send-keyframe-requests=true",
            bind = bind_str,
            parser = video_parser,
            seg = seg_location.display(),
            pl = pl_location.display(),
        )
    } else if use_relay {
        let relay_frag = gststrata::codec::CodecController::new(codec_type).relay_muxer_fragment();
        format!(
            "stratasrc links=\"{bind}\" name=src latency=200 ! \
             queue max-size-buffers=0 max-size-bytes=0 max-size-time=5000000000 ! \
             tsdemux name=d \
             d. ! queue ! {parser} ! {relay} \
             rtmpsink location=\"{url}\" sync=false \
             d. ! queue ! aacparse ! fmux.",
            bind = bind_str,
            parser = video_parser,
            relay = relay_frag,
            url = relay_url
        )
    } else if !output_file.is_empty() {
        if output_file.ends_with(".ts") {
            // Raw dump
            format!(
                "stratasrc links=\"{}\" name=src ! tee name=t ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=0 ! appsink name=sink emit-signals=true sync=false t. ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=0 ! filesink location=\"{}\" sync=false",
                bind_str, output_file
            )
        } else {
            // Remux to encoded container: Demux -> Parse -> MP4 Mux -> File
            format!(
                "stratasrc links=\"{}\" name=src ! tee name=t ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=0 ! appsink name=sink emit-signals=true sync=false t. ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=0 ! tsdemux ! {} ! mp4mux faststart=true ! filesink location=\"{}\" sync=false",
                bind_str, video_parser, output_file
            )
        }
    } else {
        format!(
            "stratasrc links=\"{}\" ! appsink name=sink emit-signals=true sync=false",
            bind_str
        )
    };

    eprintln!("Receiver Pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "Failed to cast to pipeline")?;

    // Apply TOML config file if provided
    if !config_path.is_empty()
        && let Some(src_elem) = pipeline.by_name("src")
    {
        let config_toml = std::fs::read_to_string(config_path)
            .map_err(|e| format!("Failed to read config file '{}': {}", config_path, e))?;
        src_elem.set_property("config", &config_toml);
        eprintln!("Applied config from {}", config_path);
    }

    // Setup AppSink (only used for non-relay monitor/record modes)
    let received_count = Arc::new(Mutex::new(0u64));
    let received_bytes = Arc::new(Mutex::new(0u64));

    if !use_relay {
        let sink = pipeline.by_name("sink").expect("Sink not found");
        let appsink = sink
            .dynamic_cast::<gst_app::AppSink>()
            .expect("Sink is not appsink");

        let count_clone = received_count.clone();
        let bytes_clone = received_bytes.clone();

        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                    let mut c = count_clone.lock().unwrap();
                    *c += 1;

                    let mut b = bytes_clone.lock().unwrap();
                    *b += map.size() as u64;

                    eprintln!("AppSink: Received buffer #{} ({} bytes)", *c, map.size());

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    // Setup graceful shutdown handler
    let pipeline_weak = pipeline.downgrade();
    ctrlc::set_handler(move || {
        eprintln!("Received shutdown signal. Sending EOS to pipeline...");
        if let Some(pipeline) = pipeline_weak.upgrade() {
            let _ = pipeline.send_event(gst::event::Eos::new());
        }
    })
    .expect("Error setting signal handler");

    pipeline.set_state(gst::State::Playing)?;

    // Start HLS segment uploader if in HLS relay mode (receiver)
    let _hls_uploader = if use_hls_relay {
        let hls_dir = hls_tmp_dir.as_ref().unwrap().clone();
        let base_url = hls_upload::hls_base_url(relay_url).to_string();
        Some(hls_upload::start_hls_uploader(
            hls_upload::HlsUploaderConfig {
                segment_dir: hls_dir,
                base_url,
                playlist_filename: "playlist.m3u8".into(),
            },
        ))
    } else {
        None
    };

    // ── Prometheus metrics server (receiver) ──
    let _metrics_server = if let Some(port) = metrics_port {
        let src_element = pipeline.by_name("src");
        let stats_handle = src_element.as_ref().and_then(|el| {
            el.downcast_ref::<gststrata::src::StrataSrc>()
                .and_then(|src| src.stats_handle())
        });
        match stats_handle {
            Some(handle) => {
                let addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
                match strata_bonding::metrics::ReceiverMetricsServer::start(addr, handle) {
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
                eprintln!("Warning: receiver stats handle not available");
                None
            }
        }
    } else {
        None
    };

    let bus = pipeline.bus().unwrap();

    if use_relay {
        eprintln!(
            "Receiver running — relaying to {}... Press Ctrl+C to stop.",
            relay_url
        );
    } else {
        eprintln!("Receiver running... Waiting for signal or EOS.");
    }

    // Standard GStreamer message loop
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
            MessageView::Element(element) => {
                if let Some(s) = element.structure() {
                    // Filter spammy stats if needed, or keep for visualization
                    if s.name() == "strata-stats" {
                        eprintln!("Element Message: {}", s);
                    }
                }
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;

    // Clean up HLS temp directory
    if let Some(ref dir) = hls_tmp_dir {
        let _ = std::fs::remove_dir_all(dir);
    }

    eprintln!(
        "Receiver Final Stats: Count={}, Bytes={}",
        *received_count.lock().unwrap(),
        *received_bytes.lock().unwrap()
    );

    Ok(())
}
