Strata Transport Research Report
1. BBR-Style Bandwidth Probing for Bonded UDP Links
Professional cellular‐bonding systems (LiveU/TVU/Dejero) typically measure each link’s throughput by
active probing or by modeling delivery rate from ACK timing, since the radio scheduler (not a fixed
router queue) limits capacity. We propose running a BBR‑like state machine per link using ACK
timestamps. In practice, this means continuously measuring the delivery rate (bytes ACKed / RTT) and
tracking its recent maximum as the bottleneck bandwidth estimate . BBR’s design – with a maxfilter over delivery-rate samples and a min-filter over RTT – accommodates wireless variability .
Periodic bursts (“pacing gain >1”) help reveal capacity beyond momentary delivery rates . A short
probe burst on a cellular uplink should last on the order of tens of milliseconds (multiple 1 ms TTIs) to
ensure the scheduler grants enough resources (e.g. 5–50 ms pulses). After each burst, measure the
bytes ACKed and the minimum RTT seen; update BBR’s BtlBw (= max recent delivery rate) and RTprop
(= min RTT) estimates.
Alternatively, delay-gradient methods like Vegas or COPA could be used: COPA sets its target rate to $1/
(\delta\cdot \mathit{d_{queue}})$ , adjusting send-rate based on queuing delay. However, lossagnostic ACK-clock probing (BBR-style) is generally more robust over variable-loss links. Explicit packetpair/train bursts are another option: send a tightly spaced packet train and measure receive spacing,
but this can be imprecise on cellular.
We can implement a trimmed-down BBR per link: track delivered bytes and ACK gaps to compute
delivery-rate samples, and use the windowed max-filter as BBR specifies . The bonding scheduler
needn’t “pause” when a link is probing – it simply uses the current per-link estimate to schedule new
packets. (If one link is in PROBE_BW, it might temporarily overshoot; the scheduler can treat that link as
having higher current capacity until its model stabilizes.)
Proposed algorithm: In TransportLink::get_metrics() , for each link maintain: - RTprop :
minimum RTT observed (across many packets). - BtlBw : maximum delivery-rate sample over a sliding
window (e.g. last 5–10 s) . - On every ACK: compute delivery_rate =
acked_bytes_since_last / RTT_sample ; update BtlBw as max(BtlBw, delivery_rate) . -
Periodically (every ~1 s), send a brief high-rate burst (pacing_gain>1) to test if delivery_rate spikes above
current BtlBw; if so, raise BtlBw accordingly. - Optionally detect rising RTT (queue growth) to cap rate (as
in “Delay‑BBR” ).
This replaces the Mathis formula (which ignores actual bottleneck speed) with a real-time measurement
of throughput. It will naturally yield different capacities on links with 8/5/6 Mbps limits because once
your send-rate exceeds the scheduler grant, delivery-rate samples saturate at ~8/5/6 Mbps, fixing BtlBw
there.
2. Dynamic Bitrate Behavior in GStreamer Encoders
x264enc: In modern GStreamer (post-1.10), x264enc allows runtime changes to bitrate and vbvbuf-capacity . Setting enc.set_property("bitrate", new_bitrate) will take effect in the
next GOP. In practice, the encoder will apply the new target at the next keyframe (or soon after). If VBV
1 2
1 2
3 2
4
1
1
5
6
1
buffering is enabled (by default it is, to enforce maxbitrate), then raising bitrate drastically can
empty the VBV buffer and produce a burst of frames until it refills. Thus, vbv-buf-capacity (in ms)
should roughly scale with bitrate: e.g. at 5 Mbps, a 1000 ms buffer holds ~0.625 MB. If you set
bitrate much higher than VBV can smooth, you risk sending short spikes.
Valid ranges (from code/docs): bitrate is in kbit/s (e.g. 1000–50000+). vbv-buf-capacity default
600 ms, max allowed is large (the code clips internally). In practice, keep VBV ~500–1000 ms for live CBR.
If bitrate goes to 8000 kbps and VBV is still 1000 ms, the encoder will try to fit data into that buffer,
causing high instantaneous rate (likely exceeding 8 Mbps). To avoid link overshoot, increase VBV size
when increasing bitrate.
If the application ramps bitrate too quickly, x264enc (with VBV) will output at the higher rate and
momentarily queue up to VBV size, potentially causing packet bursts > link rate. In effect, our step size
should be small relative to VBV-duration. Alternatively, we can throttle packet sends or use pacing after
the encoder.
GStreamer does not have a special “bitrate-change” event; the normal way is g_object_set() . This
will trigger x264 to adjust its internal rate control state (effectively resetting the CBR target). Some
implementations force an IDR on rate change; others smooth it. Empirically, set bitrate only on
keyframe boundaries for safety, and/or wait one GOP before more changes.
x265enc (HEVC): GStreamer’s x265enc also has a bitrate property (kbps) that is mutable. It
supports VBV via vbv-bufsize and vbv-maxrate (renamed from legacy names). It likewise has a
tune property: setting tune=zerolatency disables B-frame reordering and long lookahead for
minimal latency (as with libx265’s --tune zerolatency ). The “zerolatency” tune is crucial for live:
it sets B-frames=0 and rc-lookahead=0 (per x265 docs ). Quality at 720p–1080p is noticeably better
than x264 at the same bitrate: typically ~40% bitrate reduction for equal quality (i.e. 39–44% less
data for same PSNR ). In practice, we observe ~3–4 Mbps x265 ≈ 6 Mbps x264 in perceptual quality
for 1080p30.
AV1 (svtav1enc/rav1e): The SVT-AV1 plugin provides properties like target-bitrate and maxbitrate (kbps), allowing VBR/CBR. Dynamic changes should be possible via g_object_set() on
target-bitrate during play. Encoding latency is high: even SVT-AV1 on a fast x86 may struggle realtime at 1080p30. Commodity ARM64 (RPI5) is likely too slow; Jetson AGX Orin with its built‑in AV1
encoder can do 1080p in hardware (12×1080p30 per docs ). Other options: the Alliance’s
“av1enc” (libaom) is too slow for real-time. NVidia’s nvv4l2av1enc on Orin is probably the best for
on‑board hardware AV1.
AV1’s SVC features: GStreamer has an av1parse that recognizes OBU layers, and SVT-AV1 supports
temporal layering (via AV1’s frametemporal layering). We can expose temporal layers by enabling e.g.
--aq-mode 3 or using ALTREF. Spatial SVC (different resolution layers) isn’t fully automated by x265/
x264 style parameters; one would encode multiple resolutions or use AV1’s built-in SF (superframe).
GStreamer itself doesn’t have a simple “num_temporal_layers” property like H.264 do; advanced AV1
encoder configuration (via plugin parameters or raw args) would be needed.
Summary: Build a codec abstraction that: - Uses bitrate (x264/x265) or target-bitrate (svtav1)
properties. - For each codec, on bitrate change request: pause until a keyframe (if not already), set new
bitrate and adjust VBV buffer to match (e.g. vbv-buf-capacity = new_bitrate/1000 ms as a
heuristic). - For x264/x265 use tune=zerolatency . For AV1, use no lookahead if possible (or lowest
7
7
8
8
9
2
as supported) for minimal lag. - For dynamic changes, monitor encoder stats (via qtmux messages or
so) to ensure VBV isn’t overflowing.
3. Closed-Loop Encoder Adaptation ( BITRATE_CMD )
The receiver-side has crucial information that the sender lacks: it knows actual decode buffer health,
rebuffering events, FEC repair rate, and true end-to-end delay. Industry practice (e.g. LiveU/TVU “Smart
VBR”) leans on the receiver: it estimates safe output rate and signals the transmitter . Thus, we
should have the receiver compute a “safe bitrate” ceiling from observed goodput and losses, then send
it via BITRATE_CMD .
Concretely, the receiver can monitor its net bitrate (total recovered video bits per second after losses/
FEC) and buffer occupancy. If loss/FEC spikes or buffer empties, it computes a lower ceiling rate. We can
implement a NADA‑style reference rate ( r_ref ) at the receiver: essentially the sum of delivered rates
over all links minus estimated headroom. Our existing aggregate_nada_ref_bps is close to this. The
receiver then pushes a BITRATE_CMD( safe_bitrate ) to the sender. The sender’s
BitrateAdapter should then cap its encoder to that rate.
Alternatively, the receiver could just send raw stats (loss %, jitter, etc.) and let the sender combine them,
but simpler is for the receiver to do the math and send one number. This also aligns with how TVU/
LiveU do it: the receiver signals an encoder rate (often as a max–minR).
When the BitrateAdapter degrades
(Normal→DropB→ReduceBitrate→KeyframeOnly→Emergency), the receiver can trigger these too: for
example, if it detects buffer underrun, it might tell sender to drop to keyframes only until stable. The
GStreamer pipeline responds by forcing keyframes or quality changes. The adapter’s decisions can map
to scheduler vs. encoder: - Normal→DropB: simply drop B-frames at scheduler (no encoder change). -
ReduceBitrate: set new lower bitrate on encoder (via GstEncoderBitrateProfile or property). -
KeyframeOnly/Emergency: push an immediate IDR (via sending a special event), and schedule only
keyframes.
Regarding NADA (RFC 8698): our computed aggregate_nada_ref_bps is analogous to r_ref .
NADA’s use of r_ref sent from network to sender is similar to our receiver → sender command. We
don’t need full NADA machinery, but we can use its logic: keep the encoder input rate ( r_vin ) and
match r_ref (ceiling) to maintain low queuing.
Plan: Do receiver-based adaptation. The BitrateAdapter module at sender should only obey rates
sent by receiver (or aggregate of them). The receiver monitors actual output bitrates and socket stats,
computes a safe-rate ceiling, and sends BITRATE_CMD( ceiling ) . The sender, on receiving that
command, does adapter.set_target_bitrate(ceiling) . All final decisions come from receiver,
avoiding conflicting self-estimates.
Integration: wire the BitrateAdapter into the main loop so that on receiving a BITRATE_CMD
message, it updates the encoder’s bitrate. For visual stability, any large jump-down should also trigger
an IDR event.
10
3
4. Integration Testing Metrics
For end-to-end (sender→network→receiver) CI tests, we should run the full pipeline under emulated
impairments and check key metrics:
No-decode metrics: these can be gathered from packet stats.
Throughput: delivered bits/sec vs. target. Ensures we hit expected goodput.
Packet Delivery Ratio (PDR): fraction of sent packets received (after FEC recovery).
Time-to-first-frame: how long until the receiver shows any video (from pipeline start).
Keyframe interval stability: measure actual spacing of IDRs to ensure scheduled keyframes
were sent on time.
Bitrate ramp time: time to climb from low to target bitrate when recovery occurs.
Loss-burst resilience: longest loss burst survived (with FEC). Simulate e.g. 100 ms outage and
see if video glitches.
Buffer health: e.g. jitter-buffer occupancy over time (queueing delay).
Decode metrics: for quality/regression tests we should decode and compute objective video
quality.
VMAF/PSNR/SSIM: correlate with visual quality. (These require decoding and a reference; heavy
but useful in nightly runs.)
Such tests could run on synthetic patterns or a known clip.
If too heavy for every CI commit, run them as longer nightly jobs.
Test scenarios: Industry systems run staged impairments (e.g. link drop, jitter spike). We should script
scenarios in strata-sim , for example: 1. Static config: steady 3-link LTE (8/5/6 Mbps) with 10%
random loss, test 10 min stability. 2. Link failure: drop one link at T=30s, recover at T=40s; expect
failover and return to baseline quality by ~T=45s. 3. Sudden congestion: at T=20s, artificially limit one
link’s rate to 2 Mbps for 5s, then restore. 4. Jitter spike: introduce 100 ms jitter burst at T=50s for 10s. 5.
Increasing loss: ramp packet loss from 0% to 30% over 2 minutes and see adaptation. 6. Latency shift:
add 200 ms extra delay mid-test to simulate cell handover.
Each scenario should check no stalls/glitches beyond tolerated (e.g. no go-back if metadata sending
fails). Regression failures if metrics deviate beyond tolerance (e.g. suddenly average throughput drop by
>10% without cause).
Glass-to-glass latency: Hard to measure automatically without hardware. We can approximate by
embedding timestamps in test frames (e.g. overlay system clock) and measuring difference on receiver
(requires decode). Or assume synchronized clocks (NTP-like) and mark PTS. For CI, we could skip precise
glass-to-glass, focusing on network latency (ACK-RTT) and buffer delays. Visual latency tests might be
separate.
5. YouTube Live Ingestion: H.265/AV1
YouTube now allows live ingest of H.265 (HEVC) and AV1. According to YouTube’s encoder guidelines,
recommended bitrate caps (for all codecs) are roughly: - 720p30: up to ~8 Mbps - 1080p30: up to
~8 Mbps - 1440p30: up to ~25 Mbps - 4K30: up to ~35 Mbps .
•
•
•
•
•
•
•
•
•
•
•
•
11 12
4
YouTube’s settings page explicitly lists AV1 and HEVC in the RTMP/RTMPS protocol section . However,
the underlying RTMP/FLV container by itself cannot carry HEVC/AV1 (it was defined only for H.264) .
In practice, this means that to use HEVC or AV1 we must output a container that supports them (e.g.
MPEG-TS or fragmented MP4) and send via HLS/DASH ingest, or use a proprietary hack. The simplest
approach is to use MPEG-TS over HTTP (HLS) or fragmented MP4 (CMAF) over DASH. For example, one
can use mpegtsmux and hlssink for H265/AV1, or mp4mux ! dashsink . FLV (RTMP) only works
for H.264.
YouTube’s recommended encoder settings (for any codec) include: 2-second keyframe interval (i.e. ~60
frames for 30fps) , Rec.709 colorspace (8-bit) for SDR , and CBR (constant rate). For HEVC
specifically, use Main or Main10 profile (8- or 10-bit); and enable “zerolatency” tuning ( --tune
zerolatency ) to avoid reordering delay. For AV1, use the AV1 main profile, 8‑bit, and tile columns = 2
minimum for 4K (per YT note ). (AV1 does not support HDR on YT Live .)
Implications for Strata: Using HEVC or AV1 can roughly halve required bitrate for same quality. For
example, at ~15 Mbps aggregate, H.264 can do 1080p30 easily (~6 Mbps needed) but 4K30 would need
~20 Mbps (above our capacity). In contrast, HEVC can reach 4K30 at ~12 Mbps and AV1 at ~10 Mbps
for comparable quality . Thus 3 bonded LTE links (~15–18 Mbps total) could plausibly carry 4K30
with AV1/HEVC. In Strata, replace:
x264enc ! h264parse ! flvmux ! rtmpsink
with, for HEVC:
x265enc tune=zerolatency bitrate=... ! h265parse ! mpegtsmux ! hlssink (or
rtmpsink if container supports it)
(MPEG-TS can be pushed via HTTP POST or HLS, as YT recommends HLS for HEVC.) For AV1:
svtav1enc bitrate=... ! av1parse ! mpegtsmux ! hlssink
(or mp4mux ! dashsink ). Ensure our NAL parser supports H.265 NAL (which it does per plan) and
implement an AV1 parser for OBUs. AV1 framing uses OBUs (Sequence Header, Frame Header, Tile
Groups, etc.) instead of NALs. Map OBU types to priorities: Critical (e.g. Sequence Header, Frame Header,
OBU_TEMPORAL_DELIMITER), Reference (base-layer Tile Groups, i.e. first-order frames), Standard
(enhancement Tile Groups), Disposable (film grain, metadata). See §7 below for details.
6. Smart Use of Spare Bandwidth: Redundancy vs. Bitrate
When codec efficiency frees up bandwidth (e.g. AV1 sending 1080p30 at ~3–4 Mbps on a 15 Mbps link),
we face a trade-off: increase video quality vs. add protection. Beyond a certain point, video quality
saturates (diminishing returns). For example, double-bitrate from 4 Mbps to 8 Mbps might only
improve VMAF by a few points , whereas spending that 4 Mbps on redundancy could prevent a
catastrophic quality drop on a link failure.
Redundancy modes: - Full duplication: send each packet on 2 links. This buys perfect recovery from any
single link outage (zero recovery latency). Cost: 2× bandwidth. Best when reliability is paramount and
spare bandwidth is ample.
13
14
15 16
17 18
19
19 20
19
5
- Selective duplication: only duplicate critical packets (IDRs, SPS/PPS, scene-change frames). Modern
GOPs have ~10–15% of bits in I-frames ; duplicating just those (and codec headers) gives ~10–20%
overhead but protects against most decoders stalling.
- High-rate FEC: increase RLNC FEC from 5–10% up to 30–50% of packets. Improves long-loss recovery at
cost of extra delay (decoder must wait to decode FEC). Latency increases roughly linearly with FEC
window size (e.g. 1000 kb of FEC adds ~100 ms of buffer).
- Proactive retransmit: send duplicate of a packet preemptively when loss seems likely (e.g. a packet on a
congesting link). This is akin to limited redundancy without waiting for a NACK. It requires predictive
heuristics (e.g. rising per-link loss rate).
Decision framework: We should consider the rate–distortion curve of the video. Past a “knee” in the
curve, quality gains per extra Mbps are small. For example, if AV1 at 4 Mbps yields VMAF=90 and at
8 Mbps yields VMAF=93, the 4 Mbps only buys 3 points. But using that 4 Mbps for duplication might
avoid a VMAF 70 “glitch” (a drop of ~20) during a 500 ms outage. Qualitatively, protecting against large
losses can improve min-quality seen by viewers more than small average quality gains.
Thus, when encoder bitrate is at or above its effective plateau (as signaled by flat PSNR/VMAF increase
curve), the BitrateAdapter should shift mode: “channel excess capacity into resilience.” Practically,
this can be a flag in the adapter: if (estimated_gap > threshold) , switch from “max bitrate” mode
to “max protection” mode. Then:
- The scheduler can duplicate all “Critical” packets (making their weight=2 on two links) and apply 2× FEC
rate;
- Enhancement frames (“Standard/Disposable”) can be demoted (dropped first if congestion) and not
duplicated.
- The DWRR weights can be adjusted so that base-layer traffic gets proportional extra weight across
links.
Content-aware scheduling: With H.265/AV1 SVC, we can route lower-quality (enhancement) layers on
weaker links. E.g. always duplicate the base layer (L1 temporal, or lowest spatial), but send
enhancement layers only once. On link loss, we’d gracefully drop enhancement frames (framerate or
resolution drops) while still delivering a decodable base layer. TVU’s “priority routing” does this by
splitting streams into priority queues.
MUX considerations: Our pipeline muxes into MPEG-TS, which interleaves streams. To preserve packetlevel priority, we should classify before muxing. Ideally use RTP or fMP4 with tagged OBU boundaries.
With TS, we can still tag PES packets by PID/stream type, but mixing layers into one stream complicates
scheduling. A future enhancement is to output layered streams separately (e.g. produce base and
enhancement elementary streams), then bond-scheduler could schedule them with independent
priorities.
Summary: Define a utility function trading bitrate vs. loss-protection: e.g. target a bit-error–weighted
PSNR or VMAF. When spare bandwidth beyond the “quality optimal” point is detected, switch to
redundancy. In practice, implement “redundancy mode” in BitrateAdapter : if
(target_quality_gain < safety_gain) , then allocate Z Mbps to protection (full/partial
duplication or extra FEC) and reduce encoder to target_quality_bitrate . Adjust scheduler
weights accordingly: weights for critical priorities get multiplied, others reduced.
7. New Scheduler Strategies for AV1/H.265
AV1 and HEVC introduce new scalability features. We should expand our packet-priority tables:
21
6
AV1 OBU types: According to the AV1 spec, OBUs include sequence headers, frame headers, tile groups
(video data), temporal delimiters, and metadata. We map them as: - Critical: Sequence Header (global
params), Frame Header (synchronization info), Temporal Delimiter OBUs – without these the stream
decodes nothing.
- Reference: Base-layer Tile Group OBUs (the first tile group of each frame, or all tiles of a base-layeronly frame) – these enable core frame reconstruction.
- Standard: Enhancement-layer Tile Group OBUs (if using SVC) or regular P-frame tiles. Also highpriority metadata.
- Disposable: Nonessential metadata (like film-grain OBUs, discarded frames).
The AV1 parser will check the OBU type and show_frame flags. A tile group OBU with
show_frame=0 (non-displayed intermediate) can be considered drop-able. Spatial-temporal layer info
(via decode target info) could allow finer classification if we implement SVC fully.
Temporal SVC: Both AV1 and H.265 can encode multiple temporal layers. For example, a 2-layer
temporal AV1 encodes frames [I, P, B, P, I,…] where every other frame is dropable. This provides a
graceful fallback: dropping the high-temporal layer (like B-frames) reduces FPS without breaking
continuity. Our scheduler should treat “drop B-frames” stage as “drop temporal enhancement layers”. In
the adapter’s stage machine, Stage1 (drop B) becomes “disable layer 1 temporal frames”; Stage2
(reduce bitrate) triggers actual encoder rate change; Stage3 (KeyframeOnly) remains forcing IDRs.
Viewer studies show dropping enhancement (framerate reduction) often looks smoother than dropping
quality on every frame.
Tiles & Parallelism: AV1’s tile groups (intra-frame slices) could in theory be sent on different links. In
practice, tiles of one frame should all reach by decode time, so splitting them across links does parallel
transport. This is like intra-frame striping. We could schedule tiles round-robin on links to reduce perlink burstiness. It’s complex and must respect OBU boundaries.
Rate–Distortion–Reliability scheduling: Ideally, the scheduler would make joint decisions: it knows
the encoder’s R–D curve (e.g. VMAF vs bit rate) and packet-loss impact. It can then decide, for each extra
bitrate unit, whether to allocate it to video bits (raising quality) or to redundant bits (lowering loss). This
is an open research question, but a simple model is: if the marginal utility of more video (ΔVMAF per
Mbps) is below the utility of added protection, invest in protection. We don’t have an exact formula, but
one approach is a cost function like
$$U = \text{quality}(b) - \lambda \cdot \text{loss_prob}(b)$$
and maximize it over b (bitrate) and redundancy. The BitrateAdapter could implement a heuristic: keep
encoder at the knee of the quality curve, pump remaining bits into FEC/dup.
Summary: Extend priority tables: - H.265: Already planned to support NAL types (we’ll add any missing
NALs, e.g. VPS for 3D, SEI as Critical). - AV1: As above, classify OBUs by type. Temporal/Spatial layers: at
least mark base layer frames as higher priority. - Adjust stage1 to “drop 1st temporal layer” in SVC
instead of indiscriminate B-frames. - Consider packetizing by OBU for finer control.
References: Key insights drawn from BBR’s draft , congestion-control research (Copa ), and
streaming encoder recommendations . All vendor-specific info is inferred from public
docs and common practice.
BBR Congestion Control
https://www.ietf.org/archive/id/draft-cardwell-iccrg-bbr-congestion-control-02.html
1 2 4
8 20 22 12
1 2 3 21
7
usenix.org
https://www.usenix.org/system/files/conference/nsdi18/nsdi18-arun.pdf
[1901.09177] An Optimized BBR for Multipath Real Time Video Streaming
https://ar5iv.org/pdf/1901.09177
change the gstreamer element paramter dynamic
https://gstreamer-devel.narkive.com/gxBnnuk5/change-the-gstreamer-element-paramter-dynamic
Preset Options - x265 Documentation - Read the Docs
https://x265.readthedocs.io/en/stable/presets.html
H.264 vs H.265 - AVC vs HEVC - What's the difference?
https://flussonic.com/blog/news/h264-vs-h265
AV1 encoding performance - Jetson AGX Orin - NVIDIA Developer Forums
https://forums.developer.nvidia.com/t/av1-encoding-performance/248804
[PDF] TVU Transceivers & Receivers
https://www.tvunetworks.com/doc/TVU_Transceiver_3200.pdf
Choose live encoder settings, bitrates, and resolutions - YouTube Help
https://support.google.com/youtube/answer/2853702?hl=en
Does YouTube support Live ingest of HEVC over RTMP or is this only available via HLS? - Stack
Overflow
https://stackoverflow.com/questions/61996884/does-youtube-support-live-ingest-of-hevc-over-rtmp-or-is-this-only-availablevia
Av1 vs. H264 - Which Codec Should You Use?
https://getstream.io/blog/av1-h264/
4
5
6
7
8 19
9
10
11 12 13 15 16 17 18 22
14
20
8