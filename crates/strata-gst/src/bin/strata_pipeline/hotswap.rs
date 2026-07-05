//! Hot-swap control: source switching, link toggling, and the Unix control
//! socket that feeds them.

use gst::prelude::*;
use std::sync::Mutex;

use crate::stats::json_to_toml;

// ── Hot-swap helpers ────────────────────────────────────────────────

/// Add a new source branch to the pipeline and link it to input-selector.
/// Returns the new sink pad on the selector that this branch feeds into.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_source_branch(
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
pub(crate) fn handle_source_switch(
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
pub(crate) fn run_control_socket(path: &str, pipeline_weak: gst::glib::WeakRef<gst::Pipeline>) {
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
pub(crate) fn handle_toggle_link(
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
