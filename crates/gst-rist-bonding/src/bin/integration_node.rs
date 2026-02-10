use gst::prelude::*;
use gst::MessageView;
use std::env;
use std::sync::{Arc, Mutex};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging for production use.
    // Controlled by RUST_LOG env var (e.g., RUST_LOG=info,librist=warn).
    rist_bonding_core::init();

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
    let mut config_path = "";

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
        } else if args[i] == "--config" && i + 1 < args.len() {
            config_path = &args[i + 1];
            i += 1;
        }
        i += 1;
    }

    if dest_str.is_empty() {
        eprintln!("Missing --dest <url>");
        std::process::exit(1);
    }

    register_plugins()?;

    // Pipeline: videotestsrc ! x264enc ! mpegtsmux ! rsristbondsink (request pads)
    // We send 1200 buffers (20s at 60fps) to ensure continuous flow beyond test duration.
    // VBV buffer = bitrate (1-second buffer) constrains the peak rate to ~2× target,
    // preventing unconstrained VBR overshoot with tune=zerolatency.
    let pipeline_str = format!(
        "videotestsrc num-buffers=1200 is-live=true pattern=smpte ! video/x-raw,width=1920,height=1080,framerate=60/1 ! x264enc name=enc tune=zerolatency bitrate={bps} vbv-buf-capacity={bps} ! mpegtsmux alignment=7 pat-interval=10 pmt-interval=10 ! rsristbondsink name=rsink",
        bps = bitrate_kbps
    );
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

    // Additive-increase step: ramp bitrate back up by this amount (kbps)
    // each stats interval when bandwidth-available is signalled.
    let ramp_step_kbps: u32 = (bitrate_kbps / 10).max(100);

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
                            // Update encoder: only reduce bitrate (congestion relief),
                            // never increase beyond the configured value.
                            if let Some(enc) = pipeline.by_name("enc") {
                                // x264enc bitrate is in kbit/sec
                                let recommended_kbps = (recommended / 1000) as u32;
                                let current: u32 = enc.property("bitrate");
                                let target = std::cmp::max(recommended_kbps, 500);
                                if target < current {
                                    eprintln!(
                                        "Congestion Control: Reducing Bitrate from {} to {} kbps",
                                        current, target
                                    );
                                    enc.set_property("bitrate", target);
                                }
                            }
                        }
                    } else if s.name() == "bandwidth-available" {
                        // AIMD additive increase: ramp bitrate back up
                        // towards the configured ceiling.
                        if let Some(enc) = pipeline.by_name("enc") {
                            let current: u32 = enc.property("bitrate");
                            if current < bitrate_kbps {
                                let target = std::cmp::min(current + ramp_step_kbps, bitrate_kbps);
                                eprintln!(
                                    "Bandwidth Available: Increasing Bitrate from {} to {} kbps",
                                    current, target
                                );
                                enc.set_property("bitrate", target);
                            }
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
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--bind" && i + 1 < args.len() {
            bind_str = &args[i + 1];
            i += 1;
        } else if args[i] == "--output" && i + 1 < args.len() {
            output_file = &args[i + 1];
            i += 1;
        } else if args[i] == "--config" && i + 1 < args.len() {
            config_path = &args[i + 1];
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

    // Apply TOML config file if provided
    if !config_path.is_empty() {
        if let Some(src_elem) = pipeline.by_name("src") {
            let config_toml = std::fs::read_to_string(config_path)
                .map_err(|e| format!("Failed to read config file '{}': {}", config_path, e))?;
            src_elem.set_property("config", &config_toml);
            eprintln!("Applied config from {}", config_path);
        }
    }

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
