//! DeliveredStream and monotonic-DTS gates (A2): pad probes that keep the
//! stream the egress re-mux sees clean under loss.

use gst::prelude::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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

pub(crate) fn install_delivered_stream_gate(
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
pub(crate) fn install_monotonic_dts_gate(pad: &gst::Pad) {
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

#[cfg(test)]
mod tests {
    use super::{MAX_FORWARD_STEP_NS, TimelineStep, timeline_step};

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
        assert!(
            timeline_step(Some(21_007_913_971), Some(22_264_073_509)) == TimelineStep::Regression
        );
        // The run-2 corrupt-PES latch: video leapt ~+107 s in one step.
        assert!(
            timeline_step(Some(227_320_000_000), Some(24_694_000_000)) == TimelineStep::WildJump
        );
        assert!(timeline_step(Some(MAX_FORWARD_STEP_NS + 1), Some(0)) == TimelineStep::WildJump);
    }
}
