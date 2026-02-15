use gst::prelude::*;
use gst::MessageView;
use std::env;
use std::sync::{Arc, Mutex};

const SENDER_HELP: &str = r#"
USAGE: integration_node sender [OPTIONS] --dest <URL[,URL...]>

OPTIONS:
  --dest <urls>       Comma-separated RIST destination URLs (required)
                      e.g. rist://192.168.1.100:5000,rist://10.0.0.100:5000
  --source <mode>     Video source mode (default: test)
                        test   - SMPTE colour bars (videotestsrc)
                        v4l2   - Camera / HDMI capture card (v4l2src)
                        uri    - File or network stream (uridecodebin)
  --device <path>     V4L2 device path (default: /dev/video0, used with --source v4l2)
  --uri <uri>         Media URI for uridecodebin (used with --source uri)
                      e.g. file:///home/user/video.mp4
  --bitrate <kbps>    Target encoder bitrate in kbps (default: 3000)
  --framerate <fps>   Video framerate (default: 30)
  --audio             Add silent AAC audio track (required for RTMP targets)
  --config <path>     Path to TOML config file (see Configuration Reference)
  --stats-dest <addr> UDP address to relay stats JSON (e.g. 192.168.1.50:9000)
  --help              Show this help

EXAMPLES:
  # Test pattern over two cellular links
  integration_node sender --dest rist://server:5000,rist://server:5002 \
    --config sender.toml

  # 1080p60 with audio for YouTube relay via receiver
  integration_node sender --source test --framerate 60 --audio --bitrate 6000 \
    --dest rist://receiver:5000,rist://receiver:5002,rist://receiver:5004

  # HDMI capture card to cloud receiver
  integration_node sender --source v4l2 --device /dev/video0 \
    --dest rist://cloud.example.com:5000,rist://cloud.example.com:5002 \
    --bitrate 5000 --config sender.toml

  # Pre-recorded file (for testing without hardware)
  integration_node sender --source uri --uri file:///tmp/test.mp4 \
    --dest rist://192.168.1.100:5000
"#;

const RECEIVER_HELP: &str = r#"
USAGE: integration_node receiver [OPTIONS] --bind <URL>

OPTIONS:
  --bind <url>        RIST bind URL (required), e.g. rist://@0.0.0.0:5000
                      Multiple bind addresses: rist://@0.0.0.0:5000,rist://@0.0.0.0:5002
  --output <path>     Record to file (.ts = raw MPEG-TS, .mp4 = remuxed)
  --relay-url <url>   RTMP/RTMPS URL to relay the received stream to
                      e.g. rtmp://a.rtmp.youtube.com/live2/STREAM_KEY
  --config <path>     Path to TOML config file (see Configuration Reference)
  --help              Show this help

EXAMPLES:
  # Receive and monitor (no file output)
  integration_node receiver --bind rist://@0.0.0.0:5000

  # Receive bonded stream and relay to YouTube
  integration_node receiver --bind rist://@0.0.0.0:5000,rist://@0.0.0.0:5002,rist://@0.0.0.0:5004 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"

  # Receive and record to MPEG-TS file
  integration_node receiver --bind rist://@0.0.0.0:5000 --output capture.ts

  # Receive with config
  integration_node receiver --bind rist://@0.0.0.0:5000 --config receiver.toml
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging for production use.
    // Controlled by RUST_LOG env var (e.g., RUST_LOG=info,librist=warn).
    rist_bonding_core::init();

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
            eprintln!("  sender    Encode and transmit video over bonded RIST links");
            eprintln!("  receiver  Receive and reassemble bonded RIST stream\n");
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
    gstristbonding::sink::register(None)?;
    gstristbonding::src::register(None)?;
    Ok(())
}

fn run_sender(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut dest_str = "";
    let mut stats_dest = "";
    let mut bitrate_kbps = 3000u32;
    let mut framerate = 30u32;
    let mut add_audio = false;
    let mut config_path = "";
    let mut source_mode = "test";
    let mut device_path = "/dev/video0";
    let mut source_uri = "";

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
                bitrate_kbps = args[i + 1].parse().unwrap_or(3000);
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

    register_plugins()?;

    // Build the source portion of the pipeline based on the selected mode.
    // All modes produce encoded H.264 MPEG-TS output for rsristbondsink.
    //
    // VBV buffer = bitrate (1-second buffer) constrains the peak rate,
    // preventing unconstrained VBR overshoot with tune=zerolatency.
    //
    // key-int-max is set to 2× framerate (keyframe every 2 seconds).
    let key_int = framerate * 2;
    let source_fragment = match source_mode {
        "test" => {
            // Indefinite SMPTE test pattern — runs until Ctrl+C.
            format!(
                "videotestsrc is-live=true pattern=smpte ! video/x-raw,width=1920,height=1080,framerate={fps}/1 ! x264enc name=enc tune=zerolatency bitrate={bps} vbv-buf-capacity={bps} key-int-max={ki}",
                fps = framerate,
                bps = bitrate_kbps,
                ki = key_int
            )
        }
        "v4l2" => {
            // V4L2 camera or HDMI capture card.
            // Most capture cards output raw video or MJPEG; we re-encode to H.264.
            // For cards that output H.264 natively, use --source uri with a custom pipeline.
            format!(
                "v4l2src device={dev} ! videoconvert ! videoscale ! video/x-raw,width=1920,height=1080,framerate={fps}/1 ! x264enc name=enc tune=zerolatency bitrate={bps} vbv-buf-capacity={bps} key-int-max={ki}",
                dev = device_path,
                fps = framerate,
                bps = bitrate_kbps,
                ki = key_int
            )
        }
        "uri" => {
            // File or network stream via uridecodebin.  Useful for testing with
            // pre-recorded content without camera hardware.
            if source_uri.is_empty() {
                eprintln!("--source uri requires --uri <media-uri>");
                std::process::exit(1);
            }
            format!(
                "uridecodebin uri={uri} ! videoconvert ! videoscale ! video/x-raw,width=1920,height=1080,framerate={fps}/1 ! x264enc name=enc tune=zerolatency bitrate={bps} vbv-buf-capacity={bps} key-int-max={ki}",
                uri = source_uri,
                fps = framerate,
                bps = bitrate_kbps,
                ki = key_int
            )
        }
        other => {
            eprintln!("Unknown source mode: {other}  (valid: test, v4l2, uri)");
            std::process::exit(1);
        }
    };

    // Build the full pipeline string.
    // When --audio is specified, add a silent AAC audio track — required
    // for RTMP targets like YouTube which reject video-only streams.
    let pipeline_str = if add_audio {
        format!(
            "{source_fragment} ! queue ! mux. \
             audiotestsrc is-live=true wave=silence ! audio/x-raw,rate=44100,channels=2 ! voaacenc bitrate=128000 ! queue ! mux. \
             mpegtsmux name=mux alignment=7 pat-interval=10 pmt-interval=10 ! rsristbondsink name=rsink"
        )
    } else {
        format!(
            "{source_fragment} ! mpegtsmux alignment=7 pat-interval=10 pmt-interval=10 ! rsristbondsink name=rsink"
        )
    };
    eprintln!("Sender Pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "Failed to cast to pipeline")?;

    if let Some(sink) = pipeline.by_name("rsink") {
        // Apply TOML config file if provided
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
            eprintln!("Configured link {} -> {}", idx, uri);
        }
    } else {
        return Err("Failed to find rsristbondsink element".into());
    }

    // Setup Stats Relay if requested
    let mut stats_socket = None;
    if !stats_dest.is_empty() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
        stats_socket = Some(sock);
    }

    // Track the current NADA-derived ceiling (updated continuously via
    // bandwidth-available messages).
    let nada_ceiling_kbps = Arc::new(Mutex::new(bitrate_kbps));

    // Graceful shutdown: send EOS on Ctrl+C so the pipeline flushes cleanly.
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
            MessageView::Element(element) => {
                if let Some(s) = element.structure() {
                    if s.name() == "congestion-control" {
                        if let Ok(recommended) = s.get::<u64>("recommended-bitrate") {
                            // RFC 8698 §5.2.2: Set encoder bitrate to r_vin
                            // (NADA-derived target).  Clamp to [500, configured_max].
                            if let Some(enc) = pipeline.by_name("enc") {
                                let recommended_kbps = (recommended / 1000) as u32;
                                let target = recommended_kbps.clamp(500, bitrate_kbps);
                                let current: u32 = enc.property("bitrate");
                                if target != current {
                                    eprintln!(
                                        "NADA Rate Signal: Setting bitrate {} -> {} kbps (r_vin={})",
                                        current, target, recommended
                                    );
                                    enc.set_property("bitrate", target);
                                }
                            }
                        }
                    } else if s.name() == "bandwidth-available" {
                        // Update NADA ceiling — the maximum safe rate
                        // according to the aggregate estimate.
                        if let Ok(max_bps) = s.get::<u64>("max-bitrate") {
                            let ceiling = ((max_bps / 1000) as u32).min(bitrate_kbps);
                            *nada_ceiling_kbps.lock().unwrap() = ceiling;
                        }
                    } else if s.name() == "rist-bonding-stats" {
                        // Relay to UDP if configured
                        if let Some(sock) = &stats_socket {
                            if let Ok(stats_json) = s.get::<&str>("stats_json") {
                                let _ = sock.send_to(stats_json.as_bytes(), stats_dest);
                            }
                        }
                    }
                }
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

fn run_receiver(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut bind_str = "";
    let mut output_file = "";
    let mut config_path = "";
    let mut relay_url = "";
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

    // Pipeline construction:
    //
    // --relay-url (RTMP relay):
    //   rsristbondsrc ! tsdemux ! h264parse ! flvmux streamable=true ! rtmpsink
    //   (audio pads are connected dynamically via pad-added signal)
    //
    // --output (recording):
    //   rsristbondsrc ! tee ! appsink + filesink (or tsdemux ! mp4mux ! filesink)
    //
    // Default (monitor only):
    //   rsristbondsrc ! appsink

    let use_relay = !relay_url.is_empty();

    let pipeline_str = if use_relay {
        // RTMP relay pipeline — we use tsdemux with dynamic pads.
        // The gst_parse syntax `d. !` auto-links pads by type.
        // This handles both video-only and video+audio MPEG-TS inputs.
        format!(
            "rsristbondsrc links=\"{bind}\" name=src latency=200 ! \
             queue max-size-buffers=0 max-size-bytes=0 max-size-time=5000000000 ! \
             tsdemux name=d \
             d. ! queue ! h264parse ! flvmux name=mux streamable=true ! \
             rtmpsink location=\"{url}\" sync=false \
             d. ! queue ! aacparse ! mux.",
            bind = bind_str,
            url = relay_url
        )
    } else if !output_file.is_empty() {
        if output_file.ends_with(".ts") {
            // Raw dump
            format!(
                "rsristbondsrc links=\"{}\" name=src ! tee name=t ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=0 ! appsink name=sink emit-signals=true sync=false t. ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=0 ! filesink location=\"{}\" sync=false",
                bind_str, output_file
            )
        } else {
            // Remux to encoded container: Encoded H264 TS -> Demux -> Parse -> MP4 Mux -> File
            // Use faststart=true to move MOOV atom to front (requires rewriting file at end).
            // Use queues to prevent blocking.
            format!(
                "rsristbondsrc links=\"{}\" name=src ! tee name=t ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=0 ! appsink name=sink emit-signals=true sync=false t. ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=0 ! tsdemux ! h264parse ! mp4mux faststart=true ! filesink location=\"{}\" sync=false",
                bind_str, output_file
            )
        }
    } else {
        format!(
            "rsristbondsrc links=\"{}\" ! appsink name=sink emit-signals=true sync=false",
            bind_str
        )
    };

    eprintln!("Receiver Pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "Failed to cast to pipeline")?;

    // Apply TOML config file if provided
    if !config_path.is_empty() {
        if let Some(src_elem) = pipeline.by_name("src") {
            let config_toml = std::fs::read_to_string(config_path)
                .map_err(|e| format!("Failed to read config file '{}': {}", config_path, e))?;
            src_elem.set_property("config", &config_toml);
            eprintln!("Applied config from {}", config_path);
        }
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
                    if s.name() == "rist-bonding-stats" {
                        eprintln!("Element Message: {}", s);
                    }
                }
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;

    eprintln!(
        "Receiver Final Stats: Count={}, Bytes={}",
        *received_count.lock().unwrap(),
        *received_bytes.lock().unwrap()
    );

    Ok(())
}
