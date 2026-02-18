# Strata Transport — Game Plan

> Concrete implementation roadmap synthesized from both research reports
> (Gemini & ChatGPT) against the current codebase state.
>
> Priority: ship the highest-impact changes first, validate with tests,
> then layer on H.265 codec support.
>
> **Hardware constraint:** Edge encoding hardware supports H.265 (HEVC) only.
> AV1 is deferred until hardware with AV1 encode support is available.

---

## Current State (as of Feb 2026)

**Working & wired:**
- BitrateAdapter (25 Mbps ceiling, 500 kbps floor, pressure-based ramp)
- Kalman RTT filter, IoDS, BLEST, Thompson Sampling, DWRR — all wired
- EWMA rate tracking in TransportLink (socket-level send rate)
- RLNC sliding-window FEC (K=32, R=4) in strata-transport
- NACK-based ARQ (3 retries)
- NAL parser (H.264 + H.265)
- GStreamer stratasink/stratasrc with stats thread → `bitrate-command` messages
- Docker sim: 3 links with tc netem (8/5/6 Mbps, varied RTT and loss)
- Control plane, dashboard, agent all running

**Implemented but NOT wired:**
- BiscayController (full BBR state machine in `congestion.rs`) — not connected
- DegradationStage (in BitrateAdapter) — computed but scheduler ignores it
- Bonding-layer RaptorQ FEC (`scheduler/fec.rs`) — not encoding/decoding
- GilbertElliott channel model — not driving FEC rate
- ModemSupervisor, LinkHealth — no real hardware interface

**Not implemented:**
- BBR delivery-rate probing (Mathis formula still used for capacity)
- Codec abstraction layer (x264/x265)
- Enhanced RTMP / HLS output for H.265
- Receiver → sender telemetry feedback (BITRATE_CMD from receiver)
- Integration test harness with automated assertions

---

## Phase A: Real Capacity Estimation (replace Mathis)

**Why first:** Both reports identify the Mathis formula as the biggest
correctness gap. It produces fictional capacity values disconnected from actual
link throughput. Everything downstream (BitrateAdapter, scheduler weights)
depends on capacity being accurate. The BiscayController already exists but
isn't wired.

### A.1 — Wire BiscayController into TransportLink

The `BiscayController` in `strata-transport/src/congestion.rs` already has:
- `on_bandwidth_sample(delivered_bytes, interval_us)` → windowed max-filter for BtlBw
- `on_rtt_sample(rtt_us)` → min-filter for RTprop, bufferbloat detection
- `pacing_rate()`, `btl_bw()`, `rt_prop_us()` getters

**What to do:**
1. Add `BiscayController` field to `TransportLink`
2. In `recv_feedback()` — when an ACK arrives, compute delivery rate from
   ACKed bytes / RTT and call `controller.on_bandwidth_sample()`
3. On PONG response — call `controller.on_rtt_sample()`
4. In `get_metrics()` — replace the Mathis formula with
   `controller.btl_bw() * 8.0` for `capacity_bps`
5. Keep EWMA `observed_bps` as a secondary signal for the scheduler

**Key insight (both reports agree):** capacity should be measured from ACK
feedback (delivery rate = bytes ACKed / interval), NOT inferred from
loss/RTT. The existing EWMA send-rate is measuring the *sending* rate, not the
*delivery* rate. We need delivery rate from ACK timestamps.

### A.2 — Add delivery rate tracking to ACK path

The receiver already sends ACKs. Enhance them to include timing info:
1. In the ACK processing path, record `delivered_bytes` since last ACK
2. Compute delivery rate sample: `acked_bytes / (ack_rtt / 2)` as initial
   approximation, or `acked_bytes / ack_interval` if we track inter-ACK gaps
3. Feed each sample to `BiscayController::on_bandwidth_sample()`
4. The windowed max-filter in BiscayController produces BtlBw

### A.3 — Phase-shifted probing coordination

Both reports flag this: if multiple links probe simultaneously, aggregate
overshoot occurs.

1. Add a `probe_phase: usize` field to the scheduler (round-robin across links)
2. Only allow one link to be in PROBE_BW:UP at a time
3. Others remain in CRUISE while one probes
4. Rotate probe token every ~1 second

**Validation:** After wiring, `capacity_bps` should read ~8/5/6 Mbps for our
three tc netem links (matching the actual rate limits), NOT the inflated
Mathis values.

---

## Phase B: DegradationStage → Scheduler

**Why next:** The BitrateAdapter already computes a `DegradationStage` but
nothing acts on it. This is the missing link between bitrate adaptation and
packet scheduling — what both reports call "graceful degradation."

### B.1 — Wire DegradationStage into BondingScheduler::send()

From the master plan §8:
- **Normal:** all packets scheduled normally
- **DropB (Stage 1):** skip Disposable-priority packets in the scheduler
- **ReduceBitrate (Stage 2):** already handled by BitrateAdapter → encoder
- **KeyframeOnly (Stage 3):** skip Standard + Disposable, only Critical + Reference
- **Emergency (Stage 4):** skip everything except Critical

**What to do:**
1. Add `degradation_stage: DegradationStage` field to BondingScheduler
2. Expose `set_degradation_stage()` method
3. In `send()`, check stage before enqueueing non-critical packets
4. In sink.rs stats thread, post degradation stage alongside bitrate-command
5. strata_node.rs reads stage from message and forwards to scheduler

### B.2 — Temporal layer awareness (H.265 SVC prep)

Both reports recommend: Stage 1 should be "drop temporal enhancement layers"
not just "drop B-frames." For now, Disposable priority already maps to B-frames
in H.264. When H.265 temporal SVC is enabled (Phase D), the NAL parser already
classifies H.265 NAL types, so the priority classification will naturally
separate base/enhancement layers.

---

## Phase C: VBV Initialization at Max Bitrate

**Why now:** Both reports explicitly warn that VBV misconfiguration causes
encoder instability. Quick fix, high impact.

### C.1 — Initialize VBV at 25 Mbps ceiling

In `strata_node.rs` pipeline construction:

```
// Before: vbv-buf-capacity was set based on initial bitrate
// After:  set it based on max expected bitrate (25 Mbps ceiling)
enc.set_property("vbv-buf-capacity", 1000u32); // 1000ms at max bitrate
```

Both reports agree: VBV buffer should be sized for the *maximum* session bitrate
at init. The `bitrate` property can then be modulated freely below that ceiling
without VBV becoming a bottleneck or causing resets.

---

## Phase D: H.265 (HEVC) Codec Support

**Why:** H.265 delivers ~40% bitrate savings over H.264 at equal quality. This
makes 4K over 3 bonded LTE links feasible (~12 Mbps HEVC vs ~20+ Mbps H.264).
YouTube now accepts H.265 via Enhanced RTMP. Our edge hardware has H.265
hardware encoding — this is the codec we're shipping with.

### D.1 — Codec abstraction layer (CodecController)

Create `crates/strata-gst/src/codec.rs`:

```rust
pub trait CodecController {
    fn set_bitrate_kbps(&self, enc: &gst::Element, kbps: u32);
    fn force_keyframe(&self, enc: &gst::Element);
    fn codec_name(&self) -> &str;
}
```

Implementations:
- **X264Controller:** `set_property("bitrate", kbps)`. At init: set
  `vbv-buf-capacity=1000`, `tune=zerolatency`, `speed-preset=superfast`.
- **X265Controller:** `set_property("bitrate", kbps)`. At init: set
  `tune=zerolatency`, `vbv-bufsize=<max_kbps>`, `vbv-maxrate=<max_kbps>`.
  ~40% bitrate savings vs x264 at same quality.

The abstraction allows future codec additions (e.g., AV1 when hardware
is available) without changing the adaptation or scheduling layers.

### D.2 — H.265 NAL classification (already done)

The NAL parser in `strata-bonding/src/media/nal.rs` already supports H.265
NAL unit types. Priority mapping already exists:

| H.265 NAL Type | Priority | Action |
|---|---|---|
| VPS, SPS, PPS (32-34) | Critical | Duplicate on all links, max FEC |
| IDR, CRA, BLA | Reference | Send on best 2 links, high FEC |
| TRAIL_R (1) | Standard | Normal scheduling |
| TRAIL_N (0) | Disposable | Lowest priority, droppable |

No new parser is needed — verify the existing H.265 classifications are
complete and add any missing NAL types (e.g., SEI, AUD).

### D.3 — Enhanced RTMP pipeline for YouTube

Standard FLV/RTMP cannot carry H.265. Options:

**Option 1 — Enhanced RTMP (preferred for lowest latency):**
GStreamer 1.24+ has `eflvmux` which supports Enhanced FLV with H.265 FourCC.
YouTube accepts Enhanced RTMP for HEVC.

```
x265enc tune=zerolatency ! h265parse ! eflvmux ! rtmp2sink location=rtmp://...
```

**Option 2 — HLS ingest (fallback if eflvmux unavailable):**
```
x265enc tune=zerolatency ! h265parse ! mpegtsmux ! hlssink2 location=...
```

YouTube recommends HLS for HEVC when Enhanced RTMP isn't available.

### D.4 — YouTube-specific encoder settings for HEVC

From YouTube's encoder guidelines:
- Keyframe interval: 2 seconds (YouTube requirement)
- Color: Rec.709, 8-bit SDR
- CBR mode
- HEVC: Main or Main10 profile
- `tune=zerolatency` (mandatory for live — disables B-frame reordering
  and lookahead, critical for sub-second bonded transport)
- Max bitrate caps: 1080p60 = 6-10 Mbps, 4K60 = 25-40 Mbps

### D.5 — Bitrate ceiling update for HEVC efficiency

With HEVC's ~40% savings, the BitrateAdapter ceiling should be reconsidered:
- 1080p60 HEVC: 4-6 Mbps for excellent quality (vs 8-10 Mbps H.264)
- 4K60 HEVC: 12-15 Mbps for excellent quality (vs 25+ Mbps H.264)
- Keep 25 Mbps ceiling as the absolute hardware max, but the adapter's
  "visually lossless" threshold is much lower with HEVC — more spare
  bandwidth available for redundancy (Phase G)

---

## Phase E: Integration Test Harness

**Why:** Both reports stress that "eyeball verification on YouTube" isn't
engineering. We need automated, assertion-based tests.

### E.1 — Transport-level metrics (no decode required)

Measure after each scenario:
- **Throughput stability:** coefficient of variation < 30% over 20s steady state
- **Packet Delivery Ratio (PDR):** >99.9% after FEC recovery
- **Bitrate ramp time:** from 500 kbps to target in < 5 GOPs
- **Recovery latency:** adapt to link failure in < 2 GOPs
- **Reordering depth:** max receiver buffer needed (proxy for jitter)

### E.2 — Core test scenarios (both reports' consensus top 5)

| # | Scenario | Assertion |
|---|---|---|
| 1 | **The Cliff** — highest-capacity link drops to 0 instantly | Stream survives, bitrate adapts within 2 GOPs |
| 2 | **Flapping Link** — one link toggles 5/0.5 Mbps every 5s | Scheduler penalizes unstable link, no encoder oscillation |
| 3 | **Jitter Bomb** — 500ms jitter on all links | Video smooth with higher latency, no stuttering |
| 4 | **Burst Loss** — 20% loss for 2 seconds (handover sim) | FEC + ARQ recovers, no visible artifacts |
| 5 | **Bandwidth Ramp** — 1 Mbps → 20 Mbps over 30 seconds | Probing detects capacity increase, encoder ramps up |

### E.3 — Test implementation in strata-sim

Each scenario:
1. Create 3 veth pairs in network namespaces
2. Apply tc netem impairments per scenario
3. Run sender → bonding → 3 links → receiver for 30-60 seconds
4. Collect metrics at receiver
5. Assert on thresholds

Soak test (nightly): 10-minute random-walk impairment, check for memory leaks,
throughput drift, A/V sync.

---

## Phase F: Receiver Telemetry Feedback

**Why:** Both reports agree the receiver has "ground truth" — it knows actual
goodput, FEC repair rate, and end-to-end delay. The sender currently estimates
everything from its own metrics, which is incomplete.

### F.1 — Architecture decision (resolved)

Both reports converge on: **Sender-side control with receiver telemetry.**

- Receiver computes: effective goodput, FEC repair rate, jitter buffer health
- Receiver sends these as raw metrics via LINK_REPORT (subtype 0x04) or a new
  RECEIVER_REPORT control packet
- Sender's BitrateAdapter uses receiver metrics as an additional input signal
  (not a single command override)
- Sender retains control because it needs per-link capacity for scheduling

**Why not pure receiver-side:** A single aggregate "safe bitrate" from the
receiver obscures per-link performance, making bonding ratio optimization
impossible.

### F.2 — Receiver report structure

Add to control packets (or reuse LINK_REPORT):
```
ReceiverReport {
    goodput_bps: u64,       // total recovered video bits/sec
    fec_repair_rate: f32,   // fraction of packets recovered by FEC
    jitter_buffer_ms: u32,  // current jitter buffer depth
    loss_after_fec: f32,    // residual loss after FEC recovery
}
```

### F.3 — BitrateAdapter integration

Feed receiver reports into BitrateAdapter alongside per-link capacity:
- If `loss_after_fec > 0.01` → apply pressure (ramp down)
- If `jitter_buffer_ms > 500` → bufferbloat signal, cap bitrate
- If `goodput_bps` significantly below encoder output → congestion

---

## Phase G: Spare Bandwidth → Redundancy

**Why last:** Only makes sense after capacity estimation is accurate (Phase A)
and codec efficiency frees up bandwidth (Phase D). This is the "polish" layer.

### G.1 — Selective duplication of Critical packets

When `capacity - encoder_bitrate > threshold`:
- Duplicate all Critical-priority packets (SPS/PPS/VPS, IDR, Sequence Headers)
  across the 2 best links
- Overhead: ~10-20% of total bandwidth
- Benefit: zero-latency recovery from I-frame loss on any single link

### G.2 — Dynamic FEC rate scaling

Use spare bandwidth to increase RLNC and/or bonding-layer FEC:
- Default: 10% overhead
- Spare bandwidth mode: up to 30-50%
- Better burst loss recovery at cost of slight added latency

### G.3 — BitrateAdapter mode switch

Add a `ReliabilityMode` to BitrateAdapter:
- **MaxQuality:** push encoder bitrate up, minimal FEC overhead
- **MaxReliability:** cap encoder at "visually lossless" threshold (e.g., 6 Mbps
  for 1080p HEVC, 15 Mbps for 4K HEVC), divert spare capacity to
  duplication + FEC
- Trigger: when marginal quality gain per Mbps falls below protection value
- Heuristic: if encoder already at 80%+ of ceiling and spare > 3 Mbps, switch

---

## Priority Summary

| Phase | Effort | Impact | Dependencies |
|---|---|---|---|
| **A: Real Capacity Estimation** | Medium | Critical — everything else depends on it | None |
| **B: DegradationStage wiring** | Small | High — graceful degradation under pressure | None |
| **C: VBV init at max bitrate** | Tiny | Medium — prevents encoder instability | None |
| **D: H.265 codec support** | Medium | High — 4K over cellular, ~40% bitrate savings | A (for bitrate ceiling accuracy) |
| **E: Integration tests** | Medium | High — regression prevention, CI confidence | A, B |
| **F: Receiver telemetry** | Medium | Medium — better adaptation accuracy | A |
| **G: Spare bandwidth redundancy** | Medium | Medium — reliability polish | A, D |

**Recommended execution order:** A → B + C (parallel) → E → D → F → G

Phases B and C are independent of A and can be done in parallel. E should come
after A so that integration tests validate the new capacity estimation. D is the
biggest effort but unlocks the most value. F and G are refinements that build on
the foundation.

---

## What We're NOT Doing (and why)

- **AV1 support:** Edge hardware only has H.265 encoding. AV1 OBU parser,
  svtav1enc integration, and AV1 SVC are all deferred until hardware with
  AV1 encode capability is available. The CodecController abstraction
  (Phase D.1) ensures AV1 can be added later without restructuring.
- **Full Biscay radio feed-forward (SINR/CQI/RSRP):** Requires real cellular
  modems. The BiscayController supports it, but wiring it needs actual QMI/MBIM
  hardware. Park until deployment hardware is available.
- **eBPF TC hooks for sub-ms impairment:** Over-engineering for current needs.
  tc netem is sufficient for testing at ms resolution.
- **Mahimahi trace replay:** Nice-to-have for simulation realism but not
  blocking. Can be added incrementally to strata-sim.
- **VMAF/PSNR decode metrics in CI:** Too compute-heavy for every CI run.
  Nightly job only when we have the test harness.
- **Rate-distortion-reliability optimization:** Academic optimization. The
  simple heuristic in G.3 is sufficient for production.
