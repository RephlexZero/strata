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

/// Audio's tighter forward-step bound. A DTS that leaps within the wide video
/// bound but past the real playout gap-skip ceiling (≤ 3 s) is a corrupt-PES
/// value; under the shared 10 s bound it was *accepted*, poisoning the
/// watermark so every legit ~23 ms-spaced frame after it regressed and was
/// dropped — audio starved for up to 10 s while `hlssink3`'s muxer waited to
/// interleave, and the 15 s egress watchdog tore the pipeline down
/// (2026-07-11 field stream: rebuild every few minutes, ~20 s outage each).
/// 4 s = the 3 s gap-skip ceiling plus margin; a legit jump can't exceed it.
const AUDIO_MAX_FORWARD_STEP_NS: u64 = 4_000_000_000;

/// Classify a buffer's DTS against the last one emitted to the muxer.
/// `max_forward_ns` is the largest credible single-step forward move
/// (`MAX_FORWARD_STEP_NS` for video, `AUDIO_MAX_FORWARD_STEP_NS` for audio).
fn timeline_step(dts: Option<u64>, last_emitted: Option<u64>, max_forward_ns: u64) -> TimelineStep {
    match (dts, last_emitted) {
        (Some(d), Some(last)) if d < last => TimelineStep::Regression,
        (Some(d), Some(last)) if d - last > max_forward_ns => TimelineStep::WildJump,
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
            let credible = timeline_step(dts, st.last_dts, MAX_FORWARD_STEP_NS) == TimelineStep::Ok;
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
        match timeline_step(dts, st.last_dts, MAX_FORWARD_STEP_NS) {
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

/// A wild-forward run this long, with each rejected DTS advancing on the one
/// before it, is a real resumed timeline (random corrupt-PES values are not
/// self-consistent at audio-frame spacing) — adopt it instead of starving the
/// muxer until the watchdog fires.
const AUDIO_RELATCH_SPAN_NS: u64 = 1_000_000_000;

/// Maximum spacing between consecutive rejected DTSes still considered one
/// self-consistent run (a few dropped frames within the run are fine; audio
/// frames are ~23 ms apart).
const AUDIO_RELATCH_MAX_GAP_NS: u64 = 1_000_000_000;

/// Never adopt a wild timeline further than this ahead of the watermark. A
/// jump past it is a corrupt demux latch (the +107 s class); if the true
/// timeline later snaps back, every legit frame would regress against the
/// adopted watermark and audio would starve for the full offset — worse than
/// letting the egress watchdog rebuild.
const AUDIO_RELATCH_MAX_JUMP_NS: u64 = 30_000_000_000;

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
///
/// Two bounded self-heal paths (2026-07-11: audio starvation was tripping the
/// 15 s egress watchdog every few minutes):
/// - a *regression* wait is bounded by [`AUDIO_MAX_FORWARD_STEP_NS`] — a
///   poisoned watermark can be at most 4 s ahead, and media time catches up
///   in real time;
/// - a sustained, self-consistent *wild-forward* run re-latches after
///   [`AUDIO_RELATCH_SPAN_NS`] (forward of the watermark, so emitting it can
///   never feed the muxer a backwards DTS).
struct AudioGate {
    last_dts: Option<u64>,
    dropped: u64,
    /// First and last rejected DTS of the current self-consistent
    /// wild-forward run.
    wild_run: Option<(u64, u64)>,
}

enum AudioGateAction {
    Emit,
    /// Emit and move the watermark to a re-latched forward timeline.
    Relatch,
    Drop,
}

impl AudioGate {
    fn new() -> Self {
        Self {
            last_dts: None,
            dropped: 0,
            wild_run: None,
        }
    }

    fn on_buffer(&mut self, dts: Option<u64>) -> AudioGateAction {
        match timeline_step(dts, self.last_dts, AUDIO_MAX_FORWARD_STEP_NS) {
            TimelineStep::Ok => {
                self.wild_run = None;
                if dts.is_some() {
                    self.last_dts = dts;
                }
                AudioGateAction::Emit
            }
            TimelineStep::Regression => {
                self.wild_run = None;
                self.dropped += 1;
                AudioGateAction::Drop
            }
            TimelineStep::WildJump => {
                let d = dts.expect("WildJump implies Some(dts)");
                let watermark = self.last_dts.expect("WildJump implies a watermark");
                let extends_run = matches!(
                    self.wild_run,
                    Some((_, last)) if d >= last && d - last <= AUDIO_RELATCH_MAX_GAP_NS
                );
                if extends_run {
                    let (first, _) = self.wild_run.unwrap();
                    if d - first >= AUDIO_RELATCH_SPAN_NS
                        && first - watermark <= AUDIO_RELATCH_MAX_JUMP_NS
                    {
                        self.wild_run = None;
                        self.last_dts = Some(d);
                        return AudioGateAction::Relatch;
                    }
                    self.wild_run = Some((first, d));
                } else {
                    self.wild_run = Some((d, d));
                }
                self.dropped += 1;
                AudioGateAction::Drop
            }
        }
    }
}

pub(crate) fn install_monotonic_dts_gate(pad: &gst::Pad) {
    use std::sync::Mutex;
    let state = Mutex::new(AudioGate::new());
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        let Some(gst::PadProbeData::Buffer(buf)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let dts = buf.dts().map(|t| t.nseconds());
        let mut st = state.lock().unwrap();
        match st.on_buffer(dts) {
            AudioGateAction::Emit => gst::PadProbeReturn::Ok,
            AudioGateAction::Relatch => {
                eprintln!(
                    "Monotonic-DTS gate (audio): re-latched onto sustained forward timeline at dts={:?} (dropped {} total)",
                    dts, st.dropped
                );
                gst::PadProbeReturn::Ok
            }
            AudioGateAction::Drop => {
                if st.dropped.is_power_of_two() {
                    eprintln!(
                        "Monotonic-DTS gate (audio): dropped {} non-credible buffer(s) (dts={:?}, last={:?})",
                        st.dropped, dts, st.last_dts
                    );
                }
                gst::PadProbeReturn::Drop
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        AUDIO_MAX_FORWARD_STEP_NS, AUDIO_RELATCH_MAX_JUMP_NS, AudioGate, AudioGateAction,
        MAX_FORWARD_STEP_NS, TimelineStep, timeline_step,
    };

    #[test]
    fn timeline_step_classifies_all_regimes() {
        // No baseline yet, or no DTS on the buffer → always credible.
        assert!(timeline_step(None, None, MAX_FORWARD_STEP_NS) == TimelineStep::Ok);
        assert!(timeline_step(Some(5), None, MAX_FORWARD_STEP_NS) == TimelineStep::Ok);
        assert!(timeline_step(None, Some(5), MAX_FORWARD_STEP_NS) == TimelineStep::Ok);
        // Normal forward motion, including a playout-window-sized gap-skip.
        assert!(timeline_step(Some(1_000), Some(999), MAX_FORWARD_STEP_NS) == TimelineStep::Ok);
        assert!(
            timeline_step(Some(3_000_000_000), Some(0), MAX_FORWARD_STEP_NS) == TimelineStep::Ok
        );
        assert!(
            timeline_step(Some(MAX_FORWARD_STEP_NS), Some(0), MAX_FORWARD_STEP_NS)
                == TimelineStep::Ok,
            "exactly at the bound is still credible"
        );
        // Backwards → the mpegtsmux-fatal case (2026-07-04 run 1).
        assert!(
            timeline_step(
                Some(21_007_913_971),
                Some(22_264_073_509),
                MAX_FORWARD_STEP_NS
            ) == TimelineStep::Regression
        );
        // The run-2 corrupt-PES latch: video leapt ~+107 s in one step.
        assert!(
            timeline_step(
                Some(227_320_000_000),
                Some(24_694_000_000),
                MAX_FORWARD_STEP_NS
            ) == TimelineStep::WildJump
        );
        assert!(
            timeline_step(Some(MAX_FORWARD_STEP_NS + 1), Some(0), MAX_FORWARD_STEP_NS)
                == TimelineStep::WildJump
        );
        // Audio's tighter bound: a +5 s step is wild for audio, fine for video.
        assert!(
            timeline_step(Some(5_000_000_000), Some(0), AUDIO_MAX_FORWARD_STEP_NS)
                == TimelineStep::WildJump
        );
        assert!(
            timeline_step(Some(5_000_000_000), Some(0), MAX_FORWARD_STEP_NS) == TimelineStep::Ok
        );
    }

    const FRAME_NS: u64 = 23_000_000; // ~one AAC frame at 44.1 kHz

    /// Feed a monotonic run starting at `start`, return how many buffers were
    /// dropped before the gate emitted (relatch or plain emit).
    fn feed_until_emitted(gate: &mut AudioGate, start: u64) -> u64 {
        for i in 0..200 {
            let dts = start + i * FRAME_NS;
            match gate.on_buffer(Some(dts)) {
                AudioGateAction::Emit | AudioGateAction::Relatch => return i,
                AudioGateAction::Drop => {}
            }
        }
        panic!("gate never emitted within 200 frames");
    }

    /// The 2026-07-11 stall: a legit timeline jump past the audio bound used
    /// to starve the muxer until the 15 s watchdog. Now a self-consistent run
    /// re-latches within ~1 s of frames.
    #[test]
    fn audio_gate_relatches_onto_sustained_forward_timeline() {
        let mut gate = AudioGate::new();
        assert!(matches!(
            gate.on_buffer(Some(1_000_000_000)),
            AudioGateAction::Emit
        ));
        // Timeline resumes +6 s ahead (legit long-outage skip, > 4 s bound).
        let dropped = feed_until_emitted(&mut gate, 7_000_000_000);
        let span_frames = super::AUDIO_RELATCH_SPAN_NS / FRAME_NS + 1;
        assert!(
            (1..=span_frames + 1).contains(&dropped),
            "expected ~1 s of drops before re-latch, got {dropped}"
        );
        // After the re-latch the new timeline flows.
        assert!(matches!(
            gate.on_buffer(Some(7_000_000_000 + 200 * FRAME_NS)),
            AudioGateAction::Emit
        ));
    }

    /// Random corrupt-PES DTS values are not self-consistent — no re-latch.
    #[test]
    fn audio_gate_does_not_relatch_onto_garbage() {
        let mut gate = AudioGate::new();
        assert!(matches!(
            gate.on_buffer(Some(1_000_000_000)),
            AudioGateAction::Emit
        ));
        // Scattered wild values: forward but mutually inconsistent (> 1 s apart).
        for d in [
            9_000_000_000u64,
            26_000_000_000,
            12_000_000_000,
            48_000_000_000,
        ] {
            assert!(matches!(gate.on_buffer(Some(d)), AudioGateAction::Drop));
        }
        // The real timeline is still accepted.
        assert!(matches!(
            gate.on_buffer(Some(1_000_000_000 + FRAME_NS)),
            AudioGateAction::Emit
        ));
    }

    /// A +107 s-class corrupt latch must never be adopted: if the true
    /// timeline snapped back afterwards, audio would starve for the full
    /// offset. Bounded by AUDIO_RELATCH_MAX_JUMP_NS.
    #[test]
    fn audio_gate_never_adopts_a_far_corrupt_latch() {
        let mut gate = AudioGate::new();
        assert!(matches!(
            gate.on_buffer(Some(1_000_000_000)),
            AudioGateAction::Emit
        ));
        let far = 1_000_000_000 + AUDIO_RELATCH_MAX_JUMP_NS + 1_000_000_000;
        for i in 0..100 {
            assert!(
                matches!(
                    gate.on_buffer(Some(far + i * FRAME_NS)),
                    AudioGateAction::Drop
                ),
                "far latch must keep dropping (frame {i})"
            );
        }
        // The true timeline resumes and is accepted immediately.
        assert!(matches!(
            gate.on_buffer(Some(1_000_000_000 + FRAME_NS)),
            AudioGateAction::Emit
        ));
    }

    /// A poisoned watermark (corrupt DTS accepted inside the 4 s bound) is a
    /// bounded regression wait: media time catches up within the poison
    /// magnitude, and the watermark is never moved backwards.
    #[test]
    fn audio_gate_regression_wait_is_bounded_by_forward_cap() {
        let mut gate = AudioGate::new();
        assert!(matches!(
            gate.on_buffer(Some(10_000_000_000)),
            AudioGateAction::Emit
        ));
        // Corrupt-but-plausible poison: +3.9 s, inside the 4 s bound → accepted.
        assert!(matches!(
            gate.on_buffer(Some(13_900_000_000)),
            AudioGateAction::Emit
        ));
        // The real timeline continues from ~10 s: regressions, dropped.
        let dropped = feed_until_emitted(&mut gate, 10_000_000_000 + FRAME_NS);
        let max_frames = AUDIO_MAX_FORWARD_STEP_NS / FRAME_NS + 2;
        assert!(
            dropped <= max_frames,
            "regression wait must resolve within the 4 s cap, got {dropped} frames"
        );
    }
}
