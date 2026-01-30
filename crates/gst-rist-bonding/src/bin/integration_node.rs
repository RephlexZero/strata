use gst::prelude::*;
use gst::MessageView;
use std::env;
use std::sync::{Arc, Mutex};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    gst::init()?;

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <sender|receiver> [args]", args[0]);
        std::process::exit(1);
    }

    let mode = &args[1];

    match mode.as_str() {
        "sender" => run_sender(&args[2..]),
        "receiver" => run_receiver(&args[2..]),
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
    let mut bitrate_kbps = 3000; // Default 3000kbps (3Mbps)

    let mut i = 0;
    while i < args.len() {
        if args[i] == "--dest" && i + 1 < args.len() {
            dest_str = &args[i + 1];
            i += 1;
        } else if args[i] == "--stats-dest" && i + 1 < args.len() {
            stats_dest = &args[i + 1];
            i += 1;
        } else if args[i] == "--bitrate" && i + 1 < args.len() {
            bitrate_kbps = args[i + 1].parse().unwrap_or(3000);
            i += 1;
        }
        i += 1;
    }

    if dest_str.is_empty() {
        eprintln!("Missing --dest <url>");
        std::process::exit(1);
    }

    register_plugins()?;

    // Pipeline: videotestsrc ! x264enc ! mpegtsmux ! rsristbondsink
    // We send 3000 buffers (approx 100s at 30fps).
    // Note: H264 Encoding might buffer frames, so we might not get exactly 450 out if we stop abruptly.
    // We use tune=zerolatency to minimize this.
    // Ensure PAT/PMT are sent effectively for quick lock.
    let pipeline_str = format!(
        "videotestsrc num-buffers=3000 is-live=true pattern=ball ! video/x-raw,width=320,height=240 ! x264enc name=enc tune=zerolatency bitrate={} ! mpegtsmux alignment=7 pat-interval=10 pmt-interval=10 ! rsristbondsink links=\"{}\"",
        bitrate_kbps, dest_str
    );
    eprintln!("Sender Pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "Failed to cast to pipeline")?;

    // Setup Stats Relay if requested
    let mut stats_socket = None;
    if !stats_dest.is_empty() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
        stats_socket = Some(sock);
    }

    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                eprintln!("Error: {}", err.error());
                pipeline.set_state(gst::State::Null)?;
                return Err(Box::new(err.error().clone()));
            }
            MessageView::Element(element) => {
                if let Some(s) = element.structure() {
                    if s.name() == "congestion-control" {
                        if let Ok(recommended) = s.get::<u64>("recommended-bitrate") {
                            eprintln!(
                                "Congestion Control: Adjusting Bitrate to {} bps...",
                                recommended
                            );
                            // Update encoder
                            if let Some(enc) = pipeline.by_name("enc") {
                                // x264enc bitrate is in kbit/sec
                                let bitrate_kbps = (recommended / 1000) as u32;
                                let target = std::cmp::max(bitrate_kbps, 500);
                                enc.set_property("bitrate", target);
                            }
                        }
                    } else if s.name() == "rist-bonding-stats" {
                        // Relay to UDP if configured
                        if let Some(sock) = &stats_socket {
                            // Convert GST Structure to JSON
                            // Naive manual conversion for specific fields we know
                            // Debug string
                            let _s_str = s.to_string();
                            // Better: extract fields
                            // The structure is flat: link_0_rtt, link_0_capacity...
                            // We need to parse this into the hierarchy the test expects
                            // Test expects: { "timestamp": f64, "total_capacity": f64, "links": { "0": { "loss": ... } } }

                            // Reconstruct hierarchy from the flat stats structure.
                            // The schema provides aggregate fields plus per-link metrics.
                            let schema_version = s.get::<i32>("schema_version").unwrap_or(0);
                            let stats_seq = s.get::<u64>("stats_seq").unwrap_or(0);
                            let heartbeat = s.get::<bool>("heartbeat").unwrap_or(false);
                            let mono_time_ns = s.get::<u64>("mono_time_ns").unwrap_or(0);
                            let wall_time_ms = s.get::<u64>("wall_time_ms").unwrap_or(0);
                            let total_capacity_field = s.get::<f64>("total_capacity").unwrap_or(0.0);
                            let alive_links = s.get::<u64>("alive_links").unwrap_or(0);

                            let mut total_cap = if total_capacity_field > 0.0 {
                                total_capacity_field
                            } else {
                                0.0
                            };
                            let mut links_map = serde_json::Map::new();
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs_f64();

                            // Iterate fields? GstStructure doesn't expose iter easily in bindings sometimes.
                            // But we know keys "link_0_rtt".
                            // Let's assume max 16 links and query them.
                            for i in 0..16 {
                                let prefix = format!("link_{}_", i);
                                if s.has_field(&format!("{}alive", prefix)) {
                                    let alive =
                                        s.get::<bool>(&format!("{}alive", prefix)).unwrap_or(false);
                                    let cap =
                                        s.get::<f64>(&format!("{}capacity", prefix)).unwrap_or(0.0);
                                    let rtt =
                                        s.get::<f64>(&format!("{}rtt", prefix)).unwrap_or(0.0);
                                    let loss =
                                        s.get::<f64>(&format!("{}loss", prefix)).unwrap_or(0.0);

                                    if alive && total_capacity_field == 0.0 {
                                        total_cap += cap;
                                    }

                                    let link_json = serde_json::json!({
                                        "rtt": rtt,
                                        "capacity": cap,
                                        "loss": loss,
                                        "alive": alive,
                                        "queue": 0 // unavailable
                                    });
                                    links_map.insert(i.to_string(), link_json);
                                }
                            }

                            let timestamp = if wall_time_ms > 0 {
                                wall_time_ms as f64 / 1000.0
                            } else {
                                now
                            };

                            let json_stats = serde_json::json!({
                                "schema_version": schema_version,
                                "stats_seq": stats_seq,
                                "heartbeat": heartbeat,
                                "mono_time_ns": mono_time_ns,
                                "wall_time_ms": wall_time_ms,
                                "timestamp": timestamp,
                                "total_capacity": total_cap,
                                "alive_links": alive_links,
                                "links": links_map
                            });

                            if let Ok(data) = serde_json::to_vec(&json_stats) {
                                let _ = sock.send_to(&data, stats_dest);
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
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--bind" && i + 1 < args.len() {
            bind_str = &args[i + 1];
            i += 1;
        } else if args[i] == "--output" && i + 1 < args.len() {
            output_file = &args[i + 1];
            i += 1;
        }
        i += 1;
    }

    if bind_str.is_empty() {
        eprintln!("Missing --bind <url>");
        std::process::exit(1);
    }

    register_plugins()?;

    // Pipeline:
    // If output_file is set:
    //   rsristbondsrc ! tee name=t ! queue ! appsink ... t. ! queue ! filesink location=...
    // Else:
    //   rsristbondsrc ! appsink ...

    let pipeline_str = if !output_file.is_empty() {
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

    // Setup AppSink
    let sink = pipeline.by_name("sink").expect("Sink not found");
    let appsink = sink
        .dynamic_cast::<gst_app::AppSink>()
        .expect("Sink is not appsink");

    let received_count = Arc::new(Mutex::new(0u64));
    let received_bytes = Arc::new(Mutex::new(0u64));

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

    eprintln!("Receiver running... Waiting for signal or EOS.");

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
                        eprintln!("Element Message: {}", s.to_string());
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
