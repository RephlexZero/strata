# Review Plan — Control-Loop Audit for Fable 5

Scope: the adaptive-bitrate / congestion / FEC control stack in
`strata-bonding` and `strata-transport`. Not a general code-quality pass —
this is a targeted audit of magic numbers and control-algorithm structure in
the crates that decide encoder bitrate, pacing, and FEC overhead in real
time. Read [hot.md](hot.md) and [wiki/Adaptation-Encoder-Cut-Signals.md](wiki/Adaptation-Encoder-Cut-Signals.md) /
[wiki/Adaptation-FEC-Sizing.md](wiki/Adaptation-FEC-Sizing.md) /
[wiki/Adaptation-Delay-Pressure.md](wiki/Adaptation-Delay-Pressure.md) first —
three bugs already fixed on this branch are the known instances of the
anti-pattern this review is hunting for more of: a control decision keyed off
an *instantaneous/derived* signal (post-FEC residual, raw queue depth) instead
of a *sustained/causal* one (channel loss, sustained goodput collapse).

A recon pass has already been done (see "Pre-found leads" below) so Fable
isn't starting cold — but every item there needs independent verification,
not just restating. Files in scope, priority order:

1. `crates/strata-bonding/src/adaptation.rs` — `BitrateAdapter`
2. `crates/strata-transport/src/congestion.rs` — `BiscayController` (BBR-based)
3. `crates/strata-bonding/src/scheduler/oracle.rs` — `CapacityOracle` (PPD)
4. `crates/strata-bonding/src/scheduler/fec.rs` — FEC overhead sizing
5. `crates/strata-bonding/src/net/transport.rs` — pacing, SO_SNDBUF, token bucket
6. `crates/strata-bonding/src/scheduler/bonding.rs` — EDPF/BLEST/IoDS scheduler
7. `crates/strata-bonding/src/config.rs` — cross-check what's centralized vs. inlined

## Pre-found leads — verify first, these are candidate real bugs, not style nits

- **`congestion.rs:845` vs `893-955` (`update_pacing_rate`)** — verified by
  direct read: `evaluate_state_transition()` (called at `on_radio_metrics_update`
  line 831) does `self.pacing_rate *= 0.7` on the Normal→Cautious edge, then
  the very next line (835) unconditionally calls `update_pacing_rate()`,
  which recomputes `rate` from scratch off `btl_bw`/phase and **overwrites**
  `self.pacing_rate = rate.max(10_000.0)` at line 955 — clobbering the line-845
  multiply in the common case (any tick where `btl_bw > 0`). The one place
  line 845 survives is the pre-calibration edge case (`BbrPhase::SlowStart`
  with `btl_bw <= 0`), where `update_pacing_rate` returns early (line 921)
  without touching `self.pacing_rate`. So line 845 is either (a) dead code
  that should be deleted because `update_pacing_rate`'s own `Cautious => rate
  *= 0.7` (line 940) already does the job, or (b) deliberate insurance for
  that one edge case and deserves a comment explaining it — right now it
  reads like an accidental double-application, which is *not quite* what it
  is. Confirm which, and fix accordingly (comment or delete).
- **`adaptation.rs:524-529`** — doc comment says "fast down (α=0.7)... slow up
  (α=0.3)" but the actual constants are `CAP_EWMA_ALPHA_UP = 0.3` /
  `CAP_EWMA_ALPHA_DOWN = 0.5` (verified by direct read). The comment's "0.7"
  matches neither constant. At minimum the comment is stale; check which
  value (0.5 or 0.7) the intent actually was and fix the drifted one.
- **`adaptation.rs:834` and `:920`** — `ewma_loss_fec > 0.08` gates
  `jitter_loss_context`, which feeds `delay_pressure`/`late_pressure`. This is
  a smaller sibling of the just-removed `ewma_loss_fec > 0.15` bug (the
  post-FEC residual was supposed to stop driving encoder cuts directly — see
  [Adaptation-Encoder-Cut-Signals](wiki/Adaptation-Encoder-Cut-Signals.md)).
  Check whether this is a legitimate, narrower use (context flag, not a
  direct cut) or the same anti-pattern re-entering through a side door.
- **`adaptation.rs:691` and `:843`** — `l.loss_rate >= 0.55 &&
  l.queue_depth.unwrap_or(0) >= 60`, identical expression duplicated verbatim
  in two functions (`update()`'s `per_link_collapse` and
  `update_with_feedback()`'s `link_collapse`). The `queue_depth >= 60` half
  uses the same raw-packet-count signal that the already-fixed `>= 90` gate
  (see the comment at `adaptation.rs:927-936`) explicitly disproved as a
  bufferbloat proxy — but this sibling wasn't revisited when that fix landed.
  Determine if `>= 60` here is doing something different enough to be safe,
  or should go the same way as the `>= 90` fix.
- **`fec.rs` — `GilbertElliott`** (`loss_threshold: 3`, `recovery_threshold:
  10`, `fec_multiplier(): 2.0`) — appears to have no call site wiring it into
  `FecEncoder` or the adapter. Confirm with a repo-wide reference search
  whether this is dead code (delete or gate behind a feature) or a half-wired
  integration (finish it or remove it) — either way it shouldn't sit
  unreferenced next to the live FEC-sizing path.
- **`oracle.rs:171`** — `self.lower_bound_peak * 0.4` floor. Search the repo
  for prior comments/wiki notes referencing a "40%-of-peak floor" as a
  contributor to a phantom-capacity/failover field bug — if that reference
  exists, this constant was implicated in a real incident and only the
  downstream symptom was patched, not this floor itself. Confirm and decide
  if it still needs a fix.
- **`transport.rs:559-560`** — a comment describes an "allow negative token
  balance up to 1 MTU" debt allowance; the actual guard (`tokens >= 0.0`)
  doesn't implement that allowance. Stale comment or missing code — check
  which, and fix the mismatch.
- **`oracle.rs:295` vs `config.rs`'s `failover_rtt_spike_factor` (default
  3.0)** — oracle.rs hardcodes its own independent `rtt_ms >
  self.baseline_rtt_ms * 3.0` rather than reading the config field. Confirm
  whether changing `failover_rtt_spike_factor` in a deployed config is
  silently a no-op for this check.

## 1. Magic-number inventory — the rest

For every remaining inline numeric literal used as a threshold, multiplier,
EWMA alpha, or timeout in the files in scope, one of three things should be
true; flag any that satisfy none:

- Named `const` with a doc comment explaining *why that value* — e.g.
  `net/transport.rs`'s `STARVED_CAPACITY_FLOOR_BPS`, `ORACLE_SANE_BTLBW_MULT`
  (which even has a paired `const _: () = assert!(...)` sanity check) is the
  bar to hold the rest of the codebase to.
- Threaded through `SchedulerConfig`/`AdaptationConfig` with a default and a
  doc comment.
- Bare literal, no name, no comment — list it: file:line, value, what it
  gates.

Additional confirmed instances to extend from (non-exhaustive):

- At least **four distinct EWMA rise/fall pairs** doing conceptually the same
  "noisy signal, asymmetric trust" job with no shared constant or documented
  rationale for why they differ: `adaptation.rs` capacity EWMA (0.3 up / 0.5
  down), `adaptation.rs` loss EWMA (~0.3/0.7), `oracle.rs` lower-bound EWMA
  (0.3 rise / 0.05 fall), `oracle.rs` baseline-RTT EWMA (0.95/0.05). Decide if
  these should converge to one or two named, shared constants.
- Values defined in more than one place that could silently drift apart: FEC
  `High=0.50`/`Low=0.10` (both `fec.rs`'s enum method and `FecConfig::default()`
  — `fec.rs:42-44` vs `:69-70`); the "3× RTT spike" factor (`config.rs`
  `failover_rtt_spike_factor` vs. `oracle.rs:295` inline); `oracle.rs:145` and
  `:239` both inline the same `100_000.0` bps cutover independently.
- Two or three independent scaling/clamping layers stacked on one underlying
  quantity: FEC overhead is the product of `adaptation.rs::recommended_fec_overhead()`
  *and* `fec.rs:132`'s separate `* 2.5` High:Low ratio; pacing_rate is
  independently floored/capped by `congestion.rs`'s `drain_factor`,
  `transport.rs:511-516`'s `peak_cap_bytes * 0.2` floor, and the SINR
  ceiling; capacity is clamped independently by oracle bounds,
  `transport.rs:1259`'s `.clamp(100_000.0, 50_000_000.0)`, and
  `capacity_floor_bps`. For each stack, check whether the layers were tuned
  together or independently, and whether they can fight.
- Several thresholds in `congestion.rs` apply state changes off a **single
  instantaneous sample** with no multi-tick sustain, inconsistent with the
  file's own `queue_building()` (which requires ≥4 consecutive samples):
  `on_modem_flow_control`'s unconditional `drain_factor *= 0.9` (~line 393),
  the RSRP-slope PreHandover trigger off as few as 2 buffered samples
  (~line 853), and the gradient-severity decay recomputed every 100 ms call
  with no confirmation window (~line 363-365). Each is a candidate sibling of
  the instantaneous-signal anti-pattern already fixed twice on this branch.

## 2. Control-loop interaction review

Strata runs at least four independent control loops that can all react to
the same underlying event (a link degrading): the encoder bitrate adapter
(`adaptation.rs`), the BBR-based per-link pacer (`congestion.rs`), the FEC
overhead sizer (`fec.rs`, fed by `max_link_loss`), and the capacity oracle
(`oracle.rs`). Assess:

- Can two of these loops independently "double-count" the same degradation
  and overcorrect? `adaptation.rs`'s `ramp_down_factor` defaults to 0.7 and
  `congestion.rs`'s Cautious-state dampening is also 0.7 — coincidence, or
  values that were tuned assuming only one of them would apply per event?
- `adaptation.rs::update()` (capacity path) and `::update_with_feedback()`
  (receiver-feedback path) can each cut the bitrate in the same tick; the
  arbitration bookkeeping (`capacity_already_cut`, `allow_feedback_cut`,
  `increased_this_tick`) has grown by one guard with each of the three
  recent fixes rather than being consolidated. Is it still legible, or is it
  time to unify the pressure signals into one ranked/prioritized decision
  instead of a pile of booleans?
- Ramp-up/ramp-down asymmetry: `ramp_up_kbps_per_step` is additive per tick,
  `ramp_down_factor` is multiplicative. Additive-up + multiplicative-down
  means recovery time scales linearly with the gap while collapse is
  exponential — confirm this asymmetry is a deliberate "recover cautiously,
  cut decisively" design choice (and say so in a comment) rather than an
  accident of two features written at different times.
- Compare the independent cooldown/grace timescales across loops
  (`adaptation.rs`'s 5 s feedback grace, 10 s burst cooldown, config's 1.5 s
  `congestion_sustain`; `net/transport.rs`'s `PROBE_FEEDBACK_COOLDOWN`) — do
  the relative durations make sense (outer loop slower than the inner one it
  depends on), or could a faster loop's transient still be "seen" by a slower
  one as if it were sustained?

## 3. Config-centralization consistency

`config.rs`'s `SchedulerConfig`/`AdaptationConfig` expose ~25 named,
documented, runtime-tunable knobs. For every constant found in step 1 that
functions like a tunable (threshold, ratio, decay factor) rather than a true
invariant (e.g. `fec.rs`'s `SYMBOL_SIZE: u16 = 1400`, a wire-format constant),
note whether it should be promoted to config or is fine as a local constant —
and flag the cases (found above) where a config field and a hardcoded
sibling already coexist, since those are actively misleading (changing the
config does nothing). Don't perform the promotion yourself — flag candidates
for the maintainer to decide; some constants are deliberately pinned to keep
the config surface bounded.

## Deliverable

A single markdown report (`review_findings.md` at repo root) listing each
finding as: `file:line — literal — what it gates — concern (1-2 sentences)
— suggested severity (nit / worth-a-comment / worth-a-fix)`. Put the
"Pre-found leads" verifications at the top, with a clear verdict for each
(confirmed bug / confirmed fine / needs a comment). Do not submit code
changes in this pass — this is an audit, not a refactor.
