//! GStreamer plugin providing RIST bonding sink and source elements.
//!
//! - `rsristbondsink` — Sends packets via bonded RIST links with DWRR scheduling
//! - `rsristbondsrc` — Receives packets from bonded RIST links with jitter-buffer reassembly

use gst::glib;

pub mod pad;
pub mod sink;
pub mod src;
mod util;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    sink::register(Some(plugin))?;
    src::register(Some(plugin))?;
    Ok(())
}

gst::plugin_define!(
    rsristbonding,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "LGPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);

#[cfg(test)]
mod tests {
    use super::*;
    use gst::prelude::*;

    #[test]
    fn test_sink_pipeline() {
        gst::init().unwrap();

        // Register manually for testing without loading the .so
        gst::Element::register(
            None,
            "rsristbondsink",
            gst::Rank::NONE,
            sink::RsRistBondSink::static_type(),
        )
        .unwrap();

        // Manual pipeline construction
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 5i32) // Explicit type for property
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        pipeline.add(&src).unwrap();
        pipeline.add(&sink).unwrap();
        src.link(&sink).unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();

        let bus = pipeline.bus().unwrap();
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    panic!("Error: {}", err.error());
                }
                _ => (),
            }
        }

        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn test_src_pipeline() {
        gst::init().unwrap();

        gst::Element::register(
            None,
            "rsristbondsrc",
            gst::Rank::NONE,
            src::RsRistBondSrc::static_type(),
        )
        .unwrap();

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("rsristbondsrc").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();

        pipeline.add(&src).unwrap();
        pipeline.add(&sink).unwrap();
        src.link(&sink).unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();

        // Run for a short time then stop
        std::thread::sleep(std::time::Duration::from_millis(100));

        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn test_loopback_pipeline() {
        gst::init().unwrap();

        gst::Element::register(
            None,
            "rsristbondsrc",
            gst::Rank::NONE,
            src::RsRistBondSrc::static_type(),
        )
        .unwrap();
        gst::Element::register(
            None,
            "rsristbondsink",
            gst::Rank::NONE,
            sink::RsRistBondSink::static_type(),
        )
        .unwrap();

        // Pipeline A: src -> bond_sink
        let pipeline_a = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 100i32)
            .property("is-live", true) // Pace the buffers so we run long enough for stats
            .build()
            .unwrap();
        let bond_sink = gst::ElementFactory::make("rsristbondsink")
            .property("links", "rist://127.0.0.1:15000") // Use high port
            .build()
            .unwrap();

        pipeline_a.add(&src).unwrap();
        pipeline_a.add(&bond_sink).unwrap();
        src.link(&bond_sink).unwrap();

        // Pipeline B: bond_src -> sink
        let pipeline_b = gst::Pipeline::new();
        let bond_src = gst::ElementFactory::make("rsristbondsrc")
            .property("links", "rist://@0.0.0.0:15000")
            //.property("stats-interval", 100u32) // If we exposed this...
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();

        pipeline_b.add(&bond_src).unwrap();
        pipeline_b.add(&sink).unwrap();
        bond_src.link(&sink).unwrap();

        // Start Receiver first
        pipeline_b.set_state(gst::State::Playing).unwrap();
        // Start Sender
        pipeline_a.set_state(gst::State::Playing).unwrap();

        // Wait for A to finish
        let bus_a = pipeline_a.bus().unwrap();
        let mut stats_found = false;

        // Wait up to 5 seconds
        let start_time = std::time::Instant::now();
        while start_time.elapsed() < std::time::Duration::from_secs(5) {
            if let Some(msg) = bus_a.timed_pop(gst::ClockTime::from_mseconds(100)) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(..) => break,
                    MessageView::Error(err) => panic!("Pipeline A Error: {}", err.error()),
                    MessageView::Element(m) => {
                        let s = m.structure();
                        if let Some(name) = s.map(|s| s.name()) {
                            if name == "rist-bonding-stats" {
                                println!("Got Stats: {:?}", s);
                                stats_found = true;
                            }
                        }
                    }
                    _ => (),
                }
            }
        }

        // Check stats
        assert!(stats_found, "Did not receive stats message from sink");

        // Stop A
        pipeline_a.set_state(gst::State::Null).unwrap();

        // Wait for B to receive
        // We can't easily count received buffers in fakesink without a signal or pad probe,
        // but if it didn't crash, it's a good sign.
        // Let's attach a pad probe to count?
        // For now, just ensuring it runs and stops cleanly is enough Integration Test Level 1.
        std::thread::sleep(std::time::Duration::from_millis(500));

        pipeline_b.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn test_request_pads() {
        gst::init().unwrap();
        gst::Element::register(
            None,
            "rsristbondsink",
            gst::Rank::NONE,
            sink::RsRistBondSink::static_type(),
        )
        .unwrap();

        let sink = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        let pad_tmpl = sink.pad_template("link_%u").expect("Missing pad template");
        let pad = sink
            .request_pad(&pad_tmpl, Some("link_0"), None)
            .expect("Failed to request pad");

        pad.set_property("uri", "rist://127.0.0.1:5000");

        // Trigger property notify
        // In a real pipeline, loop runs. Here we just set it.
        // Our connect_notify handler should fire synchronously?
        // Yes, notify signals are usually synchronous.

        // How to verify?
        // We can't inspect active_links easily.
        // We assume it worked if no panic.

        sink.release_request_pad(&pad);
    }

    // ──────────────────────────────────────────────────────────────────
    // Tier 1 — Element factory tests (gst::init + factory, no pipeline)
    // ──────────────────────────────────────────────────────────────────

    /// Helper: ensure both element types are registered for the current process.
    fn ensure_elements_registered() {
        gst::init().unwrap();
        let _ = gst::Element::register(
            None,
            "rsristbondsink",
            gst::Rank::NONE,
            sink::RsRistBondSink::static_type(),
        );
        let _ = gst::Element::register(
            None,
            "rsristbondsrc",
            gst::Rank::NONE,
            src::RsRistBondSrc::static_type(),
        );
    }

    #[test]
    fn test_sink_property_roundtrip() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        // links — default empty, set & read back
        let links_default: String = elem.property("links");
        assert_eq!(links_default, "");
        elem.set_property("links", "rist://1.2.3.4:5000,rist://5.6.7.8:6000");
        let links: String = elem.property("links");
        assert_eq!(links, "rist://1.2.3.4:5000,rist://5.6.7.8:6000");

        // max-bitrate — default 0, set & read back
        let max_br_default: u64 = elem.property("max-bitrate");
        assert_eq!(max_br_default, 0);
        elem.set_property("max-bitrate", 50_000_000u64);
        let max_br: u64 = elem.property("max-bitrate");
        assert_eq!(max_br, 50_000_000);

        // config — default empty, set & read back
        let cfg_default: String = elem.property("config");
        assert_eq!(cfg_default, "");
    }

    #[test]
    fn test_src_property_roundtrip() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsrc").build().unwrap();

        // links — default empty
        let links_default: String = elem.property("links");
        assert_eq!(links_default, "");
        elem.set_property("links", "rist://@0.0.0.0:5000");
        let links: String = elem.property("links");
        assert_eq!(links, "rist://@0.0.0.0:5000");

        // latency — default 50
        let latency_default: u32 = elem.property("latency");
        assert_eq!(latency_default, 50);
        elem.set_property("latency", 200u32);
        let latency: u32 = elem.property("latency");
        assert_eq!(latency, 200);

        // config — default empty
        let cfg_default: String = elem.property("config");
        assert_eq!(cfg_default, "");
    }

    #[test]
    fn test_sink_config_toml_applies() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        let toml = r#"
            version = 1
            [[links]]
            id = 0
            uri = "rist://10.0.0.1:5000"
            [scheduler]
            stats_interval_ms = 500
        "#;
        elem.set_property("config", toml);

        // config property stores the raw TOML
        let stored: String = elem.property("config");
        assert_eq!(stored, toml);
    }

    #[test]
    fn test_src_config_toml_applies_latency() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsrc").build().unwrap();

        // Verify default latency
        assert_eq!(elem.property::<u32>("latency"), 50);

        let toml = r#"
            version = 1
            [receiver]
            start_latency_ms = 150
        "#;
        elem.set_property("config", toml);

        // apply_config_toml should have overridden the latency setting
        let latency: u32 = elem.property("latency");
        assert_eq!(latency, 150, "config TOML should override latency");
    }

    #[test]
    fn test_sink_config_file_rejects_traversal() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        elem.set_property("config-file", "../../etc/passwd");
        // The path-traversal guard should reject it — config stays empty
        let cfg: String = elem.property("config");
        assert_eq!(cfg, "", "path traversal should be rejected");
    }

    #[test]
    fn test_src_config_file_rejects_traversal() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsrc").build().unwrap();

        elem.set_property("config-file", "../../../etc/shadow");
        let cfg: String = elem.property("config");
        assert_eq!(cfg, "", "path traversal should be rejected on source");
    }

    #[test]
    fn test_sink_config_file_nonexistent() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        // Should not panic when file doesn't exist — just warns
        elem.set_property("config-file", "/tmp/does_not_exist_rist_test_98765.toml");
        let cfg: String = elem.property("config");
        assert_eq!(cfg, "", "nonexistent file should leave config empty");
    }

    #[test]
    fn test_src_config_file_nonexistent() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsrc").build().unwrap();

        elem.set_property("config-file", "/tmp/does_not_exist_rist_test_98765.toml");
        let cfg: String = elem.property("config");
        assert_eq!(cfg, "", "nonexistent file should leave config empty");
    }

    #[test]
    fn test_sink_invalid_config_no_panic() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        // Invalid TOML should not panic — apply_config warns and returns
        elem.set_property("config", "not valid toml {{{[");
        // The raw string is still stored (set_property stores before apply)
        let cfg: String = elem.property("config");
        assert_eq!(cfg, "not valid toml {{{[");
    }

    #[test]
    fn test_src_invalid_config_no_panic() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsrc").build().unwrap();

        elem.set_property("config", "<<<garbage>>>");
        // Source stores via apply_config_toml which only stores on success,
        // so config should remain empty on parse failure
        let cfg: String = elem.property("config");
        assert_eq!(cfg, "", "invalid TOML should not be stored on source");
    }

    #[test]
    fn test_sink_element_metadata() {
        ensure_elements_registered();
        let factory = gst::ElementFactory::find("rsristbondsink").unwrap();
        assert_eq!(factory.metadata("long-name").unwrap(), "RIST Bonding Sink");
        assert_eq!(factory.metadata("klass").unwrap(), "Sink/Network");
        assert!(factory.metadata("description").unwrap().contains("RIST"));
    }

    #[test]
    fn test_src_element_metadata() {
        ensure_elements_registered();
        let factory = gst::ElementFactory::find("rsristbondsrc").unwrap();
        assert_eq!(
            factory.metadata("long-name").unwrap(),
            "RIST Bonding Source"
        );
        assert_eq!(factory.metadata("klass").unwrap(), "Source/Network");
        assert!(factory.metadata("description").unwrap().contains("RIST"));
    }

    #[test]
    fn test_sink_pad_templates() {
        ensure_elements_registered();
        let factory = gst::ElementFactory::find("rsristbondsink").unwrap();
        let templates = factory.static_pad_templates();
        assert_eq!(templates.len(), 2, "sink should have 2 pad templates");

        // Find the always-present sink pad
        let sink_tmpl = templates
            .iter()
            .find(|t| t.name_template() == "sink")
            .unwrap();
        assert_eq!(sink_tmpl.direction(), gst::PadDirection::Sink);
        assert_eq!(sink_tmpl.presence(), gst::PadPresence::Always);

        // Find the request pad template
        let link_tmpl = templates
            .iter()
            .find(|t| t.name_template() == "link_%u")
            .unwrap();
        assert_eq!(link_tmpl.direction(), gst::PadDirection::Src);
        assert_eq!(link_tmpl.presence(), gst::PadPresence::Request);
    }

    #[test]
    fn test_src_pad_templates() {
        ensure_elements_registered();
        let factory = gst::ElementFactory::find("rsristbondsrc").unwrap();
        let templates = factory.static_pad_templates();
        assert_eq!(templates.len(), 1, "source should have 1 pad template");

        let src_tmpl = templates.iter().next().unwrap();
        assert_eq!(src_tmpl.name_template(), "src");
        assert_eq!(src_tmpl.direction(), gst::PadDirection::Src);
        assert_eq!(src_tmpl.presence(), gst::PadPresence::Always);
    }

    #[test]
    fn test_request_pad_lifecycle_multiple() {
        ensure_elements_registered();
        let sink = gst::ElementFactory::make("rsristbondsink").build().unwrap();
        let tmpl = sink.pad_template("link_%u").unwrap();

        // Request 3 pads with explicit names
        let pad0 = sink.request_pad(&tmpl, Some("link_0"), None).unwrap();
        let pad1 = sink.request_pad(&tmpl, Some("link_1"), None).unwrap();
        let pad2 = sink.request_pad(&tmpl, Some("link_2"), None).unwrap();

        // Should have 4 pads total: 1 always (sink) + 3 request
        assert_eq!(sink.pads().len(), 4);

        // Set URIs
        pad0.set_property("uri", "rist://10.0.0.1:5000");
        pad1.set_property("uri", "rist://10.0.0.2:5000");
        pad2.set_property("uri", "rist://10.0.0.3:5000");

        // Release all
        sink.release_request_pad(&pad0);
        sink.release_request_pad(&pad1);
        sink.release_request_pad(&pad2);

        // Should be back to 1 pad (the always-present sink)
        assert_eq!(sink.pads().len(), 1);
    }

    #[test]
    fn test_request_pad_auto_naming() {
        ensure_elements_registered();
        let sink = gst::ElementFactory::make("rsristbondsink").build().unwrap();
        let tmpl = sink.pad_template("link_%u").unwrap();

        // Request pads without specifying names — the element's request_new_pad
        // auto-assigns link_0, link_1, etc. by scanning pad_map.
        let pad_a = sink.request_pad(&tmpl, None, None).unwrap();
        let name_a = pad_a.name().to_string();
        assert!(
            name_a.starts_with("link_"),
            "auto-named pad should start with link_: got {}",
            name_a
        );

        // Second pad — must get a different name
        let pad_b = sink.request_pad(&tmpl, None, None).unwrap();
        let name_b = pad_b.name().to_string();
        assert!(
            name_b.starts_with("link_"),
            "auto-named pad should start with link_: got {}",
            name_b
        );
        assert_ne!(name_a, name_b, "auto-named pads should have unique names");

        sink.release_request_pad(&pad_a);
        sink.release_request_pad(&pad_b);
    }

    #[test]
    fn test_sink_empty_links_no_crash() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        // Empty string
        elem.set_property("links", "");
        let links: String = elem.property("links");
        assert_eq!(links, "");

        // Commas only — should not crash
        elem.set_property("links", ",,,,");
        let links: String = elem.property("links");
        assert_eq!(links, ",,,,");
    }

    #[test]
    fn test_sink_pad_uri_property() {
        ensure_elements_registered();
        let sink = gst::ElementFactory::make("rsristbondsink").build().unwrap();
        let tmpl = sink.pad_template("link_%u").unwrap();

        let pad = sink.request_pad(&tmpl, Some("link_0"), None).unwrap();

        // Default URI should be empty
        let uri_default: String = pad.property("uri");
        assert_eq!(uri_default, "");

        // Set and read back
        pad.set_property("uri", "rist://192.168.1.100:7000");
        let uri: String = pad.property("uri");
        assert_eq!(uri, "rist://192.168.1.100:7000");

        // Update URI
        pad.set_property("uri", "rist://10.0.0.1:8000");
        let uri: String = pad.property("uri");
        assert_eq!(uri, "rist://10.0.0.1:8000");

        sink.release_request_pad(&pad);
    }

    #[test]
    fn test_sink_max_bitrate_default_and_live_update() {
        ensure_elements_registered();
        let elem = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        // Default is 0 (disabled)
        assert_eq!(elem.property::<u64>("max-bitrate"), 0);

        // Can be set while in NULL state
        elem.set_property("max-bitrate", 100_000_000u64);
        assert_eq!(elem.property::<u64>("max-bitrate"), 100_000_000);

        // Set back to 0 (disabled)
        elem.set_property("max-bitrate", 0u64);
        assert_eq!(elem.property::<u64>("max-bitrate"), 0);
    }

    // ──────────────────────────────────────────────────────────────────
    // Tier 3 — Minimal pipeline tests (appsrc/fakesrc, no network)
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_buffer_flags_to_profile() {
        ensure_elements_registered();

        // appsrc ! rsristbondsink — push buffers with various flag combos
        let pipeline = gst::Pipeline::new();
        let appsrc = gst::ElementFactory::make("appsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        pipeline.add(&appsrc).unwrap();
        pipeline.add(&sink).unwrap();
        appsrc.link(&sink).unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();

        let appsrc = appsrc.dynamic_cast::<gst_app::AppSrc>().unwrap();

        // Helper to push a buffer with given flags
        let push = |flags: gst::BufferFlags| {
            let mut buf = gst::Buffer::with_size(188).unwrap();
            {
                let buf_ref = buf.get_mut().unwrap();
                buf_ref.set_flags(flags);
            }
            appsrc.push_buffer(buf)
        };

        // 1. No flags (keyframe / non-delta) — critical, not droppable
        assert_eq!(push(gst::BufferFlags::empty()), Ok(gst::FlowSuccess::Ok));

        // 2. DELTA_UNIT only — not critical, not droppable
        assert_eq!(push(gst::BufferFlags::DELTA_UNIT), Ok(gst::FlowSuccess::Ok));

        // 3. HEADER — critical regardless of DELTA_UNIT
        assert_eq!(push(gst::BufferFlags::HEADER), Ok(gst::FlowSuccess::Ok));

        // 4. DELTA_UNIT | HEADER — critical (header overrides delta)
        assert_eq!(
            push(gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::HEADER),
            Ok(gst::FlowSuccess::Ok)
        );

        // 5. DROPPABLE — can_drop = true
        assert_eq!(push(gst::BufferFlags::DROPPABLE), Ok(gst::FlowSuccess::Ok));

        // 6. DELTA_UNIT | DROPPABLE — non-critical + droppable (B-frame)
        assert_eq!(
            push(gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::DROPPABLE),
            Ok(gst::FlowSuccess::Ok)
        );

        appsrc.end_of_stream().unwrap();

        // Wait for EOS
        let bus = pipeline.bus().unwrap();
        for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => panic!("Pipeline error: {}", err.error()),
                _ => (),
            }
        }

        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn test_sink_stop_without_start() {
        ensure_elements_registered();
        let sink = gst::ElementFactory::make("rsristbondsink").build().unwrap();

        // NULL → READY (does not call BaseSink::start)
        sink.set_state(gst::State::Ready).unwrap();
        // READY → NULL (calls stop — stats thread was never spawned)
        sink.set_state(gst::State::Null).unwrap();
        // Should not panic — the stop() impl handles None stats_thread gracefully
    }

    #[test]
    fn test_src_stop_without_start() {
        ensure_elements_registered();
        let src = gst::ElementFactory::make("rsristbondsrc").build().unwrap();

        // NULL → READY → NULL without going to PLAYING
        src.set_state(gst::State::Ready).unwrap();
        src.set_state(gst::State::Null).unwrap();
        // No panic from stopping a receiver that was never created
    }
}
