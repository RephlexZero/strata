use gst::MessageView;
use gst::prelude::*;
use std::collections::{HashSet, VecDeque};
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strata_bonding::metrics::MetricsServer;

use gststrata::hls_upload;

const SENDER_HELP: &str = r#"
USAGE: strata-pipeline sender [OPTIONS] --dest <ADDR[,ADDR...]>

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
  --startup-ramp-ms <ms> Gently ramp the encoder from a low floor up to
                      --bitrate over this window so a cold link isn't blasted
                      with full rate at startup (0 = disabled, default: 0)
  --startup-floor-kbps <kbps> Bitrate the startup ramp begins at
                      (clamped to >= --min-bitrate; 0 = adapter default)
  --framerate <fps>   Video framerate (default: 30)
  --audio             Add silent AAC audio track (required for relay targets)
  --config <path>     Path to TOML config file (see Configuration Reference)
  --stats-dest <addr> UDP address to relay stats JSON (e.g. 127.0.0.1:9100)
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
  strata-pipeline sender --dest server:5000,server:5002 \
    --config sender.toml

  # 1080p30 with audio for YouTube relay via receiver
  strata-pipeline sender --source test --framerate 30 --audio --bitrate 2000 \
    --dest receiver:5000,receiver:5002,receiver:5004

  # HDMI capture card to cloud receiver
  strata-pipeline sender --source v4l2 --device /dev/video0 \
    --dest cloud.example.com:5000,cloud.example.com:5002 \
    --bitrate 2000 --config sender.toml
"#;

const RECEIVER_HELP: &str = r#"
USAGE: strata-pipeline receiver [OPTIONS] --bind <ADDR>

OPTIONS:
  --bind <addr>       Bind address (required), e.g. 0.0.0.0:5000
                      Multiple bind addresses: 0.0.0.0:5000,0.0.0.0:5002
  --output <path>     Record to file (.ts = raw MPEG-TS, .mp4 = remuxed)
  --relay-url <url>   URL to relay the received stream to
                      e.g. rtmp://a.rtmp.youtube.com/live2/STREAM_KEY
                           https://a.upload.youtube.com/http_upload_hls?cid=KEY&copy=0&file=
  --relay-type <type> Relay protocol: rtmp or hls
                      Inferred from URL scheme when omitted:
                        rtmp:// or rtmps:// → rtmp
                        https://            → hls
  --codec <codec>     Codec of incoming stream: h265 (default) or h264
  --config <path>     Path to TOML config file (see Configuration Reference)
  --metrics-port <port> Start Prometheus metrics endpoint on this port
                        (serves /metrics on 0.0.0.0:<port>)
  --help              Show this help

EXAMPLES:
  # Receive and monitor (no file output)
  strata-pipeline receiver --bind 0.0.0.0:5000

  # Receive bonded stream and relay to YouTube
  strata-pipeline receiver --bind 0.0.0.0:5000,0.0.0.0:5002,0.0.0.0:5004 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"

  # Receive H.265 stream and relay
  strata-pipeline receiver --bind 0.0.0.0:5000 --codec h265 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"

  # Receive and record to MPEG-TS file
  strata-pipeline receiver --bind 0.0.0.0:5000 --output capture.ts

  # Receive with config
  strata-pipeline receiver --bind 0.0.0.0:5000 --config receiver.toml
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
            eprintln!("Usage: strata-pipeline <sender|receiver> [args]\n");
            eprintln!("Modes:");
            eprintln!("  sender    Encode and transmit video over bonded Strata links");
            eprintln!("  receiver  Receive and reassemble bonded Strata stream\n");
            eprintln!(
                "Run `strata-pipeline sender --help` or `strata-pipeline receiver --help` for mode-specific options."
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
    gsthlssink3::plugin_desc::plugin_register_static()?;
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

/// Reach `hlssink3`'s internal `mpegtsmux` (it muxes `video`/`audio` request
/// pads itself rather than accepting a pre-muxed stream — see
/// `gst-plugin-hlssink3`'s `hlssink3/imp.rs`, which wires it into its
/// internal `splitmuxsink` as that element's `muxer` property) and apply the
/// same settings `configure_mpegtsmux` applies to a standalone `mpegtsmux`:
/// alignment for UDP-style packetisation and preserved timestamps. PAT/PMT
/// interval is left at mpegtsmux's own default (9000 = 100 ms — already what
/// we want, see AGENTS.md on `pat-interval`): setting it explicitly here hits
/// a `tsmux_set_pmt_interval: assertion 'program != NULL' failed` critical,
/// because this internal muxer doesn't have a program until splitmuxsink
/// actually starts muxing, unlike a top-level `mpegtsmux name=mux` whose
/// pads (and program) are requested immediately while `gst::parse::launch`
/// parses the bin description.
fn configure_hlssink3_muxer(hls: &gst::Element) {
    let Some(bin) = hls.downcast_ref::<gst::Bin>() else {
        return;
    };
    let Some(splitmux) = bin.by_name("split_mux_sink") else {
        eprintln!("hlssink3: could not find internal splitmuxsink to configure its muxer");
        return;
    };
    if splitmux.find_property("muxer").is_none() {
        return;
    }
    let muxer = splitmux.property::<gst::Element>("muxer");
    if muxer.find_property("alignment").is_some() {
        muxer.set_property("alignment", 7i32);
    }
    if muxer.find_property("skew-corrections").is_some() {
        muxer.set_property("skew-corrections", false);
        eprintln!("hlssink3's internal mpegtsmux: disabled skew-corrections (GStreamer ≥1.28)");
    }
}

/// Install the DeliveredStream gate (A2) on a parsed-video src pad.
///
/// The bonding transport delivers in-order but, under loss, gap-skips leave the
/// MPEG-TS multiplex holed: the next decoded access unit can be partial or carry
/// a backwards/NONE DTS. A plain re-mux's `mpegtsmux` treats that as fatal
/// ("Timestamping error on input streams"). This probe makes the stream the
/// muxer sees *clean* so the re-mux can't be poisoned and still segments:
///   1. emit nothing until the first keyframe (IDR);
///   2. after any DISCONT, drop the rest of the damaged GOP — resume at the
///      next keyframe (the standard "decode from next IDR after loss");
///   3. drop any buffer whose DTS regresses below the last emitted (the actual
///      crash trigger), so the muxer only ever sees monotonic DTS.
///
/// Every mid-stream resume is a genuine timeline jump in the egress HLS, so it
/// also stamps DISCONT on the resumed buffer and records its running time in
/// `pending_resumes` — the bus watch in `run_receiver` matches that against
/// `hlssink3`'s `hls-segment-added` messages to mark the corresponding segment
/// for `#EXT-X-DISCONTINUITY` in `hls_upload.rs`.
/// Largest credible single-step forward DTS/PTS move mid-stream. Legitimate
/// forward gaps are bounded by the playout window's gap-skips (≤ 3 s ceiling)
/// plus a GOP; anything bigger is a demux timeline latch onto a corrupted PES
/// header (2026-07-04 field run 2: a mid-PES splice under a loss burst made
/// tsdemux re-base video +107 s while audio stayed sane — mpegtsmux then sat
/// waiting to interleave forever, and HLS egress silently stopped at t≈25 s
/// while every transport metric stayed green).
const MAX_FORWARD_STEP_NS: u64 = 10_000_000_000;

/// Classify a buffer's DTS against the last one emitted to the muxer.
fn timeline_step(dts: Option<u64>, last_emitted: Option<u64>) -> TimelineStep {
    match (dts, last_emitted) {
        (Some(d), Some(last)) if d < last => TimelineStep::Regression,
        (Some(d), Some(last)) if d - last > MAX_FORWARD_STEP_NS => TimelineStep::WildJump,
        _ => TimelineStep::Ok,
    }
}

#[derive(PartialEq)]
enum TimelineStep {
    Ok,
    Regression,
    WildJump,
}

fn install_delivered_stream_gate(
    pad: &gst::Pad,
    pending_resumes: Arc<Mutex<VecDeque<gst::ClockTime>>>,
) {
    struct GateState {
        waiting_for_keyframe: bool,
        last_dts: Option<u64>,
        dropped: u64,
        started: bool,
        pending_discont: bool,
        segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    }
    let state = Mutex::new(GateState {
        // Start by waiting: never hand the muxer a mid-GOP opening run.
        waiting_for_keyframe: true,
        last_dts: None,
        dropped: 0,
        // Whether we've ever resumed. The first keyframe of the stream
        // legitimately carries GStreamer's startup DISCONT, and there is no
        // prior reference frame for it to corrupt — so we accept it. Only
        // *mid-stream* keyframes that carry DISCONT are treated as damaged.
        started: false,
        // Set by a "strata/discont" custom event; applied to the next buffer.
        pending_discont: false,
        segment: None,
    });
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |_pad, info| {
        // stratasrc signals a skipped gap with a serialized custom event
        // because the buffer DISCONT flag does not survive tsdemux (field
        // observation: 580 aggregator discontinuities, zero DISCONT-flagged
        // buffers at this pad — the gate never engaged and splices decoded
        // as grey frames). The event rides the same serialized stream, so
        // the first buffer after it is the first post-gap buffer.
        if let Some(gst::PadProbeData::Event(ev)) = &info.data {
            if let gst::EventView::Segment(seg_ev) = ev.view()
                && let Some(segment) = seg_ev.segment().downcast_ref::<gst::ClockTime>()
            {
                state.lock().unwrap().segment = Some(segment.clone());
                return gst::PadProbeReturn::Ok;
            }
            if let gst::EventView::CustomDownstream(c) = ev.view()
                && c.structure().is_some_and(|s| s.name() == "strata/discont")
            {
                let mut st = state.lock().unwrap();
                st.pending_discont = true;
                eprintln!(
                    "DeliveredStream gate: discont event (last_dts={})",
                    st.last_dts.map(|d| d / 1_000_000).unwrap_or(0)
                );
            }
            return gst::PadProbeReturn::Ok;
        }
        let Some(buf) = info.buffer() else {
            return gst::PadProbeReturn::Ok;
        };
        let flags = buf.flags();
        let is_keyframe = !flags.contains(gst::BufferFlags::DELTA_UNIT);
        let dts = buf.dts().map(|t| t.nseconds());
        let pts = buf.pts();

        let mut st = state.lock().unwrap();
        let is_discont =
            flags.contains(gst::BufferFlags::DISCONT) || std::mem::take(&mut st.pending_discont);
        if is_discont {
            st.waiting_for_keyframe = true;
        }
        if st.waiting_for_keyframe {
            // Resume only on a keyframe we can trust. A keyframe that *itself*
            // carries DISCONT sits right after a skipped gap: at the byte
            // level the hole may have truncated the head of this IDR, so it
            // can be a damaged keyframe. Resuming on it is exactly how a
            // corrupt reference frame reached the decoder (grey / "ref with
            // POC"). Drop it and wait for the next CLEAN keyframe instead.
            // Exception: the very first keyframe (startup DISCONT, no prior
            // reference to corrupt) is always accepted so the stream can lock.
            // A resume must also stay on a credible timeline: never below the
            // emitted-DTS watermark (a backwards step is the exact fatal
            // "Timestamping error" this gate exists to prevent — 2026-07-04
            // run 1 resumed at 21.8 s after emitting 22.26 s), and never a
            // wild forward leap (run 2: a corrupt-PES +107 s latch would have
            // poisoned the watermark and stalled interleaving downstream).
            // Media time advances in real time, so a regression wait resolves
            // within the re-base magnitude + one GOP; a wild-jump wait holds
            // the last sane timeline and screams in the log instead of
            // silently wedging the muxer.
            let credible = timeline_step(dts, st.last_dts) == TimelineStep::Ok;
            let trustworthy_keyframe =
                is_keyframe && (!st.started || (!is_discont && credible));
            if trustworthy_keyframe {
                // A mid-stream resume (not the stream's very first keyframe) is
                // a real splice: stamp DISCONT so the muxer/sink see it, and
                // queue its running time for hls-segment-added correlation.
                if st.started {
                    if let (Some(segment), Some(pts)) = (&st.segment, pts)
                        && let Some(running_time) = segment.to_running_time(pts)
                    {
                        pending_resumes.lock().unwrap().push_back(running_time);
                    }
                    info.buffer_mut()
                        .unwrap()
                        .make_mut()
                        .set_flags(gst::BufferFlags::DISCONT);
                }
                st.waiting_for_keyframe = false;
                st.started = true;
                // Reset the DTS baseline to this IDR: the forward jump across
                // the skipped gap is expected and must not look like a regression.
                st.last_dts = dts;
                // PTS-stamped resume marker: lets a decode error found in the
                // HLS output be attributed to "gate knew about this splice"
                // vs "splice the gate never saw" (e.g. a leaky-queue drop).
                eprintln!(
                    "DeliveredStream gate: resumed at clean IDR pts={} (dropped {} total)",
                    pts.map(|t| t.mseconds()).unwrap_or(0),
                    st.dropped
                );
                return gst::PadProbeReturn::Ok;
            }
            st.dropped += 1;
            if st.dropped.is_power_of_two() {
                eprintln!(
                    "DeliveredStream gate: dropped {} buffer(s) awaiting a clean IDR after loss",
                    st.dropped
                );
            }
            return gst::PadProbeReturn::Drop;
        }
        // Timeline guard: a backwards DTS is what kills mpegtsmux, and a wild
        // forward DTS (corrupt-PES demux latch) is what silently stalls it —
        // the muxer buffers the leapt video waiting for the other stream to
        // interleave up to it, which never happens. Dropping ONLY the
        // offending buffer is not enough — it may be a reference frame, and
        // silently deleting one leaves every following P-frame decoding
        // against a missing reference (full-frame grey at the far decoder,
        // invisible to every metric here). Treat both like a discontinuity:
        // drop and RESYNC to the next clean, credible keyframe.
        match timeline_step(dts, st.last_dts) {
            TimelineStep::Regression => {
                st.waiting_for_keyframe = true;
                st.dropped += 1;
                let (d, last) = (dts.unwrap(), st.last_dts.unwrap());
                eprintln!(
                    "DeliveredStream gate: DTS regression ({d} < {last}) — resyncing to next clean IDR"
                );
                return gst::PadProbeReturn::Drop;
            }
            TimelineStep::WildJump => {
                st.waiting_for_keyframe = true;
                st.dropped += 1;
                let (d, last) = (dts.unwrap(), st.last_dts.unwrap());
                eprintln!(
                    "DeliveredStream gate: wild forward DTS jump ({d} >> {last}) — dropping corrupt timeline, awaiting credible IDR"
                );
                return gst::PadProbeReturn::Drop;
            }
            TimelineStep::Ok => {}
        }
        if dts.is_some() {
            st.last_dts = dts;
        }
        gst::PadProbeReturn::Ok
    });
}

/// Monotonic-DTS guard for streams without keyframes (audio).
///
/// A subset of [`install_delivered_stream_gate`]: it drops buffers whose DTS
/// regresses below the last emitted one — the exact condition that makes
/// `mpegtsmux` abort with "Timestamping error on input streams" — or leaps
/// forward incredibly (corrupt-PES demux latch; the leapt buffer would make
/// the muxer wait for the video stream to interleave up to it). Audio frames
/// are all independent, so there is no keyframe to resync to after loss; we
/// drop offenders one-by-one without moving the watermark and log at
/// power-of-two counts (this pad was previously fully silent, which is how a
/// starved-muxer stall stayed invisible on 2026-07-04).
fn install_monotonic_dts_gate(pad: &gst::Pad) {
    use std::sync::Mutex;
    let state = Mutex::new((None::<u64>, 0u64)); // (last_dts, dropped)
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        let Some(gst::PadProbeData::Buffer(buf)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let dts = buf.dts().map(|t| t.nseconds());
        let mut st = state.lock().unwrap();
        if timeline_step(dts, st.0) != TimelineStep::Ok {
            st.1 += 1;
            if st.1.is_power_of_two() {
                eprintln!(
                    "Monotonic-DTS gate (audio): dropped {} non-credible buffer(s) (dts={:?}, last={:?})",
                    st.1, dts, st.0
                );
            }
            return gst::PadProbeReturn::Drop;
        }
        if dts.is_some() {
            st.0 = dts;
        }
        gst::PadProbeReturn::Ok
    });
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
    let mut passthrough = false;
    let mut metrics_port: Option<u16> = None;
    let mut codec_str = "h265";
    let mut min_bitrate_kbps: Option<u32> = None;
    let mut max_bitrate_kbps: Option<u32> = None;
    let mut startup_ramp_ms: u32 = 0;
    let mut startup_floor_kbps: u32 = 0;

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
            "--startup-ramp-ms" if i + 1 < args.len() => {
                startup_ramp_ms = args[i + 1].parse().unwrap_or(0);
                i += 1;
            }
            "--startup-floor-kbps" if i + 1 < args.len() => {
                startup_floor_kbps = args[i + 1].parse().unwrap_or(0);
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
    let profile =
        strata_protocol::profiles::lookup_profile(Some(resolution), Some(framerate), Some(codec_str));
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

            links.push({
                let mut obj = serde_json::json!({
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
                });
                if let Ok(bw) = s.get::<f64>(&format!("link_{}_btlbw_bps", id)) {
                    obj["btlbw_bps"] = serde_json::json!(bw.round() as u64);
                }
                if let Ok(rtp) = s.get::<f64>(&format!("link_{}_rtprop_ms", id)) {
                    obj["rtprop_ms"] = serde_json::json!(rtp);
                }
                obj
            });
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
                // v4l2src is always a live source internally; it does not
                // expose is-live as a settable GObject property.  The
                // clocksync element downstream handles hot-swap timestamps.
                .build()?;
            // Many USB cameras only reach the target framerate in MJPG (this
            // FHD cam does 30fps only as MJPG; YUYV caps at ~10-15fps).
            // decodebin auto-plugs jpegdec for MJPG and passes raw through, so
            // we negotiate whatever format the camera actually offers at the
            // requested resolution/fps instead of forcing video/x-raw.
            let dbin = gst::ElementFactory::make("decodebin").build()?;
            // clocksync re-stamps buffers against the pipeline clock so that
            // switching from testsrc (already clock-stamped) to v4l2 does not
            // produce a backwards or large-forward jump at input-selector.
            let csync = gst::ElementFactory::make("clocksync").build()?;
            let conv = gst::ElementFactory::make("videoconvert").build()?;
            let scale = gst::ElementFactory::make("videoscale").build()?;
            // videorate reconciles the camera's delivered framerate with the
            // requested one. Critical for UVC cams whose only mode at a given
            // resolution is low-fps raw (this FHD cam offers 1080p only as
            // YUY2 @ 5fps); without it the downstream framerate=N/1 capsfilter
            // cannot negotiate and the branch dies with a not-linked error.
            // It's a passthrough when the rates already match.
            let rate = gst::ElementFactory::make("videorate").build()?;
            let filter = gst::ElementFactory::make("capsfilter")
                .property("caps", &caps)
                .build()?;
            let queue = gst::ElementFactory::make("queue")
                .property("max-size-buffers", 3u32)
                .build()?;
            // decodebin has dynamic src pads — link to clocksync when it appears.
            let csync_weak = csync.downgrade();
            dbin.connect_pad_added(move |_, pad| {
                if let Some(csync) = csync_weak.upgrade()
                    && let Some(sink_pad) = csync.static_pad("sink")
                    && !sink_pad.is_linked()
                {
                    let _ = pad.link(&sink_pad);
                }
            });
            let name = src.name().to_string();
            (
                vec![src, dbin, csync, conv, scale, rate, filter, queue],
                name,
            )
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
            // See the v4l2 branch: videorate reconciles source fps with the
            // requested framerate so the downstream capsfilter can negotiate.
            let rate = gst::ElementFactory::make("videorate").build()?;
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
            // Link conv→scale→rate→filter→queue (src→conv linked via pad-added)
            (vec![src, conv, scale, rate, filter, queue], name)
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
    // For v4l2: src → conv → scale → rate → filter → queue
    // For uri: conv → scale → rate → filter → queue (src→conv via pad-added)
    if mode == "v4l2" {
        // v4l2src ! decodebin (static); decodebin →(dynamic, via pad-added)
        // clocksync; then clocksync ! videoconvert ! videoscale ! videorate ! capsfilter ! queue.
        elements[0].link(&elements[1])?;
        gst::Element::link_many(&elements[2..])?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayType {
    Rtmp,
    Hls,
}

impl RelayType {
    /// Infer relay type from URL scheme.
    /// Returns `None` if the scheme is unrecognised and the caller must require
    /// an explicit `--relay-type` flag.
    fn from_url(url: &str) -> Option<Self> {
        if url.starts_with("rtmp://") || url.starts_with("rtmps://") {
            Some(RelayType::Rtmp)
        } else if url.starts_with("https://") {
            Some(RelayType::Hls)
        } else {
            None
        }
    }
}

/// Egress watchdog (HLS relay): rebuild the pipeline when hlssink3 stops
/// adding segments for this long. Steady-state cadence is one segment per
/// target-duration (1 s) and the gates' resync droughts span a few seconds,
/// so 15 s of silence means the demux/mux layer has wedged — 2026-07-04 run 4
/// (orangepi-118293) went dark for its final 98 s with transport green and
/// nothing reaching the gates. The wedge sits inside tsdemux/hlssink3 where
/// no pad probe can see it, so the only live repair is to rebuild the decode
/// side. STRATA_EGRESS_WATCHDOG_SEC overrides (0 disables — e.g. a GST_DEBUG
/// diagnostic run that should observe a wedge, not heal it).
const EGRESS_STALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Allowance before a generation's FIRST segment: stream lock + first clean
/// IDR + a full target-duration takes longer than the steady-state cadence,
/// and after a rebuild stratasrc must rejoin the live transport mid-stream.
const EGRESS_FIRST_SEGMENT_ALLOWANCE: Duration = Duration::from_secs(30);

/// Fill levels of the three named egress queues, logged when the watchdog
/// trips. This splits the two run-4 suspects: q_v/q_a holding data while no
/// segment lands means hlssink3's internal muxer is starved/blocked; all
/// three near-empty means tsdemux stopped emitting.
fn dump_egress_queue_levels(pipeline: &gst::Pipeline) {
    for name in ["q_ts", "q_v", "q_a"] {
        if let Some(q) = pipeline.by_name(name) {
            let buffers = q.property::<u32>("current-level-buffers");
            let time_ns = q.property::<u64>("current-level-time");
            eprintln!(
                "egress-watchdog: {name}: {buffers} buffer(s) / {} ms queued",
                time_ns / 1_000_000
            );
        }
    }
}

fn run_receiver(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut bind_str = "";
    let mut output_file = "";
    let mut config_path = "";
    let mut relay_url = "";
    let mut relay_type_override: Option<RelayType> = None;
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
            "--relay-type" if i + 1 < args.len() => {
                relay_type_override = match args[i + 1].to_ascii_lowercase().as_str() {
                    "rtmp" => Some(RelayType::Rtmp),
                    "hls" => Some(RelayType::Hls),
                    other => {
                        eprintln!("Unknown --relay-type '{}': expected 'rtmp' or 'hls'", other);
                        std::process::exit(1);
                    }
                };
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
    let relay_parser = codec_type.relay_parser_fragment();

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
    let relay_type = if use_relay {
        let resolved = relay_type_override
            .or_else(|| RelayType::from_url(relay_url))
            .unwrap_or_else(|| {
                eprintln!(
                    "Cannot infer relay type from URL '{}'. \
                     Use --relay-type rtmp|hls to specify it explicitly.",
                    relay_url
                );
                std::process::exit(1);
            });
        Some(resolved)
    } else {
        None
    };
    let use_hls_relay = relay_type == Some(RelayType::Hls);

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

    // Everything with process lifetime lives OUTSIDE the generation loop
    // below: the egress watchdog can tear the pipeline down and rebuild it
    // mid-run, and the uploader (which discovers segments by directory scan,
    // not by who wrote them), the gate→playlist discontinuity plumbing, and
    // the Ctrl+C handler must all span those rebuilds.
    //
    // `pending_resumes`/`discontinuous_segments` carry gate-resume running
    // times to the bus loop below, which maps them onto hlssink3's
    // `hls-segment-added` messages so hls_upload.rs knows which segments to
    // mark with `#EXT-X-DISCONTINUITY`.
    let pending_resumes: Arc<Mutex<VecDeque<gst::ClockTime>>> =
        Arc::new(Mutex::new(VecDeque::new()));
    let discontinuous_segments: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // AppSink counters (only used for non-relay monitor/record modes)
    let received_count = Arc::new(Mutex::new(0u64));
    let received_bytes = Arc::new(Mutex::new(0u64));

    // Graceful shutdown: EOS the *current* pipeline (it changes across
    // watchdog rebuilds); the flag stops the generation loop from rebuilding.
    let shutdown = Arc::new(AtomicBool::new(false));
    let current_pipeline: Arc<Mutex<Option<gst::Pipeline>>> = Arc::new(Mutex::new(None));
    {
        let shutdown = shutdown.clone();
        let current_pipeline = current_pipeline.clone();
        ctrlc::set_handler(move || {
            eprintln!("Received shutdown signal. Sending EOS to pipeline...");
            shutdown.store(true, Ordering::SeqCst);
            if let Some(pipeline) = current_pipeline.lock().unwrap().as_ref() {
                let _ = pipeline.send_event(gst::event::Eos::new());
            }
        })
        .expect("Error setting signal handler");
    }

    // Start HLS segment uploader if in HLS relay mode (receiver)
    let _hls_uploader = if use_hls_relay {
        let hls_dir = hls_tmp_dir.as_ref().unwrap().clone();
        let base_url = hls_upload::hls_base_url(relay_url).to_string();
        Some(hls_upload::start_hls_uploader(
            hls_upload::HlsUploaderConfig {
                segment_dir: hls_dir,
                base_url,
                playlist_filename: "playlist.m3u8".into(),
                discontinuous_segments: discontinuous_segments.clone(),
            },
        ))
    } else {
        None
    };

    let watchdog_stall = match env::var("STRATA_EGRESS_WATCHDOG_SEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(EGRESS_STALL_TIMEOUT),
    };
    if use_hls_relay {
        match watchdog_stall {
            Some(d) => eprintln!(
                "Egress watchdog: pipeline rebuild after {}s without a new HLS segment (STRATA_EGRESS_WATCHDOG_SEC=0 to disable)",
                d.as_secs()
            ),
            None => eprintln!("Egress watchdog: disabled"),
        }
    }

    let mut generation: u32 = 0;
    loop {
        let pipeline_str = if use_hls_relay {
            let hls_dir = hls_tmp_dir.as_ref().unwrap();
            // Segment names carry the pipeline generation: hlssink3 restarts
            // its %05d counter at zero after a watchdog rebuild, and the
            // uploader tracks uploads by filename — a reused name would never
            // re-upload. The zero-padded prefix keeps names sorted across
            // generations, which find_new_segments()'s hold-back-the-latest
            // logic relies on.
            let seg_location = hls_dir.join(format!("seg-g{generation:04}-%05d.ts"));
            let pl_location = hls_dir.join("playlist.m3u8");
            // HLS via the DeliveredStream contract (A2). Two earlier approaches
            // each failed one half of the problem:
            //   * Re-mux (tsdemux → h265parse → hlssink2) SEGMENTS correctly but
            //     mpegtsmux dies on the first backwards/NONE DTS from a gap-skip
            //     ("Timestamping error on input streams") — fatal under loss.
            //   * TS-passthrough (tsparse split-on-rai → hlssink) SURVIVES loss but
            //     cannot segment a network TS: hlssink only cuts when a live encoder
            //     upstream answers its force-key-unit requests, which never happens
            //     across the network → one ever-growing segment, no playlist.
            // The fix is to make the stream the egress sees *clean* — monotonic DTS,
            // discontinuities only at keyframe (IDR) boundaries — so a plain re-mux
            // both segments AND cannot be poisoned. The cleaning happens in a pad
            // probe on the parsed video (`install_delivered_stream_gate`), because
            // the keyframe signal is only reliable post-demux (mpegtsmux strips
            // DELTA_UNIT, so the bonding transport can't see keyframes — see
            // sink.rs). The cross-stream audio/video DTS interleave at startup was
            // a second crash trigger (run 5032), so audio rides its own gate: the
            // monotonic-DTS half of the video gate (AAC has no keyframes, so there
            // is nothing to wait for — only backwards DTS must be dropped). YouTube
            // Live will not display a video-only HLS, so the silent AAC track the
            // sender muxes has to survive the re-mux.
            format!(
                "stratasrc links=\"{bind}\" name=src latency=200 ! \
                 queue name=q_ts max-size-buffers=0 max-size-bytes=0 max-size-time=5000000000 \
                 leaky=downstream ! \
                 tsparse set-timestamps=true alignment=7 ! \
                 tsdemux name=d \
                 hlssink3 name=hls location=\"{seg}\" playlist-location=\"{pl}\" \
                 target-duration=1 max-files=10 playlist-length=6 \
                 d. ! \
                 queue name=q_v max-size-buffers=0 max-size-bytes=0 max-size-time=10000000000 \
                 leaky=downstream ! \
                 {parser} name=vparse ! hls.video \
                 d. ! \
                 queue name=q_a max-size-buffers=0 max-size-bytes=0 max-size-time=10000000000 \
                 leaky=downstream ! \
                 aacparse name=aparse ! hls.audio",
                bind = bind_str,
                parser = relay_parser,
                seg = seg_location.display(),
                pl = pl_location.display(),
            )
        } else if use_relay {
            let relay_frag =
                gststrata::codec::CodecController::new(codec_type).relay_muxer_fragment();
            format!(
                "stratasrc links=\"{bind}\" name=src latency=200 ! \
                 queue max-size-buffers=0 max-size-bytes=0 max-size-time=5000000000 \
                 leaky=downstream ! \
                 tsdemux name=d \
                 d. ! queue max-size-buffers=600 max-size-bytes=0 max-size-time=2000000000 \
                       leaky=downstream ! {parser} ! {relay} \
                 rtmpsink location=\"{url}\" sync=false \
                 d. ! queue max-size-buffers=200 max-size-bytes=0 max-size-time=2000000000 \
                       leaky=downstream ! aacparse ! fmux.",
                bind = bind_str,
                parser = relay_parser,
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

        // HLS re-mux egress: install the DeliveredStream gate on the parsed
        // video and disable mpegtsmux skew correction so it preserves our
        // timestamps.
        if use_hls_relay {
            if let Some(vparse) = pipeline.by_name("vparse")
                && let Some(src) = vparse.static_pad("src")
            {
                install_delivered_stream_gate(&src, pending_resumes.clone());
                eprintln!("DeliveredStream gate installed on vparse src pad");
            }
            // Audio rides a monotonic-DTS-only gate (see pipeline comment): mpegtsmux
            // dies on a backwards DTS, and the audio PID can regress after a gap-skip
            // just like video. No keyframe logic — every AAC frame is independent.
            if let Some(aparse) = pipeline.by_name("aparse")
                && let Some(src) = aparse.static_pad("src")
            {
                install_monotonic_dts_gate(&src);
                eprintln!("Monotonic-DTS gate installed on aparse src pad");
            }
            if let Some(hls) = pipeline.by_name("hls") {
                configure_hlssink3_muxer(&hls);
            }
        }

        // Setup AppSink (only used for non-relay monitor/record modes)
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

        *current_pipeline.lock().unwrap() = Some(pipeline.clone());

        pipeline.set_state(gst::State::Playing)?;
        if shutdown.load(Ordering::SeqCst) {
            // Ctrl+C raced pipeline startup: its EOS hit the previous (or a
            // NULL) pipeline and was lost — re-send it to this one.
            let _ = pipeline.send_event(gst::event::Eos::new());
        }

        // ── Prometheus metrics server (receiver) ──
        // Per generation: the stats handle belongs to this pipeline's
        // stratasrc, and dropping the server at iteration end frees the port
        // for the rebuilt pipeline's server.
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

        // Most recently closed HLS segment (running-time start, filename), used to
        // attribute queued gate-resume running times to the segment they fell in.
        // hlssink3 reports a segment only once it has fully closed, so a resume
        // queued during segment N is still unclaimed when segment N's own message
        // arrives — it's only resolved one message later, against segment N+1's
        // start, which is exactly the lag find_new_segments() already holds the
        // newest segment back for (see hls_upload.rs).
        let mut last_segment: Option<(gst::ClockTime, String)> = None;

        // Egress heartbeat for the watchdog. Progress is *segments*, nothing
        // else — strata-stats messages keep arriving while wedged (run 4:
        // transport stayed green through 98 s of dead air).
        let mut last_progress = Instant::now();
        let mut stall_allowance = if use_hls_relay {
            watchdog_stall.map(|d| d.max(EGRESS_FIRST_SEGMENT_ALLOWANCE))
        } else {
            None
        };
        let mut stalled = false;

        // Standard GStreamer message loop (1 s pop timeout so the watchdog
        // runs even when the bus goes quiet)
        loop {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(1)) {
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
                            if s.name() == "hls-segment-added"
                                && let (Ok(location), Ok(running_time)) = (
                                    s.get::<String>("location"),
                                    s.get::<gst::ClockTime>("running-time"),
                                )
                            {
                                last_progress = Instant::now();
                                stall_allowance = watchdog_stall;
                                if let Some((prev_start, prev_location)) = &last_segment {
                                    let mut resumes = pending_resumes.lock().unwrap();
                                    let mut claimed_any = false;
                                    while resumes.front().is_some_and(|r| *r < running_time) {
                                        resumes.pop_front();
                                        claimed_any = true;
                                    }
                                    if claimed_any {
                                        discontinuous_segments.lock().unwrap().insert(
                                            std::path::Path::new(prev_location)
                                                .file_name()
                                                .map(|n| n.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| prev_location.clone()),
                                        );
                                        eprintln!(
                                            "DeliveredStream gate: marking segment {} as discontinuous (resume at running_time={})",
                                            prev_location,
                                            prev_start.mseconds()
                                        );
                                    }
                                }
                                eprintln!(
                                    "hlssink3: segment added, location={} running_time={}",
                                    location,
                                    running_time.mseconds()
                                );
                                last_segment = Some((running_time, location));
                            }
                        }
                    }
                    _ => (),
                }
            }
            if let Some(allowance) = stall_allowance
                && last_progress.elapsed() >= allowance
            {
                stalled = true;
                break;
            }
        }

        *current_pipeline.lock().unwrap() = None;

        if stalled {
            eprintln!(
                "egress-watchdog: no HLS segment for {}s (generation {}) — rebuilding the pipeline",
                last_progress.elapsed().as_secs(),
                generation
            );
            dump_egress_queue_levels(&pipeline);
            // EOS before NULL: a wedged hlssink3 still flushes the segments its
            // muxer is holding (run 4's EOS released three), so they upload
            // instead of vanishing with the old pipeline. Bounded wait — the
            // wedge may swallow the EOS too.
            let _ = pipeline.send_event(gst::event::Eos::new());
            let flush_deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < flush_deadline {
                let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(500)) else {
                    continue;
                };
                match msg.view() {
                    MessageView::Eos(..) => break,
                    MessageView::Error(err) => {
                        eprintln!("egress-watchdog: error during EOS flush: {}", err.error());
                        break;
                    }
                    MessageView::Element(element) => {
                        if let Some(s) = element.structure()
                            && s.name() == "hls-segment-added"
                            && let Ok(location) = s.get::<String>("location")
                        {
                            eprintln!("egress-watchdog: EOS flush released segment {location}");
                        }
                    }
                    _ => (),
                }
            }
            pipeline.set_state(gst::State::Null)?;
            // Gate-resume running times are meaningless across pipelines.
            pending_resumes.lock().unwrap().clear();
            generation += 1;
            // The rebuilt pipeline starts a fresh timeline, so its first
            // segment is a genuine discontinuity in the published HLS. Its
            // name is deterministic: hlssink3 restarts %05d at zero.
            discontinuous_segments
                .lock()
                .unwrap()
                .insert(format!("seg-g{generation:04}-00000.ts"));
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            continue;
        }

        pipeline.set_state(gst::State::Null)?;
        break;
    }

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

#[cfg(test)]
mod tests {
    use super::{timeline_step, TimelineStep, MAX_FORWARD_STEP_NS};

    #[test]
    fn timeline_step_classifies_all_regimes() {
        // No baseline yet, or no DTS on the buffer → always credible.
        assert!(timeline_step(None, None) == TimelineStep::Ok);
        assert!(timeline_step(Some(5), None) == TimelineStep::Ok);
        assert!(timeline_step(None, Some(5)) == TimelineStep::Ok);
        // Normal forward motion, including a playout-window-sized gap-skip.
        assert!(timeline_step(Some(1_000), Some(999)) == TimelineStep::Ok);
        assert!(timeline_step(Some(3_000_000_000), Some(0)) == TimelineStep::Ok);
        assert!(
            timeline_step(Some(MAX_FORWARD_STEP_NS), Some(0)) == TimelineStep::Ok,
            "exactly at the bound is still credible"
        );
        // Backwards → the mpegtsmux-fatal case (2026-07-04 run 1).
        assert!(timeline_step(Some(21_007_913_971), Some(22_264_073_509)) == TimelineStep::Regression);
        // The run-2 corrupt-PES latch: video leapt ~+107 s in one step.
        assert!(
            timeline_step(Some(227_320_000_000), Some(24_694_000_000)) == TimelineStep::WildJump
        );
        assert!(timeline_step(Some(MAX_FORWARD_STEP_NS + 1), Some(0)) == TimelineStep::WildJump);
    }
}
