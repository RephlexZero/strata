use gst::prelude::*;
use gst::MessageView;
use std::env;
use std::sync::{Arc, Mutex};

const SENDER_HELP: &str = r#"
USAGE: integration_node sender [OPTIONS] --dest <URL[,URL...]>

OPTIONS:
  --dest <urls>       Comma-separated RIST destination URLs (required)
                      e.g. rist://192.168.1.100:5000,rist://10.0.0.100:5000
  --source <mode>     Initial video source mode (default: test)
                        test   - SMPTE colour bars (videotestsrc)
                        v4l2   - Camera / HDMI capture card (v4l2src)
                        uri    - File or network stream (uridecodebin)
  --device <path>     V4L2 device path (default: /dev/video0, used with --source v4l2)
  --uri <uri>         Media URI for uridecodebin (used with --source uri)
                      e.g. file:///home/user/video.mp4
  --bitrate <kbps>    Target encoder bitrate in kbps (default: 2000)
  --framerate <fps>   Video framerate (default: 30)
  --audio             Add silent AAC audio track (required for RTMP targets)
  --config <path>     Path to TOML config file (see Configuration Reference)
  --stats-dest <addr> UDP address to relay stats JSON (e.g. 127.0.0.1:9100)
  --relay-url <url>   RTMP/RTMPS URL to relay encoded stream to in parallel
                      (tees output: one copy goes via RIST, another via RTMP)
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
  integration_node sender --dest rist://server:5000,rist://server:5002 \
    --config sender.toml

  # 1080p30 with audio for YouTube relay via receiver
  integration_node sender --source test --framerate 30 --audio --bitrate 2000 \
    --dest rist://receiver:5000,rist://receiver:5002,rist://receiver:5004

  # Direct RTMP relay to YouTube (sender tees output: RIST + RTMP)
  integration_node sender --source test --bitrate 2000 \
    --dest rist://receiver:5000,rist://receiver:5002 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"

  # HDMI capture card to cloud receiver
  integration_node sender --source v4l2 --device /dev/video0 \
    --dest rist://cloud.example.com:5000,rist://cloud.example.com:5002 \
    --bitrate 2000 --config sender.toml
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

    // ── Build pipeline with input-selector for hot-swap support ──
    //
    // Pipeline structure:
    //   videotestsrc ! capsfilter ! queue ─┐
    //                                      ├─ input-selector ! x264enc ! [audio] ! mux ! rsristbondsink
    //   [dynamic v4l2/uri sources] ────────┘
    //
    // The initial source (--source flag) determines which branch is active.
    // Additional branches are added dynamically via the control socket.
    let key_int = framerate * 2;

    // Build the pipeline with input-selector.
    // The test source is always available as the fallback.
    //
    // When --relay-url is set, the encoded video and audio are teed:
    //   x264enc → tee → queue → mpegtsmux → rsristbondsink   (RIST)
    //                 └→ queue → h264parse → flvmux → rtmpsink (RTMP)
    //   voaacenc → tee → aacparse → queue → mpegtsmux         (RIST)
    //                 └→ queue → aacparse → flvmux             (RTMP)
    let use_relay = !relay_url.is_empty();

    // When relay is enabled, force audio on (RTMP requires it)
    if use_relay {
        add_audio = true;
    }

    let (audio_fragment, rtmp_fragment) = if use_relay {
        // Audio with tee — one path to RIST mux, another to FLV mux
        let audio = " audiotestsrc is-live=true wave=silence \
            ! audioconvert ! audioresample ! voaacenc bitrate=128000 \
            ! tee name=atee \
            atee. ! queue ! aacparse ! mux. \
            atee. ! queue ! aacparse ! fmux.";
        // RTMP mux + sink
        let rtmp = format!(
            " flvmux name=fmux streamable=true ! rtmpsink location=\"{url}\" sync=false",
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
    let video_to_mux = if use_relay {
        "! tee name=vtee \
         vtee. ! queue ! mux. \
         vtee. ! queue ! h264parse ! fmux."
    } else {
        "! queue ! mux."
    };

    let pipeline_str = format!(
        "videotestsrc name=testsrc is-live=true pattern=smpte \
         ! video/x-raw,width={w},height={h},framerate={fps}/1 \
         ! queue name=testq max-size-buffers=3 ! sel. \
         input-selector name=sel \
         ! x264enc name=enc tune=zerolatency bitrate={bps} vbv-buf-capacity={bps} key-int-max={ki} \
         {video_to_mux}{audio} \
         mpegtsmux name=mux alignment=7 pat-interval=10 pmt-interval=10 \
         ! rsristbondsink name=rsink{rtmp}",
        w = res_w,
        h = res_h,
        fps = framerate,
        bps = bitrate_kbps,
        ki = key_int,
        video_to_mux = video_to_mux,
        audio = audio_fragment,
        rtmp = rtmp_fragment,
    );

    eprintln!("Sender Pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "Failed to cast to pipeline")?;

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

    // ── Configure RIST destinations ──
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
        return Err("Failed to find rsristbondsink element".into());
    }

    // ── Stats relay (always if stats-dest is configured) ──
    let mut stats_socket = None;
    if !stats_dest.is_empty() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
        stats_socket = Some(sock);
        eprintln!("Stats relay → {}", stats_dest);
    }

    // ── NADA ceiling tracking ──
    let nada_ceiling_kbps = Arc::new(Mutex::new(bitrate_kbps));

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
                    } else if s.name() == "toggle-link" {
                        if let Some(sink) = pipeline.by_name("rsink") {
                            handle_toggle_link(&sink, s, &disabled_links);
                        }
                    }
                }
            }
            MessageView::Element(element) => {
                if let Some(s) = element.structure() {
                    if s.name() == "congestion-control" {
                        if let Ok(recommended) = s.get::<u64>("recommended-bitrate") {
                            if let Some(enc) = pipeline.by_name("enc") {
                                let recommended_kbps = (recommended / 1000) as u32;
                                let target = recommended_kbps.clamp(500, bitrate_kbps);
                                let current: u32 = enc.property("bitrate");
                                if target != current {
                                    eprintln!(
                                        "NADA Rate Signal: {} -> {} kbps (r_vin={})",
                                        current, target, recommended
                                    );
                                    enc.set_property("bitrate", target);
                                }
                            }
                        }
                    } else if s.name() == "bandwidth-available" {
                        if let Ok(max_bps) = s.get::<u64>("max-bitrate") {
                            let ceiling = ((max_bps / 1000) as u32).min(bitrate_kbps);
                            *nada_ceiling_kbps.lock().unwrap() = ceiling;
                        }
                    } else if s.name() == "rist-bonding-stats" {
                        if let Some(sock) = &stats_socket {
                            // Serialize the per-link fields into a JSON
                            // object that the agent telemetry can parse.
                            let json = serialize_bonding_stats(s);
                            let _ = sock.send_to(json.as_bytes(), stats_dest);
                        }
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

/// Resolve which OS network interface routes to the host in a RIST URI.
///
/// Parses the host from `rist://<host>:<port>?...` and runs
/// `ip route get <host>` to determine the outgoing interface.
fn resolve_interface_for_uri(uri: &str) -> Option<String> {
    // Extract host from rist://HOST:PORT?...
    let stripped = uri.strip_prefix("rist://")?;
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
/// interface name and either removes or re-adds the corresponding RIST
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

/// Serialize the `rist-bonding-stats` GStreamer structure into JSON
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
            let os_up = s
                .get::<i32>(&format!("link_{}_os_up", id))
                .unwrap_or(-1);
            let kind = s
                .get::<&str>(&format!("link_{}_kind", id))
                .unwrap_or("");

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
                if let Some(conv) = conv_weak.upgrade() {
                    if let Some(sink_pad) = conv.static_pad("sink") {
                        if !sink_pad.is_linked() {
                            let _ = pad.link(&sink_pad);
                        }
                    }
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
                    let pattern_enum = match pattern {
                        "smpte" => 0i32,
                        "snow" => 1,
                        "black" | "solid-color" => 2,
                        "ball" => 18,
                        "smpte100" => 13,
                        "bar" | "colors" => 14,
                        _ => {
                            eprintln!("Unknown test pattern: {pattern}, using smpte");
                            0
                        }
                    };
                    testsrc.set_property("pattern", pattern_enum);
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
