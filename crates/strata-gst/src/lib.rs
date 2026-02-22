//! GStreamer plugin for Strata bonded transport.
//!
//! - `stratasink` — Sends packets via bonded Strata links with DWRR scheduling
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

    #[test]
    fn test_src_pipeline() {
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

        pipeline.set_state(gst::State::Playing).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        pipeline.set_state(gst::State::Null).unwrap();
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

    /// Validates the RTMP relay pipeline graph:
    ///   videotestsrc → x264enc → tee → queue → mpegtsmux → stratasink (Strata path)
    ///                               └→ queue → h264parse → flvmux → fakesink (RTMP path)
    ///   audiotestsrc → voaacenc → tee → queue → aacparse → mpegtsmux
    ///                               └→ queue → aacparse → flvmux
    ///
    /// Uses fakesink in place of rtmpsink to test pipeline construction
    /// and data flow without a real RTMP server.
    #[test]
    fn test_rtmp_relay_pipeline() {
        gst::init().unwrap();

        // Skip if x264enc is not available (e.g., gstreamer1.0-plugins-ugly not installed)
        if gst::ElementFactory::find("x264enc").is_none() {
            eprintln!("Skipping test_rtmp_relay_pipeline: x264enc element not available");
            return;
        }

        gst::Element::register(
            None,
            "stratasink",
            gst::Rank::NONE,
            sink::StrataSink::static_type(),
        )
        .unwrap();

        // This pipeline mirrors the strata-node sender with --relay-url:
        // - Video teed to both Strata (MPEG-TS) and RTMP (FLV) paths
        // - Audio teed similarly
        // - fakesink replaces rtmpsink since we have no live RTMP server
        let pipeline_str = "\
            videotestsrc is-live=true num-buffers=30 pattern=smpte \
            ! video/x-raw,width=320,height=240,framerate=15/1 \
            ! x264enc tune=zerolatency bitrate=500 key-int-max=30 \
            ! tee name=vtee \
            vtee. ! queue ! mux. \
            vtee. ! queue ! h264parse ! fmux. \
            audiotestsrc is-live=true num-buffers=30 wave=silence \
            ! audioconvert ! audioresample ! voaacenc bitrate=64000 \
            ! tee name=atee \
            atee. ! queue ! aacparse ! mux. \
            atee. ! queue ! aacparse ! fmux. \
            mpegtsmux name=mux alignment=7 ! stratasink name=rsink \
            flvmux name=fmux streamable=true ! fakesink sync=false";

        let pipeline = gst::parse::launch(pipeline_str)
            .expect("Failed to construct RTMP relay pipeline")
            .downcast::<gst::Pipeline>()
            .expect("Failed to cast to pipeline");

        pipeline
            .set_state(gst::State::Playing)
            .expect("Failed to set pipeline to Playing");

        let bus = pipeline.bus().unwrap();
        let start = std::time::Instant::now();
        let mut got_data = false;

        while start.elapsed() < std::time::Duration::from_secs(10) {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(..) => {
                        got_data = true;
                        break;
                    }
                    MessageView::Error(err) => {
                        panic!("RTMP relay pipeline error: {}", err.error());
                    }
                    _ => (),
                }
            }
        }

        assert!(
            got_data,
            "RTMP relay pipeline did not reach EOS within timeout"
        );

        pipeline.set_state(gst::State::Null).unwrap();
    }
}
