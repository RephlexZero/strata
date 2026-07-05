//! Plugin registration and mpegtsmux configuration shared by the sender and
//! receiver pipelines.

use gst::prelude::*;

pub(crate) fn register_plugins() -> Result<(), gst::glib::BoolError> {
    gststrata::sink::register(None)?;
    gststrata::src::register(None)?;
    gsthlssink3::plugin_desc::plugin_register_static()?;
    Ok(())
}

/// Disable mpegtsmux skew corrections when the property is available (GStreamer ≥1.28).
///
/// In a bonding transport the receiver remuxes or writes to file, so we want to
/// preserve original timestamps rather than correcting for clock drift.
pub(crate) fn configure_mpegtsmux(pipeline: &gst::Pipeline) {
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
pub(crate) fn configure_hlssink3_muxer(hls: &gst::Element) {
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
