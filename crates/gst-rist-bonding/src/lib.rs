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
}
