# Research Questions for Strata Transport

> These are specific research prompts to inform the next phase of development.
> Each question targets a gap between our current implementation and the
> production-grade bonded cellular transport described in the master plan.

---

## 1. BBR-Style Bandwidth Probing for Bonded UDP Links

How should we probe for true per-link capacity without a TCP-like congestion
window?

**Context:** Our current capacity estimate uses a Mathis-formula approximation
(`1.3 × MSS × 8 / (RTT × √loss)`) which gives *theoretical* numbers unrelated
to the actual bottleneck bandwidth. The tc netem rate limits in our simulation
are 8/5/6 Mbps per link, but the Mathis estimate doesn't know about these — it
just sees RTT and loss.

**Specific questions:**
- How do LiveU, TVU, and Dejero measure per-link capacity on cellular uplinks
  where the bottleneck is the radio scheduler (not a router buffer)?
- Should we use ACK-clock (delivered bytes / RTT), delay-gradient (like
  COPA/Vegas), or explicit probing bursts?
- BBRv3 models BtlBw and RTprop — can we run a stripped-down BBR state machine
  per link inside the bonding scheduler, using ACK timestamps?
- How does the BBR PROBE_BW phase interact with bonded scheduling? If one link
  is probing while others are stable, does the scheduler need to know?
- What's the minimum probe duration needed to get a useful capacity estimate on
  LTE/5G uplinks (where the radio scheduler grants resources in 1ms TTIs)?

**Desired output:** A capacity estimation algorithm we can implement in
`TransportLink::get_metrics()` that gives genuinely different, accurate capacity
numbers for links with different rate limits.

---

## 2. x264enc / x265enc / AV1 Dynamic Bitrate Behavior with GStreamer

When we change `bitrate` on a running encoder element, what actually happens?

**Context:** We currently set `enc.set_property("bitrate", target)` on a live
x264enc element. The ramp works, but we don't know the failure modes.

**Specific questions for x264enc:**
- Does a bitrate change take effect immediately or only at the next keyframe?
- Does `vbv-buf-capacity` need to track `bitrate`? What happens if we set
  bitrate to 8000 but vbv-buf-capacity is still 1000 from the initial config?
- What are the valid ranges for bitrate and vbv-buf-capacity?
- What happens if we ramp too fast — does the encoder produce bursts that
  exceed the link capacity? How does VBV interact with our ramp step size?
- Is there a GStreamer-standard way to signal bitrate changes (e.g.,
  `GstEncoderBitrateProfile`, renegotiation events)?

**Specific questions for x265enc (HEVC):**
- Does GStreamer's `x265enc` support dynamic bitrate changes on a live pipeline?
- What properties control it? (`bitrate`, `vbv-bufsize`, `vbv-maxrate`?)
- Does x265enc support `tune=zerolatency` equivalent for live streaming?
- What's the quality-per-bit advantage over x264enc at our target bitrates
  (1-15 Mbps for 720p/1080p)?

**Specific questions for AV1 (svtav1enc / rav1e / av1enc):**
- Which GStreamer AV1 encoder element is production-ready for live streaming?
- Does `svtav1enc` support dynamic bitrate changes? What properties?
- What's the encoding latency for real-time AV1 at 1080p30? Is it feasible on
  our target hardware (commodity ARM64, RPi5, Jetson)?
- AV1 has built-in SVC (Scalable Video Coding) — how does GStreamer expose
  temporal/spatial layers? Can we route layers independently through the
  bonding scheduler?

**Desired output:** A codec abstraction layer that works with x264enc, x265enc,
and svtav1enc, with known-safe dynamic bitrate change procedures for each.

---

## 3. Closed-Loop Encoder Adaptation for Bonded Cellular

The master plan specifies a `BITRATE_CMD` control packet (subtype 0x05). Who
computes the target bitrate — the sender or the receiver?

**Context:** Currently the sender computes bitrate from its own capacity
estimates (via the stats thread). But the receiver sees the *actual delivered
goodput* — it knows what arrived, what was lost, what was recovered by FEC, and
what the actual end-to-end delay is. The `BitrateAdapter` in `adaptation.rs` is
fully implemented but not wired into the pipeline.

**Specific questions:**
- LiveU and TVU both use receiver-side bitrate estimation — is that the right
  model for us? What information does the receiver have that the sender doesn't?
- Should the receiver compute a "safe bitrate" and send it back via
  `BITRATE_CMD`, which the sender uses as a ceiling?
- Or should the receiver send raw metrics (goodput, FEC repair rate, jitter
  buffer health) and let the sender's `BitrateAdapter` make the decision?
- How does the `BitrateAdapter`'s degradation stage system (Normal → DropB →
  ReduceBitrate → KeyframeOnly → Emergency) interact with the GStreamer
  pipeline? Which stages require encoder changes vs. scheduler changes?
- NADA (RFC 8698) defines `r_ref` (reference rate from network) and `r_vin`
  (video input rate). Our stats thread computes `aggregate_nada_ref_bps` — is
  the NADA model the right framework, or should we use something simpler?

**Desired output:** A clear decision on sender-side vs. receiver-side bitrate
computation, and a wiring plan to connect `BitrateAdapter` into the GStreamer
message loop as the single source of truth.

---

## 4. Integration Testing Strategies for Real-Time Video Transport

What metrics should an automated test measure to assert "the system is working
well"?

**Context:** We currently test by streaming to YouTube and eyeballing the
dashboard. We need automated tests that run the full GStreamer pipeline locally
(sender → bonded links with tc netem → receiver), measure actual behavior, and
assert on outcomes.

**Specific questions:**
- What metrics can we measure without decoding the video? (Throughput, packet
  delivery ratio, time-to-first-frame, keyframe interval stability, bitrate
  ramp time, degradation recovery time, max loss burst survived.)
- What metrics require decode? (VMAF, SSIM, PSNR.) Is it worth adding a decode
  step to CI tests, or should visual quality be a separate manual gate?
- How do LiveU/TVU test their systems? Do they have standardized test scenarios
  (e.g., "link failure at T=30s, recovery by T=35s, quality back to baseline
  by T=40s")?
- We have `strata-sim` with tc netem — how do we structure long-running
  soak tests (e.g., 10 minutes with evolving impairment) that detect
  regressions in throughput stability, memory leaks, or late adaptation?
- Should we measure glass-to-glass latency in tests? If so, how without
  specialized hardware? (Embedded timestamps in test pattern? NTP-synced
  capture?)

**Desired output:** A test harness design for `strata-sim` that runs
sender→receiver pipelines under controlled impairment, collects metrics, and
fails CI on regressions. Plus a list of the 5-10 most important test scenarios.

---

## 5. YouTube Live Ingestion: AV1, H.265, and Bitrate Ceilings

YouTube now supports AV1 and H.265 (HEVC) for live stream ingestion. What does
this enable for Strata?

**Context:** We currently send H.264 over RTMP. YouTube's live ingestion
supports H.264/H.265/AV1 via RTMP, HLS, and DASH ingest. At equal visual
quality, H.265 needs ~40% less bitrate than H.264, and AV1 needs ~50% less.
This means our bonded links could deliver *significantly* better video quality
at the same aggregate bandwidth.

**Specific questions:**

### YouTube-specific
- What are YouTube's current maximum ingest bitrate caps for 720p, 1080p,
  1440p, and 4K live streams?
- Does YouTube accept H.265 over RTMP, or only via HLS/DASH ingest?
- Does YouTube accept AV1 over RTMP, or only via HLS/DASH (CMAF)?
- What container formats does YouTube accept for each codec? (FLV doesn't
  support H.265/AV1 — we may need to switch to fragmented MP4 or MPEG-TS
  over HTTP.)
- Are there YouTube-recommended encoder settings for live AV1/H.265?
  (Profile, level, keyframe interval, etc.)

### Codec efficiency implications
- At our aggregate link capacity (~15-19 Mbps across 3 bonded LTE links),
  what resolution/framerate can we achieve with each codec?
  - H.264: 1080p30 at ~6 Mbps, 4K requires 20+ Mbps
  - H.265: 1080p30 at ~4 Mbps, 4K at ~12 Mbps (feasible!)
  - AV1:   1080p30 at ~3 Mbps, 4K at ~10 Mbps (feasible!)
- Does this mean we could deliver 4K live from 3 bonded LTE links with AV1,
  where H.264 would top out at 1080p?

### Strata pipeline changes for H.265/AV1
- What GStreamer elements replace x264enc → h264parse → flvmux → rtmpsink for
  H.265 and AV1 output?
- For HEVC: `x265enc ! h265parse ! mpegtsmux` → what output format for
  YouTube? (MPEG-TS over HTTP? fMP4 via DASH?)
- For AV1: `svtav1enc ! av1parse ! ???` → what muxer and output protocol?
- Does our NAL parser (`strata-bonding/src/media/nal.rs`) need updating for
  H.265 NAL unit types? (It already supports H.265 according to master plan,
  but verify.)
- AV1 uses OBU (Open Bitstream Unit) framing, not NAL. The master plan marks
  AV1 OBU parsing as TODO. What OBU types map to our priority levels
  (Critical/Reference/Standard/Disposable)?

---

## 6. Smart Use of Spare Bandwidth: Redundancy vs. Bitrate

When we have more link capacity than the encoder needs, what's the optimal
strategy — crank the bitrate higher, or use the spare bandwidth for redundancy?

**Context:** With efficient codecs (AV1/H.265), we'll often have spare
bandwidth. At 15 Mbps aggregate capacity with AV1 delivering great 1080p at
3-4 Mbps, we have 10+ Mbps of spare capacity. Simply increasing encoder bitrate
has diminishing returns (the quality curve flattens). But that spare bandwidth
could be used to make the stream *more resilient* instead.

**Specific questions:**

### Redundancy strategies
- **Full packet duplication**: Send every packet on 2 of 3 links. This
  survives any single link failure with zero recovery latency. Cost: 2× the
  bandwidth. When does this make sense vs. FEC?
- **Selective duplication**: Only duplicate I-frames and codec config on
  multiple links (Critical priority packets). P/B-frames get single-link
  scheduling. What's the bandwidth overhead? (~10-15% for typical GOP
  structures.)
- **Increased FEC rate**: Instead of 5-10% RLNC overhead, ramp to 30-50%
  when spare bandwidth exists. Better burst loss recovery, but adds decode
  latency. How does RLNC sliding-window FEC latency scale with redundancy
  percentage?
- **Proactive retransmission**: Pre-emptively retransmit packets that are
  "at risk" (sent on a link with rising loss). No NACK needed — send the
  retransmit before the receiver even detects the loss.

### Decision framework
- What's the quality vs. resilience Pareto frontier? At what point does
  adding more encoder bitrate give less marginal quality gain than spending
  the same bandwidth on redundancy?
- Does the answer depend on the content? (Static talking head → bitrate
  plateau is low; fast sports action → bitrate plateau is high.)
- Should the `BitrateAdapter` have a mode where it says "encoder is at
  quality ceiling, divert remaining capacity to redundancy"?
- How should the DWRR scheduler's weights change when operating in
  "redundancy mode" vs. "maximize bandwidth" mode?

### Codec-aware scheduling with SVC
- AV1-SVC and H.265 temporal SVC encode video in layers. The base layer
  is self-contained decoder. Enhancement layers add quality/resolution.
- Can we route the base layer with maximum protection (duplicate across
  links, high FEC) while sending enhancement layers opportunistically?
- If a link degrades, we drop enhancement layer packets (graceful quality
  reduction) while the base layer remains protected.
- This is what TVU calls "priority routing" — how do they decide the
  boundary between protected and opportunistic traffic?

### MPEG-TS layer considerations
- Our pipeline goes `encoder → mpegtsmux → stratasink`. The MUX adds
  framing that crosses NAL boundaries. Can we still do media-aware
  priority classification at the MPEG-TS level, or do we need to classify
  *before* muxing?
- For SVC: do temporal layers survive muxing into MPEG-TS, or do we need
  a different container (e.g., raw RTP, fragmented MP4)?

**Desired output:** A decision framework for the `BitrateAdapter` that answers:
"given X Mbps capacity and Y Mbps encoder ceiling, spend Z Mbps on redundancy."
Plus an SVC integration plan for AV1/H.265 that enables layer-aware scheduling.

---

## 7. AV1 and H.265 in the Master Plan: New Scheduler Capabilities

What new scheduling strategies do next-gen codecs enable that weren't possible
with H.264-only?

**Context:** The master plan's media awareness section (§8) is H.264/H.265
focused. AV1's OBU framing and built-in SVC open new possibilities for the
bonding scheduler that should be added to the roadmap.

**Specific questions:**

### AV1 OBU integration
- AV1 OBU types: Sequence Header, Frame Header, Tile Group, Metadata,
  Temporal Delimiter. How do these map to our 4-tier priority system
  (Critical/Reference/Standard/Disposable)?
- AV1 has frame-level `show_frame` and `showable_frame` flags. Can we use
  these to identify truly droppable frames without decoding the full bitstream?
- AV1's tile-based encoding produces independently decodable tile groups
  within a frame. Could we route different tiles to different links for
  parallel delivery?

### Temporal scalability
- With H.265 temporal SVC or AV1 temporal scalability, every other P-frame
  can be dropped without breaking decode. This gives us a *codec-level*
  graceful degradation that's cleaner than our current "drop B-frames" stage.
- How does temporal scalability interact with our degradation stages?
  Should Stage 1 be "drop temporal enhancement" instead of "drop B-frames"?
- What's the quality impact of dropping temporal layers vs. reducing encoder
  bitrate? (Framerate reduction vs. quality reduction — which looks better
  to viewers?)

### Rate-distortion optimized scheduling
- Given that we know the codec's rate-distortion curve (diminishing quality
  returns at higher bitrates), can the scheduler make *joint* decisions about
  bitrate AND redundancy that optimize for overall viewer experience?
- Example: 4 Mbps AV1 at 1080p delivers VMAF 93. 8 Mbps delivers VMAF 95.
  The extra 4 Mbps buys only 2 VMAF points. Spending that 4 Mbps on packet
  duplication might protect against a 500ms link outage that would cause a
  visible glitch (VMAF drop to 70 for several frames). Which is better for
  viewer experience?
- Is there research on joint rate-distortion-reliability optimization for
  bonded video transport?

**Desired output:** Updated priority classification tables for AV1 OBU types,
a temporal-SVC integration plan, and a rate-distortion-reliability optimization
framework for the scheduler.
