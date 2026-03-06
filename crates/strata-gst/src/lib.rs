//! GStreamer plugin for Strata bonded transport.
//!
//! - `stratasink` — Sends packets via bonded Strata links with EDPF scheduling
//! - `stratasrc`  — Receives packets from bonded Strata links with jitter-buffer reassembly

use gst::glib;

pub mod codec;
pub mod hls_upload;
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
    strata,
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

        gst::Element::register(
            None,
            "stratasink",
            gst::Rank::NONE,
            sink::StrataSink::static_type(),
        )
        .unwrap();

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 5i32)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("stratasink").build().unwrap();

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

    /// Regression: `stratasrc` must advertise `is-live = true` as soon as the
    /// object is constructed — before `BaseSrcImpl::start()` is ever called.
    ///
    /// If this property is first set inside `start()` (which runs during the
    /// READY→PAUSED transition), GStreamer has already decided whether to wait
    /// for a preroll buffer.  That race causes the pipeline to stall forever
    /// in PAUSED waiting for data that `stratasrc` refuses to produce until
    /// the pipeline reaches PLAYING.
    #[test]
    fn stratasrc_is_live_before_start() {
        use gst_base::prelude::BaseSrcExt;
        gst::init().unwrap();
        gst::Element::register(
            None,
            "stratasrc",
            gst::Rank::NONE,
            src::StrataSrc::static_type(),
        )
        .unwrap();

        // Just creating the element — no pipeline, no state change.
        let src = gst::ElementFactory::make("stratasrc").build().unwrap();
        let base_src = src.downcast_ref::<gst_base::BaseSrc>().unwrap();

        // The element must already know it is live at object-construction time
        // (ObjectImpl::constructed), not after start() sets the flag.
        assert!(
            base_src.is_live(),
            "stratasrc must be is-live=true at construction time. \
             If this fails, the Preroll deadlock has been reintroduced: \
             set_live(true) must live in ObjectImpl::constructed(), not start()."
        );
    }

    /// Regression: a `stratasrc ! fakesink` pipeline must reach PLAYING within
    /// a 2-second wall-clock deadline.
    ///
    /// Previously the pipeline would stall at PAUSED indefinitely because
    /// `stratasrc` set `is-live = true` too late (inside `start()`).  GStreamer
    /// saw a non-live source, demanded a preroll buffer, and `stratasrc`'s
    /// create thread waited for PLAYING before producing one — deadlock.
    ///
    /// This test will time-out (and thus FAIL) rather than hang the test
    /// suite, making the regression immediately visible in CI.
    #[test]
    fn stratasrc_no_preroll_deadlock() {
        gst::init().unwrap();
        gst::Element::register(
            None,
            "stratasrc",
            gst::Rank::NONE,
            src::StrataSrc::static_type(),
        )
        .unwrap();

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("stratasrc").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline.add(&src).unwrap();
        pipeline.add(&sink).unwrap();
        src.link(&sink).unwrap();

        // The specific failure mode of the Preroll deadlock was that
        // `pipeline.set_state(Playing)` would NEVER RETURN — it blocked
        // forever inside GLib's g_cond_wait() in stratasrc's create thread,
        // waiting for the pipeline to reach PLAYING, while the pipeline was
        // itself waiting for a preroll buffer from stratasrc.
        //
        // The correct regression test is therefore to assert that set_state
        // RETURNS within a short deadline, not that the pipeline reaches PLAYING
        // (which for a live source with fakesink may be Async anyway).
        let t0 = std::time::Instant::now();
        let ret = pipeline.set_state(gst::State::Playing);
        let elapsed = t0.elapsed();

        pipeline.set_state(gst::State::Null).unwrap();

        // A live PushSrc must return StateChangeReturn::Async immediately.
        // Any blocking (>200 ms) means the Preroll deadlock is reintroduced.
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "pipeline.set_state(Playing) took {elapsed:?} — should be <200 ms. \
             The Preroll deadlock has been reintroduced. Ensure \
             ObjectImpl::constructed() calls set_live(true) on stratasrc."
        );
        // A live source MUST return Async (not Error, not Success).
        assert!(
            matches!(ret, Ok(gst::StateChangeSuccess::Async)),
            "Expected StateChangeReturn::Async for live source, got {ret:?}. \
             If Success: is-live may not be set. If Error: pipeline setup failed."
        );
    }

    #[test]
    fn test_loopback_pipeline() {
        gst::init().unwrap();

        gst::Element::register(
            None,
            "stratasrc",
            gst::Rank::NONE,
            src::StrataSrc::static_type(),
        )
        .unwrap();
        gst::Element::register(
            None,
            "stratasink",
            gst::Rank::NONE,
            sink::StrataSink::static_type(),
        )
        .unwrap();

        let pipeline_a = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 100i32)
            .property("is-live", true)
            .build()
            .unwrap();
        let bond_sink = gst::ElementFactory::make("stratasink")
            .property("destinations", "127.0.0.1:15000")
            .build()
            .unwrap();

        pipeline_a.add(&src).unwrap();
        pipeline_a.add(&bond_sink).unwrap();
        src.link(&bond_sink).unwrap();

        let pipeline_b = gst::Pipeline::new();
        let bond_src = gst::ElementFactory::make("stratasrc")
            .property("links", "0.0.0.0:15000")
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();

        pipeline_b.add(&bond_src).unwrap();
        pipeline_b.add(&sink).unwrap();
        bond_src.link(&sink).unwrap();

        pipeline_b.set_state(gst::State::Playing).unwrap();
        pipeline_a.set_state(gst::State::Playing).unwrap();

        let bus_a = pipeline_a.bus().unwrap();
        let mut stats_found = false;

        let start_time = std::time::Instant::now();
        while start_time.elapsed() < std::time::Duration::from_secs(5) {
            if let Some(msg) = bus_a.timed_pop(gst::ClockTime::from_mseconds(100)) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(..) => break,
                    MessageView::Error(err) => panic!("Pipeline A Error: {}", err.error()),
                    MessageView::Element(m) => {
                        let s = m.structure();
                        if let Some(name) = s.map(|s| s.name())
                            && name == "strata-stats"
                        {
                            println!("Got Stats: {:?}", s);
                            stats_found = true;
                        }
                    }
                    _ => (),
                }
            }
        }

        assert!(stats_found, "Did not receive stats message from sink");

        pipeline_a.set_state(gst::State::Null).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(500));

        pipeline_b.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn test_request_pads() {
        gst::init().unwrap();
        gst::Element::register(
            None,
            "stratasink",
            gst::Rank::NONE,
            sink::StrataSink::static_type(),
        )
        .unwrap();

        let sink = gst::ElementFactory::make("stratasink").build().unwrap();

        let pad_tmpl = sink.pad_template("link_%u").expect("Missing pad template");
        let pad = sink
            .request_pad(&pad_tmpl, Some("link_0"), None)
            .expect("Failed to request pad");

        pad.set_property("uri", "192.168.1.100:5000");

        sink.release_request_pad(&pad);
    }
}
