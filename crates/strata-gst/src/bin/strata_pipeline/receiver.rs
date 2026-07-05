//! Receiver mode: reassemble the bonded stream and relay/record/monitor it,
//! with the HLS egress watchdog and generation-rebuild loop.

use gst::MessageView;
use gst::prelude::*;
use std::collections::{HashSet, VecDeque};
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gststrata::hls_upload;

use crate::cli::ReceiverArgs;
use crate::gate::{install_delivered_stream_gate, install_monotonic_dts_gate};
use crate::stats::serialize_bonding_stats;
use crate::util::{configure_hlssink3_muxer, register_plugins};

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
/// A watchdog rebuild can race the kernel's deferred io_uring teardown: with
/// SQPOLL the old generation's UDP sockets are released asynchronously after
/// their reader threads join, so the rebind can transiently hit EADDRINUSE
/// (field run orangepi-123888: generation 1 died on StateChangeError while
/// generation 0's link threads had cleanly exited). Retry with a pause
/// instead of dying; give up if the port genuinely stays taken.
const MAX_REBUILD_ATTEMPTS: u32 = 5;
const REBUILD_RETRY_PAUSE: Duration = Duration::from_secs(1);

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

pub(crate) fn run_receiver(args: &ReceiverArgs) -> Result<(), Box<dyn std::error::Error>> {
    let bind_str = args.bind.as_str();
    let output_file = args.output.as_str();
    let config_path = args.config.as_str();
    let relay_url = args.relay_url.as_str();
    let relay_type_override: Option<RelayType> = args.relay_type.as_deref().map(|t| match t {
        "rtmp" => RelayType::Rtmp,
        "hls" => RelayType::Hls,
        other => unreachable!("clap validated --relay-type: {other}"),
    });
    let codec_str = args.codec.as_str();
    let metrics_port = args.metrics_port;
    let stats_dest = args.stats_dest.as_str();

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

    // ── Stats relay (same contract as the sender path) ──
    // The receiver daemon spawns us with --stats-dest and drains this socket
    // in telemetry.rs; link stats plus the egress heartbeat travel here.
    let mut stats_socket = None;
    if !stats_dest.is_empty() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
        stats_socket = Some(sock);
        eprintln!("Stats relay → {}", stats_dest);
    }

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
    let mut rebuild_attempts: u32 = 0;
    // Cumulative across generations — the daemon-facing egress heartbeat
    // must not reset when the watchdog rebuilds the pipeline.
    let mut segments_total: u64 = 0;
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

        if let Err(e) = pipeline.set_state(gst::State::Playing) {
            // StateChangeError itself is opaque — the failing element posted
            // the real reason (e.g. "Failed to bind link") on the bus, which
            // nothing was draining yet. Surface it before deciding anything.
            let bus = pipeline.bus().unwrap();
            while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
                if let MessageView::Error(err) = msg.view() {
                    eprintln!(
                        "pipeline failed to start: {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    );
                }
            }
            let _ = pipeline.set_state(gst::State::Null);
            *current_pipeline.lock().unwrap() = None;
            rebuild_attempts += 1;
            if generation == 0
                || rebuild_attempts >= MAX_REBUILD_ATTEMPTS
                || shutdown.load(Ordering::SeqCst)
            {
                // Generation 0 failing is a misconfiguration — fail fast.
                return Err(Box::new(e));
            }
            eprintln!(
                "egress-watchdog: pipeline restart failed (attempt {rebuild_attempts}/{MAX_REBUILD_ATTEMPTS}) — retrying in {}s",
                REBUILD_RETRY_PAUSE.as_secs()
            );
            std::thread::sleep(REBUILD_RETRY_PAUSE);
            continue;
        }
        rebuild_attempts = 0;
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
                                if let Some(sock) = &stats_socket {
                                    let mut v = serialize_bonding_stats(s);
                                    if use_hls_relay {
                                        v["egress"] = serde_json::json!({
                                            "segments_produced": segments_total,
                                            "wd_restarts": generation,
                                            "last_segment_age_ms":
                                                last_progress.elapsed().as_millis() as u64,
                                        });
                                    }
                                    let _ = sock.send_to(v.to_string().as_bytes(), stats_dest);
                                }
                            }
                            if s.name() == "hls-segment-added"
                                && let (Ok(location), Ok(running_time)) = (
                                    s.get::<String>("location"),
                                    s.get::<gst::ClockTime>("running-time"),
                                )
                            {
                                segments_total += 1;
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
                            segments_total += 1;
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
