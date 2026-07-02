# Log

Append-only record of decisions, ingests, and meaningful changes. Newest at
the top. One dated entry per day — enough to reconstruct *why* later.

Format: `## YYYY-MM-DD` heading per day, bullet per entry.

## 2026-07-02

- Started implementing [review_findings.md](review_findings.md) +
  [PLATFORM_REVIEW.md](PLATFORM_REVIEW.md) in full, per plan
  `rosy-squishing-treasure`. Dispatched 9 parallel worktree-isolated agents
  for the batch 1-3 items; 6 of 9 produced real work before an account-level
  usage limit cut them off mid-task (3 — adaptation.rs consolidation,
  dashboard WS auth, platform E9 hygiene — made zero progress and need
  redoing). Reviewed, fixed, tested, and merged the 6 landed batches to
  `main` directly:
  - **L5**: deleted the entire dead `scheduler/fec.rs` module (RaptorQ/UEP,
    ~726 lines) — zero external references confirmed.
  - **L1/N1/N2/N6** (`congestion.rs`): deleted the dead Cautious pacing
    dampening line; fixed the real RSRQ/RSRP bug (N1 — the PreHandover
    guard compared RSRP against an RSRQ threshold, always true) with a new
    `rsrq_history` field and a >=3-sample floor; documented the radio
    feed-forward's live-caller gap (no modem currently produces real
    RadioMetrics); fixed two stale drain_factor docs. Caught and fixed a
    real bug in the agent's own regression tests (letting CQI go flat let
    the "CQI stable" edge revert Cautious->Normal before the RSRQ guard
    ever ran).
  - **L6/L8/§2.4.1** (`oracle.rs`+`bonding.rs`): gave `lower_bound_peak`
    the same 1%/s decay `peak_estimate` already has (fixes the
    never-decaying 40%-of-peak floor behind a real "phantom capacity"
    field incident); documented that the failover RTT-spike detector and
    the oracle's downshift detector are intentionally independent despite
    sharing the number 3; required 2 consecutive RTT-spike ticks (not 1)
    before the failover broadcast fires — finished wiring this myself
    after the agent left the constant unused.
  - **N9/L7** (`net/transport.rs`): replaced the far-future-`Instant`
    probe-feedback sentinel with an explicit `ProbeFeedbackBlock` enum;
    reworded the token-bucket comment; named the remaining `§1a` magic
    numbers.
  - **E5/E7** (`streams.rs`): fixed the concurrent-stream-guard SQL
    bind-count bug (one placeholder, two binds — likely 500'd every
    platform stream-start); wired `receiver.stream.stop` on stop (fixes
    the receiver-pipeline-orphan and `active_streams` drift together);
    removed the hardcoded `bonding_config` override that force-enabled
    `redundancy_enabled`/`critical_broadcast` and pinned the floor to
    5 Mbps against `SchedulerConfig::default()`'s field-tuned values;
    added a regression test. (Recovered this branch's work after an
    accidental `git checkout --` briefly wiped it — re-applied from the
    reviewed diff.)
  - **E10**: retired `strata-portal` per explicit user decision (invest vs.
    retire vs. protocol-only migrate vs. skip — user chose retire).
    Removed the crate, workspace member, `portal-dev` compose service, CI
    step, and doc references. Flagged a real follow-up gap: `strata-sender`'s
    local onboarding HTTP server (`portal.rs`, `:3001`) served this crate's
    build output and now has nothing to serve.
  - New wiki page [Adaptation-EWMA-Conventions](wiki/Adaptation-EWMA-Conventions.md)
    (§1b): states the rise-fast/fall-slow-for-capacities vs
    rise-slow/fall-fast-for-floors polarity rule implicit across the
    dozen-plus EWMAs in the bonding/transport stack.
  - Environment note: this sandbox's `RLIMIT_MEMLOCK` (hard-capped 8 MB)
    intermittently (now persistently) fails 8 `strata-bonding` monoio/
    io_uring tests with `OS OutOfMemory` — confirmed identical failures on
    unmodified `main`, unrelated to any of the above. User approved
    `--no-verify` commits (with the reason noted in each message) rather
    than blocking on a full-workspace pre-commit hook that can't pass in
    this sandbox regardless of code correctness.
  - **Remaining** (not yet done): adaptation.rs §2.2 ranked-decision
    consolidation + its smaller fixes (L2/L3/L4/N4/N5/N7), dashboard WS
    auth/scoping (E3), platform E9 hygiene pass, then the larger
    executive items E1 (protocol crate), E2 (state machine +
    reconciliation), E4 (device identity), E6 (port allocation), E8
    (receiver telemetry).

## 2026-07-01

- Executed [review_plan.md](review_plan.md) in full → [review_findings.md](review_findings.md)
  (Fable 5, audit only, no code changes). All 8 pre-found leads verified with
  verdicts; headline new finds: `congestion.rs:853` compares RSRP where the
  guard means RSRQ (always-true condition), the entire Biscay radio
  feed-forward has **no live caller** (`notify_rf_metrics` uncalled — the
  state machine in congestion.rs is dead in production), ALL of
  `scheduler/fec.rs` (RaptorQ/UEP/GilbertElliott) is dead code (live FEC is
  RLNC via `set_fec_rate(32, R)`), `congestion_headroom_ratio`/
  `congestion_trigger_ratio` are dead config knobs, adapter tick-count
  sustains silently rescale with `stats_interval_ms`, and the failover
  broadcast triggers off a single RTT sample while the encoder cut needs
  1.5 s sustain. Suggested landing order at the end of the report.
- Wrote [PLATFORM_REVIEW.md](PLATFORM_REVIEW.md): top-down architecture
  review of the management plane + web (control/dashboard/portal/sender/
  receiver daemons). Ten ranked executive changes (E1–E10). Headliners:
  dashboard `/ws` is unauthenticated and unscoped (any client gets full
  fleet telemetry; violates the stated per-owner security model); protocol
  exists in 3 divergent copies (partial enums, stringly dispatch, 41
  hand-copied dashboard types); WS-drop is conflated with stream death and
  there is no state reconciliation; control plane hardcodes a bonding_config
  that force-enables `redundancy_enabled`+`critical_broadcast` (both
  default-OFF after field incidents) and pins capacity_floor to 5 Mbps;
  `receiver.stream.stop` is never sent (stop path orphans receiver
  pipelines); likely-fatal extra-bind SQL bug in the stream-start
  concurrency guard; device-key auth is TODO with O(n·argon2) connect scans.
- Merged `fix/adapt-goodput-not-residual` to `main` (4 commits: FEC sizing on
  channel loss not residual, burst reflex gated on real goodput collapse,
  PAT/PMT mux-bloat fix, HLS egress/discontinuity hardening). All 415 tests
  pass (368 strata-bonding + 47 strata-gst), clippy clean workspace-wide.
- Wrote [review_plan.md](review_plan.md): a targeted audit brief for the
  incoming Fable 5 model to review magic numbers and control-loop structure
  in `adaptation.rs`/`congestion.rs`/`oracle.rs`/`fec.rs`/`transport.rs`. A
  recon pass (direct read + subagent) already turned up concrete leads worth
  flagging up front: `congestion.rs:845`'s Cautious-transition `pacing_rate
  *= 0.7` looks dead in the common case (immediately overwritten by
  `update_pacing_rate()`'s own dampening moments later, confirmed by direct
  read — only survives in the pre-calibration SlowStart edge case);
  `adaptation.rs:524-529`'s doc comment ("α=0.7 down") doesn't match its own
  constants (`CAP_EWMA_ALPHA_DOWN = 0.5`); `adaptation.rs:691`/`:843` still
  duplicate a `queue_depth >= 60` collapse gate using the same raw-packet-
  count signal the `>= 90` gate was already disproven and removed for; and
  `fec.rs`'s `GilbertElliott` model looks unwired/dead. None of these fixed
  yet — that's the review task.
- gst(receiver): field-validated yesterday's HLS discontinuity-tagging work
  via `orangepi_ethernet_field_test.sh` (run orangepi-72665, real Band 8,
  2 modem links, 120 s live YouTube stream). Cross-aarch64 build picked up
  the new `gst-plugin-hlssink3` dependency cleanly. Result: 6 total
  `DeliveredStream` gate resumes (1 harmless startup resume, correctly left
  untagged since `GateState.started` was false for it, + 5 real mid-stream
  splices triggered by 655 lost packets at the reassembly layer). All 5 real
  splices were correctly attributed to their segment and marked
  discontinuous, a clean 1:1 match — the gate → `hls-segment-added`
  correlation → playlist rewrite chain works under real cellular loss, not
  just synthetic testing. 32 segments uploaded, playlist produced
  throughout, `damaged=0` at the app layer for the whole run (0
  `fec_corrupt_dropped` on both links). Updated
  [HLS-Egress-Discontinuity-Tagging](wiki/HLS-Egress-Discontinuity-Tagging.md)
  and closed the open question in `hot.md`.

## 2026-06-30

- gst(receiver): HLS egress hardening for YouTube stutter during the fade
  window (forwarding mechanics were healthy — the stutter was unmarked
  timeline jumps at gate resumes). Latency cut (target-duration 2→1s, upload
  poll 500→250ms) was uncontested. The other two proposed fixes had false
  premises, caught by reading source before implementing: hlssink3 does NOT
  auto-tag DISCONT as `#EXT-X-DISCONTINUITY` (checked gst-plugin-hlssink3
  0.15.3 source directly), and nothing in our pipeline ever sets
  `BufferFlags::CORRUPTED` (every loss already reaches the gate via
  `pending_discont`, so that fix had no real signal to hook into — dropped).
  Implemented the real version: hlssink3 migration (registered statically,
  `gsthlssink3::plugin_desc::plugin_register_static()`), gate re-stamps
  DISCONT + queues resume running-times, a bus watch correlates
  `hls-segment-added` messages to those resumes, and `hls_upload.rs`
  reconstructs `#EXT-X-DISCONTINUITY` + `#EXT-X-DISCONTINUITY-SEQUENCE` in the
  uploaded playlist text (hlssink3 owns and rewrites the on-disk file itself).
  Hit a second false premise mid-implementation: hlssink3 isn't a drop-in for
  hlssink — it has no muxed `sink` pad, only `video`/`audio` request pads it
  muxes internally, so the pipeline topology had to drop our own `mpegtsmux`
  entirely. Also found hlssink3 needs both pads fed audio+video or it never
  closes a single segment (confirmed by a 15s-stalled 0-byte segment file in
  testing) — not a new requirement in practice since field-test scripts
  already pass `--audio`, but now a hard one. End-to-end smoke-tested locally
  (real sender→receiver, segments cut every ~1s, hls-segment-added fires with
  correct running times, no crashes/critical warnings); discontinuity tagging
  itself validated via unit tests on the rewrite function, not yet against a
  real mid-stream loss event. 47 strata-gst + 368 strata-bonding tests pass,
  clippy clean. New wiki note
  [HLS-Egress-Discontinuity-Tagging](wiki/HLS-Egress-Discontinuity-Tagging.md).

## 2026-06-29

- gst(mux): **the whole encoder-cut / FEC / playout saga was one wrong mux
  constant.** `mpegtsmux pat-interval=1 pmt-interval=1` — those properties are
  **90 kHz ticks (default 9000 = 100 ms), not a packet count** — so `=1` emitted
  PAT+PMT (376 B) before nearly every video packet, **tripling wire bandwidth**.
  Found via a step-by-step field diagnosis: instrumented the playout-window term
  breakdown (showed it sized entirely by delay-spread, a symptom), then traced a
  ~243k-packet/run paced-queue **AQM self-inflicted-loss** flood. Walked back two
  wrong hypotheses (broadcast/redundancy duplication — redundancy was OFF; then
  encoder overshoot) by checking config and adding a **pre-mux encoder-output
  probe**: encoder emitted **2.26 Mbps** (tracking target) but the **post-mux
  sink saw 7.0 Mbps (3.1×)** — proving the muxer, not the encoder, was the flood.
  Fix: `pat-interval=9000 pmt-interval=9000` at all 3 sender sites
  (`strata_pipeline.rs`). Field-validated (orangepi-57909): post-mux egress
  7.0→2.55 Mbps, AQM drops 243k→330, receiver lost ~85k→799, discontinuities
  ~1200→121, playout window unpinned from the 3 s cap to ~1.8 s. PAT/PMT keep the
  HEADER→Critical→FEC-protected path, so 100 ms is ample resilience. New note
  [MPEG-TS-Mux-Overhead](wiki/MPEG-TS-Mux-Overhead.md); AGENTS.md pattern
  corrected. The encoder, adapter, FEC sizing and playout logic were all correct.
- gst(encoder): **companion fix — `rc-mode=cbr` on the Rockchip MPP encoder**
  (`codec.rs::configure_static_props`, find_property-guarded like header-mode).
  The BSP default rc-mode let the encoder burst past the 0.5 s paced-queue budget
  even at the right average rate; CBR smooths it. Field-measured AQM bursts −98%.
  Kept independent of the mux fix. Diagnostic instrumentation kept: sink
  egress-rate log + transport snapshot `retransmissions`/`fec_repairs_sent`
  (useful telemetry); the pre-mux probe + window-term breakdown were removed as
  one-shot debug.

## 2026-06-28

- adapt(encoder): **gate the burst reflex on a real goodput collapse** (field
  follow-up to the residual-override removal). Field run orangepi-10360 showed
  the encoder still slammed to the 500 floor 34% of ticks with ~5.3 Mbps spare —
  not via the removed EWMA gates, but via `burst_loss`/`severe_burst`, which
  keyed on the *instantaneous* `loss_after_fec`. That residual is the same
  reorder/late-contaminated signal: 72 burst windows averaged 5.3 Mbps delivered
  goodput (100% >= 2 Mbps) while reporting 0.65 mean loss-after-FEC; damaged=0
  all run. Fix: `burst_loss` now also requires `goodput_bps > 0 && goodput <
  0.7x offered` — a reorder spike with healthy goodput no longer cuts; a real
  burst (goodput collapses too) still cuts same-window. `severe_burst` inherits
  it. New regression test `burst_loss_does_not_cut_when_goodput_is_healthy`
  (trips severe_burst on old code). 368 tests pass, clippy clean. Wiki note +
  orangepi-10360 evidence updated. Same run confirmed FEC overhead 16.8% mean
  (was 41.6% pinned — death spiral dead) and ramp-up recovery to 2.5 Mbps. Field
  re-test pending.
- adapt(encoder): **remove the post-FEC residual override on the encoder
  bitrate.** Sibling of the FEC-sizing fix: the residual (`ewma_loss_fec`) folds
  in reorder/late loss the encoder can't fix, so it must not move the bitrate.
  Deleted the two `ewma_loss_fec > 0.15` gates — `loss_pressure` (forced an
  encoder cut) and `loss_suppressed` (blocked ramp-up); both were binary,
  headroom-blind, and pinned/cut the encoder while spare bandwidth existed. The
  encoder now follows the continuous capacity path (per-link channel-loss
  discount → pressure, already correct), goodput shortfall (delivered < 0.7×
  offered — headroom-aware, reorder-immune), AQM self-congestion, and genuine
  per-link melt (`link_collapse`, the half `loss_pressure` legitimately
  bundled). Hardened goodput shortfall with a severe tier (< 0.5× pre-update
  target) that bypasses the post-increase grace — replacing the residual's grace
  pass-through with a trustworthy, staleness-robust signal. Residual is kept only
  for `jitter_loss_context` and the FEC burst-lift. New regression test
  `high_residual_loss_with_headroom_does_not_cut_encoder` (fails on old code);
  renamed `loss_pressure_gated_on_goodput` →
  `mild_residual_loss_with_healthy_goodput_does_not_cut`. 367 unit + all
  integration tests pass, clippy clean. New note Adaptation-Encoder-Cut-Signals
  + index row. Branch `fix/adapt-goodput-not-residual`.
- adapt(fec): **stop the FEC death spiral.** Traced why the post-fix run
  (orangepi-11528) stayed loss-bound. Root cause was NOT "lever 2" (per-link
  loss → EDPF, route around a bad link) — both links were clean (~2% wire
  loss; EDPF already de-rates by loss/delay/jitter). It was
  `recommended_fec_overhead` sizing parity from `ewma_loss_fec` (the *post-FEC
  residual*), which folds in cross-link reorder + late-arrival loss that parity
  cannot repair. Feedback loop: reorder loss → more parity → repair microbursts
  at generation boundaries overflow marginal-link buffers → more late/reorder
  loss → still more parity. Field evidence (run 2026-06-27, receiver
  65.109.5.169): per-link wire loss ~2% but receiver post-FEC residual ~17%
  cumulative (spiking ~36%), FEC overhead pinned at **41.6%** while the encoder
  sat at the **500 floor with 3.7 Mbps spare** and both links idle; 2.66× wire
  redundancy, playout buffer pinned at the 3 s cap 75% of ticks, 925
  discontinuities, 0 resync churn. The `self_congested` guard that pins FEC to
  baseline can't fire at the floor (pressure ≈ 0.085 ≪ 0.7; lowering it
  reintroduces the 2026-06-15 bursty-modem bug).
- adapt(fec): **fix** — size FEC parity to per-link CHANNEL loss
  (`max_link_loss`), not the post-FEC residual: strictly more correct (more
  protection for real channel loss, none for reorder/late). New field
  `max_link_loss` set in `update()`; driver swap in `recommended_fec_overhead`.
  New regression test `fec_overhead_not_inflated_by_reorder_residual`; existing
  `fec_overhead_pinned_under_self_congestion` updated to the channel-loss
  driver. 366 lib + integration tests pass; fmt clean. See
  [Adaptation-FEC-Sizing](wiki/Adaptation-FEC-Sizing.md), related
  [Adaptation-Delay-Pressure](wiki/Adaptation-Delay-Pressure.md).
- adapt(fec): **field-validated** (run orangepi-3870, 120 s, clean EOS,
  10 HLS segments, damaged=0). Hit *worse* radio than the baseline (link 0
  ~8% mean channel loss vs ~2%), yet receiver-side quality improved sharply:
  post-FEC residual mean **7%→1.5%** (90% of ticks <5% vs 63%), discontinuities
  **925→285**, egress-gate drops **1772→348**, playout pinned at the 3 s cap
  **75%→61%**. Death-spiral signature gone: **0** ticks of high-FEC (>25%) with
  low channel loss (<5%); all 21 high-FEC ticks co-occurred with genuine high
  channel loss. Encoder floor time now tracks real loss (mean channel loss
  12.5% at floor vs 8.3% above) — correct adaptation, not a phantom collapse.
  Note redundancy_enabled=false, so the wire overhead was FEC+retransmits only.
- **Open:** secondary contributor to the 2.66× — adaptive-redundancy
  duplication also floods when spare is huge. Watch on field validation.

## 2026-06-27

- Initialized AI workspace (LLM wiki pattern) from ai-workspace-template: added
  CLAUDE.md, hot.md, index.md, log.md, raw/, .claude/settings.json,
  .claude/commands/ (ingest, wiki-new, wiki-lint). AGENTS.md and GEMINI.md
  converted to symlinks → CLAUDE.md. Existing wiki/ pages registered in index.
- adapt: removed the `max_queue_depth >= 90/60` packet-count gates from
  `delay_pressure`/`late_pressure` in adaptation.rs. A deep paced queue is the
  *intended* state during keyframe bursts (queue is byte/drain-time bounded at
  0.5 s), so the gate misread benign bursts as bufferbloat and pinned the
  encoder to the 500 floor (~65 % of ticks, field run orangepi-3924: usable
  ~4.7 Mbps, pressure ~0.1, loss ≈ 0). Bufferbloat is now AQM-drop +
  receiver-delay based; genuine standing-queue congestion still caught via the
  pressure-gated self-congestion path. New regression test
  `deep_paced_queue_without_loss_does_not_cut`; 365 bonding tests pass. New
  wiki note [Adaptation-Delay-Pressure](wiki/Adaptation-Delay-Pressure.md).
  Post-fix field run orangepi-11528 confirmed pure-bufferbloat reduces 57→0;
  that run was loss-bound (~40 % post-FEC) so the avg-bitrate win is not yet
  demonstrated — real loss-driven collapse remains (lever 2 territory).
- meta: inverted doc symlinks — AGENTS.md is now the canonical file;
  CLAUDE.md and GEMINI.md are symlinks → AGENTS.md.
