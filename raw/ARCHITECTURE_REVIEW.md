# Strata — Top-Down Architectural Review

**Date:** 2026-05-29
**Scope:** Whole system, not just the receiver HLS stage. Written after the
`field-test-16983` finding that the TS-passthrough fix stopped the crash but
broke segmentation — a symptom, not the disease.

---

## 0. TL;DR

Strata is an **ambitious real-time bonded-transport stack** being used for a
**store-and-forward broadcast** job (YouTube HLS ingest, which adds 5–30 s of its
own buffering downstream). Most of the pain in the last ~16 field runs comes from
that mismatch, expressed three ways:

1. **Too many coupled feedback loops** (≈8) all reacting to the same noisy
   cellular signals on overlapping timescales → whipsaw. The fixes so far have
   been per-loop damping (slew caps, EWMA, floors), not decoupling.
2. **The documented architecture overstates the live one.** Several headline
   subsystems are dead or stubbed (`sbd`, Thompson sampling, radio feed-forward).
   They cost reading time, test surface, and credibility without affecting a
   single field packet.
3. **No clean architectural seam between "transport delivered bytes" and "egress
   produces a container."** The crash↔no-segment dilemma exists *because* the
   receiver's output stage reaches back into transport-domain artifacts (gap-skip
   DISCONTs, holed multiplex) instead of consuming a defined clean stream.

The single highest-leverage change is **not** another tuning knob. It is to
**split the system along its real use case** with an explicit *broadcast profile*
that turns the adaptive machinery down to a few stable loops, and to **define a
clean delivered-stream boundary** the egress can trust.

---

## 1. The use-case mismatch (root cause of the saga)

| | Designed for | Actually doing |
|---|---|---|
| Latency budget | sub-second glass-to-glass | YouTube HLS, 5–30 s downstream buffer |
| Playout window | adaptive, chases the mean to stay low | should be a fixed, generous floor |
| Bitrate | track capacity aggressively | a stable, slightly-conservative target is fine |
| Probing | discover capacity continuously | unnecessary; perturbs the very links it measures |
| Failover | fast broadcast on instability | rarely needed; downstream buffer hides reconverge |

The diagnosis docs (`STRATA_DIAGNOSIS.md`, `findings-report.md`) already converge
on this: the dominant artifact source was the **adaptive playout window sitting
below the real cellular OWD tail**, dropping ~3.2 packets/s as "late." The Phase-1
floor raise (start=1500/min=1000/max=3000 ms) treated it. But the floor is a
band-aid on a window that *should not be adaptive at all* for this use case.

**The competitor baseline note is correct:** srtla/BELABOX win here with ~3 loops
and a fixed latency. Strata's ~8 loops are a liability, not a feature, for
broadcast.

---

## 2. Live architecture vs documented architecture

> **Update 2026-05-29 — acted on.** The three dead/orphaned subsystems below
> (SBD, the modem supervisor, automated band-locking) and the doc-only Thompson
> sampling were **cut** (~2,034 LOC removed). The radio feed-forward *consumer*
> (Biscay `on_radio_metrics`) and the `RfMetrics` seam were **kept**; only the
> orphaned producer-side machinery was removed. `wiki/Architecture.md` was
> reconciled to match. Workspace builds clean; 352 bonding tests pass. See §7
> Tier C for the rationale.

`wiki/Architecture.md` described a much larger system than the one that runs in
the field. Verified against the code (pre-cut):

| Subsystem | Doc claims | Reality in code | Verdict |
|---|---|---|---|
| EDPF scheduler | core router | live (`bonding.rs`, 13 refs) | ✅ real |
| BLEST HoL guard | core filter | live (21 refs) | ✅ real |
| IoDS in-order | live | live (18 refs) | ✅ real |
| Kalman oracle | per-link capacity | live (16 refs) | ✅ real |
| **SBD (shared bottleneck)** | listed as scheduler feature | **0 refs in `bonding.rs`**; only surfaces in dashboard types | ⚠️ **dashboard-only, not on decision path** |
| **Thompson sampling** | "contextual bandit for link preference" | **not present in code at all** | ❌ **aspirational doc** |
| **Radio feed-forward** (SINR→ceiling, CQI derivative, handover detect) | 4 named extensions to Biscay | machinery exists (`modem/health.rs`, `kalman`) but **no field path pushes real QMI/MBIM metrics** — supervisor comment: "In production, an external poller … pushes" | ❌ **dormant; Biscay runs blind** |
| Biscay congestion (BBRv3 base) | live | live in `strata-transport/congestion.rs` | ✅ real (but blind, see above) |
| Hybrid FEC+ARQ | live | live (`rlnc.rs`, `arq.rs`) | ✅ real |

**Implication:** ~3 of the most complex, most-marketed pillars are not earning
their keep. They inflate the mental model every debugging session starts from,
and the radio feed-forward gap means Biscay is making decisions *without* the
exact signals the design says protect it from cellular collapse — which is
likely *why* startup-under-loss is still fragile.

---

## 3. The feedback-loop tangle

Eight control loops, all fed (directly or indirectly) by the same handful of
noisy cellular measurements, on overlapping 100 ms–8 s timescales:

```
            ┌─────────────────────── cellular link (noisy: HARQ tail, bufferbloat) ──────────────────────┐
            ▼                                                                                             │
 (1) Biscay CC ──pacing──► link queue ──┐                                                                │
 (2) Saturation probes ──perturbs──────►├──► loss / RTT / goodput  ──┐                                   │
 (3) Kalman oracle ──capacity est.──────┘                            │                                   │
                                                                     ▼                                   │
 (4) Encoder bitrate adapter (adaptation.rs, fed by capacity×(1-loss)) ──BITRATE_CMD──► encoder ─────────┘
 (5) TAROT FEC overhead ──competes for the same capacity──► more bytes ──► (1)(2)(3)
 (6) Failover detector ──broadcast──► doubles offered load ──► (1)
 (7) Receiver playout window (aggregator.rs: jitter + delay-spread + loss + late-pressure) ──► late drops
 (8) ARQ/NACK ──retransmits──► more bytes ──► (1)
```

Why this whipsaws (all observed in the field/diagnosis docs):

- **Loop 4 chases loop 3 chases loops 1+2.** Capacity estimate spikes (a probe
  briefly reads 7+ Mbps on 2×2 Mbps links) → bitrate jumps → over-pressure →
  loss → estimate collapses → bitrate collapses. Already patched with an up-only
  +15%/tick slew (`adaptation.rs::slew_clamp`) and asymmetric capacity EWMA —
  *damping a loop that shouldn't be this twitchy for broadcast*.
- **Loop 5 fights loop 4 for the same pipe.** FEC overhead and video bytes draw
  from one capacity budget; raising one starves the other, and both feed the loss
  signal that drives both.
- **Loop 7 is decoupled from 1–6 by design** (separate receiver machine) yet
  couples back through the metric: its "late" drops look like clean delivery to
  the wire metrics but like reference-frame holes to the decoder. The two halves
  of the system optimize against *different definitions of loss* (see §5).
- **Loops 2 and 6 actively make the links worse** to measure/protect them, then
  the other loops react to the self-inflicted perturbation. Both are now defaulted
  OFF for bonded-cellular (recent commits) — which is the right instinct, but it's
  being done knob-by-knob instead of as a coherent profile.

**The pattern:** every fix so far adds damping to an individual loop. The
architectural fix is to **remove or freeze loops that don't serve the use case**,
leaving a small set that can't fight each other.

---

## 4. The egress structural flaw (the immediate dilemma)

The receiver pipeline has **no defined boundary between "transport output" and
"container egress."** Today both failing options reach across that missing seam:

- **Re-mux path** (`tsdemux → h265parse → hlssink2`): segments correctly but the
  internal `mpegtsmux` dies on backwards/NONE DTS produced by gap-skips/holes in
  the transport multiplex → "Timestamping error on input streams."
- **Passthrough path** (`tsparse split-on-rai → hlssink`): can't be poisoned, but
  can't segment a network TS (no upstream encoder to answer hlssink's force-key-
  unit requests) → one ever-growing file.

Both are consequences of the same omission: **the egress consumes a stream still
carrying transport-domain damage** (DISCONT flags, holed PCRs, non-monotonic DTS).
The aggregator's job is to deliver a *clean, monotonic, in-order* stream; whatever
it can't deliver cleanly it should **conceal at the media layer** (hold for a
keyframe, or emit a well-formed gap) so that egress sees a stream indistinguishable
from a local encoder's output.

The right architecture is a **named `DeliveredStream` contract**:

```
transport+reassembly ──► [DeliveredStream: monotonic DTS, no mid-AU holes,
                          keyframe-aligned discontinuities only] ──► egress (dumb)
```

Once that contract holds, egress can be the *boring, reliable* re-mux path
(`h265parse → hlssink2` or `splitmuxsink`) and it will neither crash (no backwards
DTS reaches it) nor fail to segment (h265parse flags keyframes). The egress stops
being clever; the cleverness moves to where it belongs — the reassembly layer that
already has the sequence/keyframe metadata to do concealment.

---

## 5. Observability is actively misleading

From `findings-report.md`, confirmed in code:

- **Receiver `loss_rate` is an EWMA** (`0.95·prev + 0.05·instant`) — it *decays
  after a burst*, so the operator sees "loss=0.000" while the decoder is still
  paying for damage. It is not a run-health metric.
- **Sender `[link] loss=` is retransmission ratio**, a *different quantity* on a
  *different timescale* than receiver-visible media damage. The two halves report
  "different truths" that look contradictory.
- **"Late" drops are invisible** to every loss metric — they're the dominant
  artifact source yet don't show up as loss anywhere.
- **Routing truth is global, not per-link.** The adapter cuts bitrate during
  collapse while EDPF still treats the hurting link as alive and eligible (a link
  can show sender `loss=0.758` while the system keeps routing to it).

**You cannot tune what you mis-measure.** Several of the ~16 runs were spent
chasing metrics that were lying. This is an architectural problem (wrong signals
exported as the prominent ones), not a dashboard polish problem.

---

## 6. Configuration sprawl

**37 distinct `STRATA_*` env knobs**, no profiles. Every field run is a manual
re-assembly of a dozen interacting toggles (`field-test.sh` + `.env`), and the
"correct" combination for the broadcast use case is tribal knowledge encoded in
git history and memory files, not in the code. The probes-off / failover-off /
floor-raised settings that *actually work* aren't a named, defaulted thing.

---

## 7. Recommended changes (prioritized)

> **Update 2026-05-30 — Tiers A & B implemented.** All six items below landed,
> each built and unit-tested; A2 additionally loopback-validated. The egress is
> now keyed to three profiles (`broadcast` default for HLS, `low-latency` for
> RTMP/SRT, `realtime` for direct), reflecting the insight that the latency
> budget belongs to the *egress target*, not Strata — so the adaptive machinery
> is gated, not cut. Details inline. Tier C was already done (below).

### Tier A — Immediate, high-leverage, low-risk  *(DONE 2026-05-30)*

1. ✅ **`StreamProfile`** (`config.rs`): `broadcast` (HLS — fixed 1500 ms playout,
   probes & failover off), `low-latency` (RTMP/SRT — adaptive 400–1500 ms,
   failover on), `realtime` (direct — tight adaptive, probes on). Selected via
   `profile = "…"` in TOML / `STRATA_PROFILE`; explicit knobs still override.
   Collapses the ~8-loop tangle to a coherent per-egress operating point.

2. ✅ **`DeliveredStream` gate** — realized in the GStreamer *receiver* (not the
   aggregator: the keyframe signal is lost at the sender's mux boundary, so it's
   only reliable post-demux). `install_delivered_stream_gate` (`strata_pipeline.rs`)
   is a pad probe on the parsed video that (a) emits nothing until the first IDR,
   (b) after any DISCONT drops the damaged GOP until the next keyframe, (c) drops
   DTS regressions. With clean input a plain video-only re-mux both **segments**
   and **survives loss**. Loopback-validated: 3–4 segments + playlist, 0
   timestamping errors (vs the passthrough's 1 giant segment / the old re-mux's
   crash). Audio dropped for now (its startup DTS-interleave was a 2nd crash
   trigger); re-add behind its own gate once field-proven.

3. ✅ **Health metrics** — added cumulative non-decaying `damaged_packets`
   (lost + late) to `ReassemblyStats` + the `strata-stats` bus message + the
   field-test monitor headline; documented `loss_rate` as a smoothed CC-only
   signal. "Late" drops are now first-class.

### Tier B — Structural  *(DONE 2026-05-30)*

4. ✅ **Fixed-by-default playout, adaptive-by-opt-in** — `fixed_playout` flag on
   `ReassemblyConfig`/`ReceiverConfig`, set true by the broadcast profile; gates
   the entire jitter/spread/late-pressure adaptation block in `aggregator.rs`.
   Adaptive remains for the low-latency/realtime profiles.

5. ✅ **FEC budget decoupled from video** (`adaptation.rs`): usable video
   capacity now reserves `max(control_headroom, recommended_fec_overhead())`, so
   when FEC ramps up under loss the video target yields by the same amount
   instead of the two loops fighting for the same bytes.

6. ✅ **Per-link routing truth** (`edpf.rs`): added `SEVERE_LOSS_THRESHOLD` so a
   link drowning in *pure radio loss* (high loss, shallow queue) is shed from
   routing — the old collapse heuristics required loss **and** a deep queue, so a
   fading link kept getting traffic. Unit-tested (`pure_radio_loss_link_is_shed…`).

### Tier C — Debt / honesty  *(DONE 2026-05-29)*

7. **Delete dead subsystems.** ✅ Cut `scheduler/sbd.rs` (533 LOC, orphan — its
   NADA consumer was never built), `modem/supervisor.rs` (558, never
   constructed), `modem/band.rs` (943, needs AT-command control the NCM dongles
   don't expose), and the doc-only Thompson sampling + its dead dashboard
   widget/fields. Slimmed `modem/health.rs` to just the `RfMetrics` seam.
   **Kept** the Biscay `on_radio_metrics` consumer + `RfMetrics` type as the
   integration point for a future QMI/MBIM poller — that's the one piece with
   real low-latency-robustness value, and it's cheap to keep. Net −2,034 LOC,
   build clean, 352 tests pass.

8. **Reconcile `wiki/Architecture.md` with reality.** ✅ Removed Thompson
   sampling, marked radio feed-forward + band-locking as roadmap (gated on
   QMI-capable hardware), corrected the source-tree listing and dataflow
   diagram, fixed `wiki/Testing.md`.

9. **Collapse the two diagnosis docs + this review into a single living
   ARCHITECTURE doc** once Tier A/B land, so the next contributor starts from
   truth. ✅ DONE 2026-07-04 — durable content merged into
   [wiki/Control-Loop-Map.md](../wiki/Control-Loop-Map.md) and
   [wiki/Observability-Semantics.md](../wiki/Observability-Semantics.md);
   all three source docs archived to `raw/`.

---

## 8. What I'd do first, in order

1. `broadcast` profile (Tier A1) — fastest path to a working YouTube stream and
   removes most whipsaw.
2. `DeliveredStream` contract + dumb re-mux egress (Tier A2) — kills the
   crash/segment dilemma permanently.
3. Health-metric fix (Tier A3) — so you can *see* whether 1 & 2 worked.

Everything else is real but secondary. The first three turn Strata-for-broadcast
from "fighting itself" into "boring and reliable," which is exactly what a
store-and-forward ingest path should be. The full adaptive/bonded cleverness then
lives behind the low-latency profile where it's actually justified.
