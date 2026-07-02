# Review Findings — Control-Loop Audit (Fable 5, 2026-07-01)

Scope per [review_plan.md](review_plan.md): magic numbers and control-algorithm
structure in `strata-bonding` / `strata-transport`. Audit only — no code
changes made. Every pre-found lead was independently re-verified against the
source (and git history where the intent was ambiguous).

Severity scale: **nit** / **worth-a-comment** / **worth-a-fix**. Items marked
**worth-a-fix (high)** are the ones I'd land first.

---

## Part 0 — Pre-found leads: verdicts

### L1. `congestion.rs:845` — `pacing_rate *= 0.7` on Normal→Cautious — **confirmed near-dead code → delete**

Verified exactly as the plan describes. `on_radio_metrics` (line 831) calls
`evaluate_state_transition()`, whose Normal→Cautious edge multiplies
`pacing_rate` by 0.7 in place, then line 835 unconditionally calls
`update_pacing_rate()`, which rebuilds `rate` from `btl_bw`/phase, applies its
*own* `Cautious → rate *= 0.7` (line 940), and overwrites
`self.pacing_rate = rate.max(10_000.0)` (line 955). Line 845's write survives
only when `update_pacing_rate` early-returns (SlowStart with `btl_bw <= 0`,
line 921) — i.e. it dampens the 100 KB/s bootstrap constant during
pre-calibration Cautious, a state that in practice can't even be reached
(see L9: the radio feed-forward that drives these transitions has no live
caller). **Verdict: (a) — delete line 845**; the one edge case it covers is
worthless (dampening a bootstrap constant) and the line reads as an accidental
double-application to anyone tracing a Cautious transition. If kept, it needs
a two-line comment. *worth-a-fix*

### L2. `adaptation.rs:524-529` — stale EWMA comment — **confirmed: code is intent, comment is stale**

Git history settles it: commit `9c0286a` introduced
`CAP_EWMA_ALPHA_DOWN = 0.7` with the matching comment; commit `e76a40e`
("fix(adaptation): stabilize capacity estimation and prevent target collapse")
deliberately changed the constant 0.7 → 0.5 and did not touch the comment.
The 0.5 is the tuned survivor of a real stabilization fix — fix the comment
to say α=0.5, not the constant. *worth-a-comment*

### L3. `adaptation.rs:834` / `:920` — `ewma_loss_fec > 0.08` context gate — **confirmed: the anti-pattern re-entering through a side door**

The use is narrower than the removed `> 0.15` gates (context flag, not a
direct cut), but the context is **not independent evidence**. By the code's
own comments (lines 886-894), `loss_after_fec` / `ewma_loss_fec` fold in
**late-arrival loss**. So:

```rust
let late_pressure = feedback.late_rate > 0.05 && jitter_loss_context;
```

is partially self-confirming — a pure late/reorder spike raises `late_rate`
*and* the residual that satisfies `jitter_loss_context`, so the "requires
independent loss evidence" guard is satisfied by the late packets themselves.
`late_pressure` then feeds `delay_pressure` → sustained → a cut that
additionally falls back to the **static** floor (line 1125 includes
`late_pressure` in the min-floor list). Same story for the
`jitter_growth_ms > 120 && jitter_loss_context` arm: a late/reorder event
inflates jitter-buffer depth and the residual simultaneously.

The 1.5 s sustain gate and the wiki's "pure jitter without residual can't be
fixed by cutting the encoder" argument blunt this, but the *evidence* half of
the conjunction is the exact signal the branch's three fixes disqualified.
**Suggested direction:** gate the context on channel-side evidence
(`max_link_loss`, already computed each tick) instead of the residual, or
document why residual-as-context is acceptable here when it wasn't at 0.15.
Also: `jitter_loss_context_pre` (line 834) and `jitter_loss_context`
(line 920) are the same expression computed twice per tick against
pre-/post-update EWMA values — if that staleness split is deliberate it needs
a comment; if not, it's drift waiting to happen. *worth-a-fix*

### L4. `adaptation.rs:691` / `:843` — duplicated `loss_rate >= 0.55 && queue_depth >= 60` — **confirmed duplicate; conjunct is defensible but under-justified**

Verbatim-identical expression in `update()` (`per_link_collapse`, suppresses
ramp-up) and `update_with_feedback()` (`link_collapse`, eligibility for a hard
cut with static-floor fallback). Since `update_with_feedback` *calls*
`update()`, the same tick evaluates it twice with the same inputs — pure
duplication, and the two copies can silently drift.

Is `>= 60` the disproven bufferbloat proxy? Partially. Unlike the removed
standalone `>= 90` gate, here queue depth is ANDed with ≥55 % channel loss, so
a benign IDR burst alone can't trip it. But the wiki's own analysis
([Adaptation-Delay-Pressure](wiki/Adaptation-Delay-Pressure.md)) established
that the paced queue routinely exceeds these counts during *healthy* bursts —
so the conjunct adds little discrimination on top of the loss term (a link at
55 % loss is melting whether 59 or 61 packets are queued), while re-using a
signal the codebase has explicitly documented as untrustworthy. **Suggested
direction:** extract one named helper (`fn link_melting(&LinkCapacity) ->
bool`) so the two sites can't drift, and consider replacing the raw-count
conjunct with the established honest signal (per-link AQM-drop delta) or
documenting why raw count is acceptable inside this conjunction.
*worth-a-fix (consolidation), worth-a-comment (threshold)*

### L5. `fec.rs` `GilbertElliott` — **confirmed dead — and the finding is bigger: the entire module is dead**

Repo-wide search: `GilbertElliott`, `fec_multiplier`, `is_bad()` have zero
call sites outside `scheduler/fec.rs` (the integration test's
`GilbertElliottLoss` is an unrelated local struct). But so do **`FecEncoder`,
`FecBlockDecoder`, `ProtectionLevel`, `FecConfig`, `set_overheads`, and
`split_source_block`** — nothing outside `scheduler/fec.rs` references any of
it. The live FEC path is entirely different: `adaptation.rs::
recommended_fec_overhead` → GStreamer `fec-overhead` message →
`TransportLink::set_fec_overhead` ([net/transport.rs:1479](crates/strata-bonding/src/net/transport.rs#L1479))
→ `strata-transport` `Sender::set_fec_rate(K=32, R)` (RLNC, not RaptorQ).

Consequences:
- The module's doc header describes RaptorQ UEP with 50 %/10 % overhead tiers
  as if it were the product. It is not; it's an abandoned parallel design.
- The plan's §1 concerns about `fec.rs:42-44` vs `:69-70` dual definitions and
  the `fec.rs:132` `× 2.5` High:Low stacking layer are **moot** — that code
  never runs.
- ~450 lines + tests of maintenance surface and reviewer-misdirection cost
  (this very review plan budgeted it as file #4 of 7).

**Verdict: delete the module** (or move to a research/examples area with a
"not wired" banner). This is the same disease the 2026-05-29 architecture
review named: "documented architecture overstates the live one."
*worth-a-fix (high)*

### L6. `oracle.rs:171` — `lower_bound_peak * 0.4` floor — **confirmed implicated in the phantom-capacity incident; the floor itself was never revisited**

The reference exists, in the code itself: `set_broadcast_active`'s doc
([oracle.rs:403-408](crates/strata-bonding/src/scheduler/oracle.rs#L403)) and
[bonding.rs:823-829](crates/strata-bonding/src/scheduler/bonding.rs#L823) both
name "the 40 %-of-peak floor" as the mechanism that *trapped* the
broadcast-contaminated estimate ("cap_kbps reports a phantom 2-4× the real
physical capacity"). The fixes that landed (broadcast/probe suppression,
`ORACLE_SANE_BTLBW_MULT` defense in transport.rs) all cut off *contamination
sources*; the trap mechanism is untouched. Residual exposure:

- `lower_bound_peak` is a **lifetime high-water mark with no decay**. Its only
  reset path is `reset_on_downshift()`, which requires a 3× RTT spike
  (+10 s cooldown). A genuine slow capacity decline — cell loading, SINR
  drift, band change without an RTT signature — leaves the floor pinned at
  40 % of best-ever indefinitely, and `recompute()` propagates that floor
  straight into `estimated_cap` whenever confidence is low.
- Contrast: `peak_estimate` (the *other* peak in the same file) decays 1 %/s
  precisely because a non-decaying peak was recognized as a hazard there.

**Suggested fix:** give `lower_bound_peak` the same slow decay as
`peak_estimate`, or compute the floor against a windowed (e.g. 60 s) peak.
*worth-a-fix*

### L7. `transport.rs:556-565` — token-debt comment — **confirmed fine; comment is misleading, code is correct**

The plan's premise is subtly wrong. The guard `p.tokens >= 0.0` admits a
packet whenever the balance is non-negative and *then* subtracts `len` — so
the bucket can legitimately go negative by up to one packet (~1 MTU;
paced-queue entries are individual packets, GSO concatenation happens later in
`send_batch`). That **is** the "allow negative balance up to 1 MTU" debt
allowance the comment describes. What misleads is the phrasing "if we have
tokens, OR if we have a minimum burst debt" — there is no second condition;
the debt allowance is an emergent property of check-then-subtract. Reword the
comment to say so. *worth-a-comment*

### L8. `oracle.rs:295` vs `config.rs` `failover_rtt_spike_factor` — **confirmed: two independent 3× detectors; the config knob does not reach the oracle**

They are semantically different detectors that happen to share the number 3:

- [bonding.rs:806-812](crates/strata-bonding/src/scheduler/bonding.rs#L806)
  reads the config field and compares this tick's smoothed RTT against the
  **previous tick's** value → triggers the fast-failover broadcast.
- [oracle.rs:295](crates/strata-bonding/src/scheduler/oracle.rs#L295)
  hardcodes `* 3.0` against a slow EWMA **baseline** (α=0.05) → triggers the
  capacity downshift reset.

So yes: an operator changing `failover_rtt_spike_factor` in a deployed config
silently does nothing to downshift detection. Either thread the config value
into `should_reset` (renaming it if the two checks must stay independent) or
document on the config field that it governs failover only. Related
controller-level concern in Part 2 (§2.4): the failover trigger itself is a
**single-sample** decision with bond-wide consequences. *worth-a-fix*

---

## Part 0b — New findings (not in the plan)

### N1. `congestion.rs:851-855` — RSRQ guard compares RSRP — **always-true condition (real bug, currently defused by dead wiring)**

```rust
// RSRP slope < -2.5 dB/s AND RSRQ < -12 → PRE_HANDOVER
let latest_rsrp = self.rsrp_history.back()...;
if rsrp_slope < -2.5 && latest_rsrp < -12.0 {
```

The comment (and the state-machine diagram at the top of the file) say the
second conjunct is **RSRQ** < -12 dB. The code tests **RSRP**, which is in
dBm and sits at ~-70…-120 on any attached LTE modem — the condition is
*always true*. The intended interference guard is unimplemented;
PreHandover triggers on slope alone. Compounding it, `rsrp_slope_db_per_sec`
is a first-to-last difference over as few as **2 samples**, so two noisy RSRP
readings 2.5 dB apart within a second → PreHandover → pacing × 0.1 (drain
mode), `can_enqueue() == false`, and on exit a full BBR reset
(`btl_bw = 0`, SlowStart → ~9 s recalibration under the scheduler capacity
floor). Today this whole path is unreachable (N2), which is the only reason
it hasn't burned a field run — it's a landmine for whoever wires the modem
supervisor in. Fix the field to `rsrq_db` (store it or read it from the
latest metrics) and require ≥3 slope samples. *worth-a-fix (high)*

### N2. Radio feed-forward has **no live caller** — the Biscay state machine is dead code in production

`BondingScheduler::notify_rf_metrics` ([bonding.rs:939](crates/strata-bonding/src/scheduler/bonding.rs#L939))
has zero callers (grep + call-graph both confirm; the only other mention is a
doc comment in `modem/health.rs`). Therefore `on_radio_metrics`, the
Normal/Cautious/PreHandover transitions, CQI-drop tracking, and the
`sinr_to_capacity_kbps` ceiling never execute outside unit tests. This is the
headline diagram of `congestion.rs` ("Master Plan §5") and it is unreachable —
the same overstatement disease as L5, in the file that most needs to be
readable. Either wire the modem supervisor in (and first fix N1) or cut the
state machine down to what runs. *worth-a-fix (high, as a decision — code
change either way is mechanical)*

### N3. `config.rs:331-332` — `congestion_headroom_ratio` / `congestion_trigger_ratio` are **dead config knobs**

Zero reads outside `config.rs` (definition, default, resolve, tests). An
operator can set them in TOML; nothing consumes them. Meanwhile the *live*
equivalents are `AdaptationConfig::headroom` (0.15) and
`pressure_threshold` (0.9) — note `1 - 0.85` and `0.90`, the same numbers,
strongly suggesting these SchedulerConfig fields are the abandoned older
home of the pair. Worse than L8: at least L8's knob does *something*.
Delete them (or wire them). *worth-a-fix*

### N4. `adaptation.rs:940` — `jitter_buffer_ms > 3000` hardcodes the receiver's *default* playout ceiling

The receiver's max playout window is config-tunable
(`ReceiverConfig::max_latency`, default 3000 ms — [config.rs:292](crates/strata-bonding/src/config.rs#L292)).
The adapter's delay-pressure arm hardcodes `> 3000`. Deploy with
`max_latency = 5 s` and the adapter starts cutting the encoder the moment the
jitter buffer legitimately uses the window the operator granted; deploy with
`max_latency = 1.5 s` and the "overflow" arm can never fire. Same
misleading-config class as L8. Sender and receiver configs are separate
processes, so this needs either a plumbed value or a documented contract.
*worth-a-fix*

### N5. Tick-count sustain constants silently rescale with `stats_interval_ms`

`AQM_SUSTAINED_TICKS = 2`, `ZERO_CAP_COLLAPSE_TICKS = 2`,
`over_pressure_ticks >= 2`, `consecutive_increases >= 3` (adaptation.rs) are
all **tick counts**, but the tick is the strata-gst stats thread whose period
is a *config field* (`stats_interval_ms`, default 1000 — the adapter comments
assume "~1s/tick", and one still says "500ms update intervals",
[adaptation.rs:273](crates/strata-bonding/src/adaptation.rs#L273)). An
operator halving `stats_interval_ms` for snappier telemetry silently halves
every sustain window in the encoder loop — the opposite of what the
sustain-gate fixes on this branch were for. Convert to wall-clock durations
(the `congestion_started` pattern already in the same file) or document the
coupling on `stats_interval_ms` itself. *worth-a-fix*

### N6. `congestion.rs` drain_factor floor: three contradictory documented values

Field doc says "decays toward **0.1**" (line 213-215); the getter doc says
"(**0.2**–1.0)" (line 522); every code path floors at **0.5** (lines 365,
393, 776, 779). The code is self-consistent; both docs are stale, presumably
from two earlier tunings. *worth-a-comment*

### N7. `adaptation.rs:670-676` — "consecutive increases" counts *flat* capacity, and 0.90–0.95 is a dead zone

`aggregate >= prev * 0.95` increments `consecutive_increases` — so a
perfectly flat capacity "increases" every tick, and the ramp gate
`consecutive_increases >= 3` is really "≥3 ticks without a notable decrease".
That behavior (ramp on stability) is probably what you want, but the name and
the plan-level description ("a run of increasing capacity") say something
stronger than the code checks. Meanwhile a tick in the 0.90–0.95 band
increments *neither* counter, freezing both trends. Rename or comment.
*worth-a-comment*

### N8. `queue_building()` doc premise correction (plan §1)

The plan cited `queue_building()` as "requires ≥4 consecutive samples" and
contrasted other single-sample thresholds against it. Actually it requires
only `rtt_samples.len() >= 4` (a warm-up), then evaluates the **latest single
sample** against the MASD-scaled trip. It is *not* a multi-sample sustain —
it's a single-sample test with a path-relative threshold. The plan's contrast
stands (path-relative beats fixed constants) but the sustain claim shouldn't
be repeated in future docs. *nit*

### N9. `transport.rs:1461` — `PROBE_FEEDBACK_COOLDOWN * 100` sentinel

"Hold the block open with a far-future deadline" = 150 s dressed up as
arithmetic. If a probe ever fails to call `set_saturation_probe_active(false)`
(crash/early-return path), receiver feedback is silently ignored for 150 s.
Use an explicit `Option`/state instead of a magic far-future Instant, or name
the sentinel. *nit*

---

## Part 1 — Magic-number inventory (beyond the leads)

The bar (per the plan) is `net/transport.rs`'s named-const style —
`STARVED_CAPACITY_FLOOR_BPS`, `ORACLE_SANE_BTLBW_MULT` with compile-time
asserts. That file is genuinely the model; the gaps are elsewhere.

### 1a. Bare literals that gate control decisions (no name, no doc)

| Location | Literal | Gates | Note |
|---|---|---|---|
| adaptation.rs:604-608 | `2.0` / `5.0` | pressure sentinels for "zero capacity" / "no links" | arbitrary sentinels flow into `DegradationStage::from_pressure(1/p)`; name them |
| adaptation.rs:643 | `+ 0.05` | self-congestion pressure bump past threshold | |
| adaptation.rs:670/673 | `0.95` / `0.90` | capacity trend up/down bands | see N7 dead zone |
| adaptation.rs:703 | `0.80` | "at ceiling" for MaxReliability switch | |
| adaptation.rs:709 | `* 1.2` | MaxReliability → MaxQuality hysteresis | pairs with `quality_cap_kbps` |
| adaptation.rs:762 | `50` kbps / `0.10` | command-commit threshold | |
| adaptation.rs:795 | `200` kbps / `0.10` | grace-arming threshold | |
| adaptation.rs:830-856 | `160` ms | jitter-growth increase-revert | |
| adaptation.rs:906-913 | `0.35`, `0.7`, `0.50`, `200` ms | burst / severe-burst definition | the 0.7 goodput conjunct is the new (good) fix; still unnamed |
| adaptation.rs:917 | `* 0.8` | burst EWMA lift | |
| adaptation.rs:926 | `0.05` | late-rate threshold | see L3 |
| adaptation.rs:939-940 | `120` ms, `3000` ms | delay pressure | 3000 → N4 |
| adaptation.rs:1007 | `0.85` | goodput-ceiling clamp trigger | |
| adaptation.rs:1018-1026 | `0.7` / `0.5` | goodput shortfall / severe tiers | |
| adaptation.rs:1037 | `5` s | feedback grace | promote or name; interacts with ramp cadence |
| adaptation.rs:1114 | `min(0.55)` | severe-burst cut factor | |
| adaptation.rs:1257 | `10` s | burst cooldown | named only in comment |
| adaptation.rs:1263/1275 | `- 0.05` | ramp-up hysteresis gap | commented, unnamed |
| adaptation.rs:1281 | `1.3` | ramp ceiling vs peak goodput | |
| adaptation.rs:1330-1332 | `0.5` / `0.8` | dynamic floor (peak-half / EWMA cap) | commented well, unnamed |
| congestion.rs:261-262 | `100_000.0`, `14_000.0` | bootstrap pacing / cwnd | |
| congestion.rs:363-365 | `0.05`, `3.0`, `8.0` | gradient severity → drain decay | throttled to 100 ms; borderline OK |
| congestion.rs:393 | `0.9` | modem flow-control drain step | **no rate-limit** — per-call decay depends on caller frequency, unlike the 100 ms-throttled siblings; floor 0.5 bounds the damage |
| congestion.rs:774-779 | `5.0`, `3.0`, `1.5`, `0.85`, `0.95` | RTT-ratio fallback drains | explicitly doctrine-flagged in its own comment; fine as fallback |
| congestion.rs:929/934/940-941 | `1.25`, `0.5`, `0.7`, `0.1` | BBR gains / state dampening | standard BBR-ish values; name them |
| congestion.rs:961 | `2800.0` | min cwnd "2 packets" | 2×1400, derive from SYMBOL/MTU const |
| congestion.rs:1036-1055 | SINR step table | pacing ceiling | coarse 2× steps; currently dead (N2) |
| oracle.rs:145/239/375 + transport.rs:1153/1254/1285 | `100_000.0` | "have a meaningful baseline" cutover | **six copies** of the same 100 kbps epsilon across two files — one named const |
| oracle.rs:275 | `0.5` | downshift lower-bound haircut | |
| oracle.rs:291 | `10.0` s | downshift cooldown | |
| oracle.rs:343 | `0.999` | peak decay (~1 %/s at 2 Hz) | comment assumes tick rate — cadence-coupled like N5 |
| oracle.rs:375-380 | `3.0`, `50_000_000.0` | PPD sanity cap / absolute ceiling | 50 M also at transport.rs:1259 — silently caps >50 Mbps links (5G) in two places |
| transport.rs:516 | `0.2` | pacing floor vs oracle peak | commented; unnamed |
| transport.rs:547 | `0.01`, `10_000.0` | token-bucket burst cap | |
| transport.rs:791/802 | clamps `250-1000 ms`, `4×SRTT`, `500-2000 ms` | ACK-rate sampling windows | |
| transport.rs:1092 | `500_000` µs | per-link ack-rate window | |
| transport.rs:1255/1289 | `1.5` / `1.2` | btl_bw vs ack-rate cap / ack fallback headroom | adjacent, different, both unnamed — were they co-tuned? |
| bonding.rs:231 | `0.025` | default OWD seed 25 ms | |
| bonding.rs:339 | `1600` ms | probe recv-wait max | |
| bonding.rs:594 | `0.25` | min probe window fraction | |

### 1b. The EWMA zoo

Twelve-plus independently-tuned EWMAs doing "noisy signal, asymmetric trust",
no shared constants, no cross-reference:

| Signal | α (rise / fall or single) | Where |
|---|---|---|
| per-link capacity | 0.3 / 0.5 | adaptation.rs:528 |
| post-FEC loss | 0.3, stall-decay ×0.9 | adaptation.rs:881-884 |
| goodput | 0.3, stall-decay ×0.8 | adaptation.rs:951/983 |
| oracle lower bound | 0.3 / 0.05 | oracle.rs:157 |
| oracle baseline RTT | 0.05 | oracle.rs:312 |
| socket rate | 0.2, idle-decay ×0.98 | transport.rs:1025/1031 |
| goodput (link) | 0.5 | transport.rs:917 |
| ACK rate (global) | 0.2 | transport.rs:816 |
| ACK rate (per-link) | 0.2, stall-decay ×0.5 | transport.rs:1120/1126 |
| delay gradient + jitter | 0.2 | congestion.rs:337-342 |
| RTT MASD | 0.3 | congestion.rs:699 |
| regime loss | 0.1 | congestion.rs:310 |
| scheduler ewma_alpha | 0.125 (config) | config.rs:370 |

Not proposing they converge to one value — they smooth different cadences —
but the *rise-fast/fall-slow vs rise-slow/fall-fast* polarity flips between
files depending on whether the signal is a capacity (trust drops) or a floor
(trust rises), and nothing states that rule. One doc comment defining the
polarity convention + named constants for the recurring 0.3/0.05 and 0.2
pairs would prevent the next L2-style comment drift. *worth-a-fix (docs +
naming pass)*

### 1c. Stacked scaling layers on one quantity

- **FEC overhead**: after L5, the live chain is clean —
  `recommended_fec_overhead` (0.10–0.50) → `R = round(32·ratio).clamp(1,32)`.
  Note the quantization: overhead resolves in ~3.1 % steps with a 3.1 % floor
  even when 0 is wanted (deliberate, per the R≥1 comment).
- **pacing_rate**: `btl_bw × gain` → Cautious/PreHandover dampening →
  SINR ceiling → `drain_factor` → 10 KB/s floor (congestion.rs) → **then**
  transport.rs applies `max(peak_cap × 0.2)` floor → `rtt_bufferbloat_throttle`
  (0.25–1.0). Two files, two floors, two delay-based reducers
  (`drain_factor` and `rtt_throttle` both respond to RTT-above-baseline —
  they multiply: 0.5 × 0.25 = 8× reduction from the same bloat event).
  The comments in `flush_paced` do order them deliberately (floor before
  throttle), but the two delay reducers double-counting one queue is
  unacknowledged. *worth-a-comment, and a candidate consolidation*
- **capacity**: oracle bounds → `ORACLE_SANE_BTLBW_MULT` rejection →
  `btl_bw.min(ack_rate×1.5).clamp(100k, 50M)` → adaptation drain-clamp →
  loss discount → EWMA → headroom. Six layers, each individually justified;
  the 50 Mbps ceilings (two copies) are the only ones likely to bite later.

---

## Part 2 — Control-loop interaction review

### 2.1 Double-counting a degradation event

The 0.7s are a genuine coincidence of value but **not** a same-signal
double-count: `ramp_down_factor = 0.7` cuts the *encoder* on
pressure/feedback; Cautious `× 0.7` cuts one link's *pacing* on CQI (and is
currently unreachable, N2). They compose sequentially — pacer cuts drain →
drain clamp lowers usable → encoder over-pressure cut → ~0.49 combined — but
that's the intended "encoder follows the honest drain rate" design, and the
capacity path reacting to a *changed drain rate* is not double-counting.

The double-count that **does** exist is delay: `drain_factor` (gradient/RTT
paths in congestion.rs) and `rtt_bufferbloat_throttle` (transport.rs) both
multiply pacing down in response to RTT-above-floor (§1c). And within one
adapter tick, a severe burst can legitimately cut twice
(`capacity_already_cut` is bypassed by `severe_burst` — documented as an
emergency; fine).

### 2.2 The arbitration boolean pile — consolidate

Confirmed the plan's suspicion, and it's worse than "one guard per fix":
`current_target_kbps` is mutated in **four distinct places** in a single
`update_with_feedback` tick — (1) `update()`'s commit (guarded by
`target_changed && interval_ok`), (2) the jitter-growth increase-revert
(line 853, bypasses those guards), (3) the goodput-peak ceiling clamp
(line 1009, sets `last_command_time` directly), (4) the feedback cut
(line 1133). Each writes its own subset of the bookkeeping
(`last_increase_time`, `last_burst_time`, `last_command_time`,
`consecutive_*`), and the guards (`feedback_grace`, `increased_this_tick`,
`capacity_already_cut`, `allow_feedback_cut`, `burst_cooldown`,
`congestion_started`, `self_congested`, `zero_capacity_ticks`,
`over_pressure_ticks`) are pairwise-consistent only by careful reading.
It still *works* — the mega info-log at line 1082 is the tell that debugging
it requires dumping 20 booleans — but the next fix lands on a pile where
correctness is O(guards²). **Recommendation:** collect the tick's evidence
into a struct, rank the pressure signals (LinkFailure > severe_burst >
sustained congestion > capacity > ramp-up), and commit the target **once** at
the end through a single function that owns all bookkeeping. This is the
plan's "unify into one ranked decision", and I'd rate it the highest-value
refactor in the file. *worth-a-fix (high)*

### 2.3 Additive-up / multiplicative-down asymmetry — deliberate, undocumented

The asymmetry is AIMD-shaped and consistent with every comment ("recover
cautiously, cut decisively"), so I read it as intent — but nowhere stated.
Worked numbers worth writing down next to `ramp_up_kbps_per_step`: a cut from
3 Mbps → 2.1 Mbps (one 0.7 step, instant) takes ≥4 ticks (~4 s) of +250 to
recover *if* gates stay open; a collapse to the 500 floor takes ~10 s of
clean ticks to re-reach 3 Mbps, and any single `raw_signal` tick resets the
sustain clock while `burst_cooldown` (10 s) can re-arm on the way. That
recovery-time-linear-in-gap behavior is the field-visible "slow climb after
every dip" and should be a named design choice, not archaeology.
*worth-a-comment*

### 2.4 Cooldown/grace timescale table

| Loop | Reaction | Confirmation | Recovery |
|---|---|---|---|
| pacer drain (congestion.rs) | 100 ms throttled | none (single sample vs path-relative trip) | +0.05/100 ms → ≤1 s from floor |
| encoder adapter | 1 s tick | 1.5 s sustain (2 ticks); severe bypass | +250 kbps/tick; 5 s grace; 10 s burst cooldown |
| FEC sizing | 1 s (rides adapter) | none (per-tick `max_link_loss`) | instant down |
| oracle | continuous | 30 s confidence half-life | 10 s downshift cooldown |
| failover broadcast (bonding.rs) | 1 s tick | **none — single tick** | 3 s + 0.5 s suppression tail |
| probe feedback block | — | — | 1.5 s cooldown |

Ordering is mostly correct (inner loops faster than outer). Two exceptions:

1. **Failover is the most disruptive action with the least confirmation.**
   One tick where smoothed RTT ≥ 3× the previous tick's (or one
   Live→Degrade phase edge) → *every* packet duplicated to *all* links for
   3 s, plus oracle suppression. On jittery cellular, a 20 ms → 60 ms tick
   is routine. Duplication doubles offered load exactly when a link wobbles —
   the same self-amplification family as the FEC death spiral (and the config
   comment at config.rs:355-359 already admits duplication "makes bursty
   congestion worse"). The encoder needs 1.5 s of sustained evidence to cut
   30 %; failover needs 1 sample to double the bond's load. Give it the same
   sustain treatment (or gate on ≥2 consecutive spike ticks).
   *worth-a-fix (high)*
2. **FEC sizing has no sustain**: `max_link_loss` is a per-tick max of
   per-interval loss; one bursty second lifts overhead (`loss × 0.5`, plus
   the `>= 0.25 → ≥25 %` step) for exactly one tick, injecting a parity
   burst. The self-congestion pin protects the congestive case; a plain
   HARQ burst still gets a one-tick 2×+ parity spike. An EWMA (or reusing
   `congestion_sustain`) on the FEC input would match the branch's doctrine.
   *worth-a-fix*

Also: adapter grace (5 s) vs receiver-report cadence (1 s) is comfortable,
and `PROBE_FEEDBACK_COOLDOWN` (1.5 s) matching `congestion_sustain` (1.5 s)
appears coincidental — worth one line saying whether they must stay equal.

---

## Part 3 — Config-centralization consistency

Misleading pairs (config field + hardcoded sibling) — the actively harmful
class:

1. `failover_rtt_spike_factor` vs oracle.rs:295 hardcoded 3.0 (L8).
2. `congestion_headroom_ratio` / `congestion_trigger_ratio` — dead knobs, the
   live pair lives in `AdaptationConfig` (N3).
3. `ReceiverConfig::max_latency` vs adaptation.rs:940 hardcoded 3000 ms (N4).
4. `stats_interval_ms` silently rescales all adapter tick-count sustains (N5).

Promotion candidates (tunables in behavior, constants in code) — flag only,
per plan; the config surface is a maintainer decision:

- `adaptation.rs`: feedback grace (5 s), burst cooldown (10 s), slew cap
  (15 %), goodput-shortfall ratios (0.7/0.5), burst thresholds (0.35/0.5),
  AQM trio (5 drops / 2 ticks / 0.7 pressure). These moved twice each during
  field tuning — history says they're tunables.
- `oracle.rs`: 40 % floor (L6), downshift cooldown (10 s), confidence
  half-life (30 s).
- Fine as pinned constants: everything in `net/transport.rs`'s named-const
  block (already exemplary), wire-format sizes, BBR gains (1.25/0.5), the
  RTT-ratio fallback table (self-documented as fallback-only).

Also noting for the workspace docs (not code): `CLAUDE.md` states
`capacity_floor_bps = 5 Mbps`; the actual default is **1.5 Mbps**
(config.rs:372).

---

## Suggested landing order

1. **N1** RSRQ/RSRP bug + **N2** decision on the dead radio feed-forward
   (they're one work item: wire it fixed, or cut it).
2. **L5** delete dead `scheduler/fec.rs`.
3. **2.2** adapter decision consolidation (ranked pressures, single commit).
4. **2.4.1** failover sustain gate.
5. **L6** `lower_bound_peak` decay; **L8/N3/N4/N5** config-vs-hardcode
   reconciliation (one small PR).
6. **L3/L4** context-gate and link-melt helper.
7. Comment/naming pass: L1, L2, L7, N6, N7, §1b polarity doc.
