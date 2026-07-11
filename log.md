# Log

Append-only record of decisions, ingests, and meaningful changes. Newest at
the top. One dated entry per day — enough to reconstruct *why* later.

Format: `## YYYY-MM-DD` heading per day, bullet per entry.

## 2026-07-02 (cont'd — N3 landed: control-loop audit now fully done)

- Deleted `SchedulerConfig::congestion_headroom_ratio`/
  `congestion_trigger_ratio` (review_findings.md N3) — confirmed zero
  reads outside `config.rs` itself via grep and
  `mcp__gitnexus__impact` (0 upstream impact, LOW risk both candidates).
  Removed the `SchedulerConfigInput` fields, `SchedulerConfig` fields,
  defaults, resolve-mapping, and the two TOML-parsing test assertions.
  No `deny_unknown_fields` on the input struct, so old TOML configs that
  still set these keys keep parsing (the keys were already effectively
  ignored). 24/24 config tests + full `strata-bonding` suite (359 lib +
  8 integration) pass, clippy clean. Commit `ab58233`.
  - This was the last open item in `review_findings.md`'s Part 0-2 —
    the control-loop audit is now **fully implemented**, aside from
    §2.2's deliberately-deferred full redesign (bookkeeping-only version
    landed instead) and the §1b EWMA-naming pass (docs only, by design).
  - Checked in with the user after Batch 1-3 completed in full; they
    chose to finish this last small item (N3) and the CorsLayer/
    `/metrics` posture flag, then stop here — Batches 4-6 (E1 protocol
    crate, E2 state machine, E4 device identity, E6 port allocation, E8
    telemetry) are left for a future session with fresh context.
  - Updated review_findings.md's status banner, N3 entry, Part 3, and
    the suggested-landing-order section; `hot.md` refreshed to reflect
    the control-loop audit being fully done.

## 2026-07-02 (cont'd — Batch 3.4 landed: platform timing hygiene, E9)

- Implemented [PLATFORM_REVIEW.md](PLATFORM_REVIEW.md) E9: named the
  platform's own magic-number sprawl (JWT expiry, reconnect backoff,
  channel capacities, timeouts, fallback ports) and added the one real
  behavior change the finding calls for — jitter on every reconnect loop,
  since a control-plane restart currently makes every agent, receiver, and
  open dashboard tab reconnect in lockstep (a thundering herd against E4's
  still-open O(n·argon2) enrollment scan).
  - JWT expiry: new `strata_common::auth::SESSION_TOKEN_TTL_SECS`, used at
    all 3 production call sites that each hardcoded `now + 3600`
    (`api/auth.rs`, `ws_agent.rs`, `ws_receiver.rs`); left the 3 test-only
    literals in `strata-common/src/auth.rs`'s test module alone.
  - Reconnect backoff (byte-for-byte duplicated in `strata-sender`/
    `strata-receiver`'s `control.rs`): named `INITIAL_BACKOFF`/
    `MAX_BACKOFF`, added `BACKOFF_JITTER_FRACTION` (±20%, via a new `rand`
    dependency already vendored workspace-wide). Left the cross-crate
    duplication itself alone — the finding asks for "one documented config
    module *per crate*", not a shared dedup, and a cross-crate refactor is
    a bigger, separate call.
  - Dashboard reconnect (`strata-dashboard/src/ws.rs`): fixed 3s → 3-4s
    jittered, via `js_sys::Math::random()` rather than pulling in `rand`
    (which needs extra getrandom/wasm32 backend wiring on this target).
  - Channel capacities named across `state.rs`/`ws_agent.rs`/
    `ws_receiver.rs`/both `main.rs`: found and flagged (not silently
    unified) a real discrepancy — the control plane's per-device command
    channels are 64, but the agent/receiver's own outbound channel to the
    same link is 128.
  - Named `STOP_FORCE_END_TIMEOUT` (15s, `api/streams.rs`),
    `MONITOR_POLL_INTERVAL` (500ms, both `pipeline_monitor.rs`),
    `FALLBACK_RECEIVER_PORTS` (with a comment that it must track
    `strata-receiver`'s own CLI default, not a live discovery).
  - Converted the 3 silently-dropped command sends the finding actually
    names (`receiver.stream.start`/`stream.stop`/`receiver.stream.stop` in
    `api/streams.rs`) to log a warning on failure. Left every other
    `let _ = ...send(...)` alone (best-effort dashboard broadcasts,
    auth-error responses to sockets about to close, watch-channel
    reconnect signals) — not the "command drops" this finding names, and
    logging them would just add noise for expected failure paths.
  - Full-workspace build + clippy clean; 18/18 `strata-control` tests,
    `strata-sender`/`strata-receiver` unit tests, and `strata-dashboard`
    against `wasm32-unknown-unknown` all pass.
  - `mcp__gitnexus__detect_changes`: "medium" risk (touches multiple
    crates' entry points), scoped to exactly the files this touched.
  - Updated PLATFORM_REVIEW.md's E9 status, top banner, and sequencing
    table; `hot.md` refreshed.
  - **Remaining**: N3, §2.2's full redesign (deliberately deferred), the
    CorsLayer/`/metrics` posture decision, then E1 (protocol crate), E2
    (state machine + reconciliation), E4 (device identity), E6 (port
    allocation), E8 (receiver telemetry).

## 2026-07-02 (cont'd — Batch 3.2 landed: dashboard WS auth + scoping, E3)

- Implemented [PLATFORM_REVIEW.md](PLATFORM_REVIEW.md) E3: `GET /ws`
  (`ws_dashboard.rs`) had no authentication at all and the broadcast channel
  was global (every operator saw every other operator's fleet). Fixed both:
  - `ws_dashboard.rs` now requires an `auth.login` first message carrying a
    JWT (mirrors `ws_agent.rs`/`ws_receiver.rs`'s handshake exactly, per the
    finding's own "like the agent WS" instruction — not the previous unread
    `?token=` query param, since tokens in URLs end up in proxy logs).
    Device-role tokens (`claims.owner.is_some()`) are rejected — only real
    user sessions may open the dashboard feed.
  - `AppState::dashboard_tx` now carries `(owner_id, DashboardEvent)`
    instead of a bare `DashboardEvent`; every `broadcast_dashboard` call
    site (`ws_agent.rs` ×6, `ws_receiver.rs` ×1, `api/streams.rs` ×3) now
    supplies the owning user's ID (threaded from auth for the two WS hubs,
    from `AuthUser` for the REST handlers). `ws_dashboard.rs` filters its
    subscription — and the initial snapshot queries — to the connected
    user's own `owner_id`.
  - Updated the dashboard client (`strata-dashboard/src/ws.rs`) to send the
    token as the first WS message instead of the URL query param, and to
    tell the Envelope-wrapped auth response apart from the (still
    un-enveloped) live event stream.
  - Two new integration tests in `crates/strata-control/tests/
    api_integration.rs` (`dashboard_ws_scopes_events_to_owner`,
    `dashboard_ws_rejects_invalid_token`), using a real `tokio_tungstenite`
    client against a real `TcpListener` — WS upgrades can't be exercised
    through axum's oneshot tower-service testing that the rest of this file
    uses. New dev-dependency: `tokio-tungstenite` (already vendored in the
    workspace via `strata-sender`/`strata-receiver`).
  - Deliberately NOT touched: `CorsLayer::permissive()` and the
    unauthenticated `/metrics` endpoint — the finding explicitly asks to
    flag these for a deliberate deployment-posture decision, not silently
    change them.
  - 18/18 `strata-control` integration tests pass (Postgres via `docker
    compose up -d postgres`), full-workspace `cargo build`/clippy clean,
    `strata-dashboard` checked against the real `wasm32-unknown-unknown`
    target (added via `rustup target add`).
  - `mcp__gitnexus__detect_changes` confirmed "low" risk, scoped to exactly
    the 10 touched files.
  - Updated PLATFORM_REVIEW.md's E3 status, the "Security is declared, not
    enforced" verdict bullet (now partially addressed), and the sequencing
    table; `hot.md` refreshed.
  - **Remaining**: N3, §2.2's full redesign (deliberately deferred), the
    CorsLayer/`/metrics` posture decision, platform E9 hygiene, then E1
    (protocol crate), E2 (state machine + reconciliation), E4 (device
    identity), E6 (port allocation), E8 (receiver telemetry).

## 2026-07-02 (cont'd — Batch 2 landed)

- Continued [review_findings.md](review_findings.md) implementation:
  Batch 2 (`adaptation.rs`), done solo rather than via background agent per
  the plan's own caution (Batch 1.2/1.3 both needed hand-fixing after agent
  handoff). Two commits, both on `main` directly:
  - **Phase A** (`b28d983`/`f7ee15c`): L2 (stale α comment), L3 (the
    `jitter_loss_context` gate was self-confirming — gated on the post-FEC
    residual, which a pure reorder/late burst inflates in the same window
    as the signals it's meant to corroborate; now gated on channel-side
    `max_link_loss` instead, computed once and reused, fixing the
    pre/post-update duplication too — two new regression tests), L4
    (extracted `fn link_melting()` for a duplicated loss/queue-depth
    check), N4 (the `jitter_buffer_ms > 3000` hardcode is now
    `AdaptationConfig::jitter_buffer_ceiling_ms`, wired from the receiver's
    real `max_latency` via `sink.rs::apply_config`, which had been
    silently discarding the parsed `receiver` config section entirely), N5
    (documented, not converted — the tick-count sustain gates'
    `stats_interval_ms` coupling; full wall-clock conversion would force
    a dozen+ existing tests to either sleep for real seconds or special-
    case a zero-duration override that defeats the "sustained" semantics
    under test), N7 (`consecutive_increases` doc + named trend-band
    consts), §2.3 (AIMD asymmetry documented as deliberate design intent
    with worked recovery numbers), §1c (acknowledged the `drain_factor`/
    `rtt_bufferbloat_throttle` double-count in a comment), plus a full
    named-const pass over the remaining `adaptation.rs` §1a magic numbers.
  - **Phase B** (`97707f9`): §2.2's bookkeeping-centralization half — a
    `TargetOverride` struct + `apply_target_override` that the three
    explicit `current_target_kbps`-mutation sites now share, instead of
    each hand-rolling its own subset of `last_command_time`/
    `last_increase_time`/`last_burst_time`/`consecutive_*`. Deliberately
    did NOT attempt the full "collect evidence, rank, commit once"
    redesign the finding describes — on inspection the three sites are a
    fixed-order sequential cascade of downward-only refinements, not
    actual competing alternatives, and forcing that into a strict ranked
    model risked changing real arbitration behavior in the live encoder
    loop without field hardware to validate against. 359 `strata-bonding`
    tests pass **unchanged** through both commits — the actual proof nothing
    behavioral shifted in Phase B's mechanical extraction.
  - `mcp__gitnexus__detect_changes` run before each commit; both scoped to
    exactly the intended files/symbols, "critical" risk reflecting blast
    radius (the live encoder control loop) not unexpected fallout.
  - Only **N3** (dead `congestion_headroom_ratio`/`congestion_trigger_ratio`
    config knobs) remains unstarted in the control-loop audit.
  - Updated `review_findings.md`'s status markers/tables to match; this
    entry; `hot.md` refreshed next.
  - **Remaining**: dashboard WS auth/scoping (E3), platform E9 hygiene
    pass, then E1 (protocol crate), E2 (state machine + reconciliation),
    E4 (device identity), E6 (port allocation), E8 (receiver telemetry).

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

## 2026-07-04

- **Platform review fully implemented** (PLATFORM_REVIEW.md — all executive
  items now ✅). Four commits: `3422861` E1 strata-protocol crate (single
  wire-schema source, exhaustive enum dispatch in all four hubs/daemons,
  dashboard hand-copy deleted, `proto_version` added); `a2b2f67` E2 stream
  state machine + heartbeat reconciliation (WS drop = unobserved; sweeper
  backstop; readopt-vs-enforce split keyed on inferred-vs-confirmed ends);
  `8b6c04a` E4 device identity (one-time `<id>.<secret>` tokens, single
  argon2 verify, ed25519 challenge reconnect, persistent daemon identity,
  decorative session JWT deleted); `e8eb5a9` E6+E7+E8 (receiver-owned port
  allocation via request/ack, COUNT(*)-derived capacity, receiver-side
  stats on the dashboard). Control integration suite grew 18 → 25 tests,
  all against real WS handshakes.
- **Control-loop audit closed out** (`abee62b`): §2.4.2 FEC-sizing sustain
  (asymmetric EWMA `max_link_loss_sustained`, regression-tested), the
  skipped §1a literals (bootstrap pacing/cwnd, modem drain step, twin
  0.999 peak decays unified, OWD seed, probe min-window, 50 Mbps clamp
  copy), and §1b's EWMA-α naming pass. review_findings.md: every item ✅.
- **ARCHITECTURE_REVIEW item 9 done**: STRATA_DIAGNOSIS.md,
  findings-report.md, ARCHITECTURE_REVIEW.md archived to `raw/`; durable
  content merged into two new atomic notes, wiki/Control-Loop-Map.md and
  wiki/Observability-Semantics.md (index.md updated).
- Notable drift found & killed along the way: the dashboard's
  TransportSenderMetrics carried 8 fields no producer ever sent (NAL
  counters, fec_overhead_ratio, fec_layer); its FEC-layer/BLEST/
  fec_overhead_percent controls were placebo (config keys with no
  consumer) — all deleted rather than ported. The dashboard's `fec`
  config-update section, previously silently dropped by the control
  plane's typed parse, now round-trips and gets an honest "not supported"
  error from the agent.

## 2026-07-04 (later) — Orange Pi field test: fatal DTS-step crash found and fixed

Field run 1 (first after the review work): receiver died 21 s in with
GStreamer-fatal "Timestamping error on input streams"; the field script
watched only the sender PID, so 99 s of dead air passed as `OK`. Chain:
link 1 dead ~10 s post-admission → playout window ballooned to the
3000 ms ceiling → link recovered → window snapped down 1250 ms in ~2 s →
tsdemux's PCR skew estimator (drift-tolerant, step-intolerant) re-based
output DTS 1.256 s backwards → DeliveredStream gate resumed on an IDR
*below* its emitted-DTS watermark (resume reset `last_dts`
unconditionally) → mpegtsmux abort. Fixes (commits `8317ed7`, `38c842a`,
`0e5c5d7`, wiki `63e7451`): aggregator downward slew limit 50 ms/s
wall-clock (iat-clamped; growth stays fast), gate refuses to resume below
the watermark, script now fails on receiver death / fatal-error string,
stale `strata-portal` COPY dropped from Dockerfile.cross-aarch64 (broke
every cross build), dead `fixed_playout`/`PlayoutProfile.fixed` config
deleted (fb487f7 reverted the feature but left the lying plumbing).
Field run 2 (fixes deployed): 120 s clean — receiver alive throughout, 16
segments uploaded to YouTube, three mid-run DTS regressions each absorbed
by the gate (61 buffers dropped total, sub-second freezes), measured max
window shrink 56 ms/s, FEC steady 13–15 % under ~5 % channel loss,
encoder ~2.6 Mbps. Known cosmetic gap: script's `damaged=` readout greps
the `damaged_packets` metric deleted in fb487f7 — always 0.

## 2026-07-04 (evening) — Runs 2/3 postmortem: the demux timeline is the real defect

Run 2's "YouTube went dark at 20 s" was not YouTube: HLS egress silently
stalled at media ≈25 s (corrupt-PES splice under a loss burst latched
tsdemux +107 s on video only; mpegtsmux interleave-deadlocked; every
transport metric stayed green; EOS flushed segments stamped past the
pipeline's own age). Fixes (`04a2aa5`): `timeline_step()` gate classifier
(Regression | WildJump>10 s), audio-gate logging (was fully silent),
script egress heartbeat = cumulative 'segment added' (file count is
rotation-flat) with STALLED warning + FAILED verdict; phantom `damaged=`
readout removed; wiki Observability-Semantics row corrected. Validation
run 3 (`runs/orangepi-111043`): 117 segments/77 s (10× run 2) under ~6 %
loss, zero crashes/latches, then a THIRD timeline pathology — progressive
inflation (stamps ~60 s ahead of wall) with periodic backward corrections
— stalled egress at t≈77 s and was caught live by the new detector.
Conclusion in `runs/INVESTIGATION-2026-07-04.md`: three runs, three faces
of one defect — tsparse/tsdemux retiming is unstable over bonded-loss
input; gates contain, can't repair. Next: GST_DEBUG diagnostic run,
reconsider `tsparse set-timestamps=true`, egress-watchdog restart.

## 2026-07-04 (evening) — run 4 analyzed: silent wedge again (best run yet until it wasn't); local HLS preview added

Run 4 (`runs/orangepi-118293`, ~250 s, rough radio: playout pinned at the
3 s cap most of the run, ~5.8 % wire loss): **best live stretch to date —
128 segments over ~148 s** with the 04a2aa5 gates visibly absorbing ~14
splice storms (735 video + 256 audio buffers dropped, all logged, all
correctly re-synced, discontinuity-tagged 1:1). Then a heavy loss burst at
19:20:44–52 (post-FEC residual peaked 39 %, sender AQM self-holes on BOTH
links — third run where AQM seeds the trigger burst) → 23-discont splice
storm → video gate resynced cleanly to IDR at 160.9 s → **egress went
silent for the final ~98 s**. No gate logs, no errors, video DTS frozen at
160.94 s, while stratasrc delivered ~210 pkts/s throughout and the sender
stayed healthy to the end. At EOS the wedge flushed 3 segments (156–161 s)
— hlssink3 HAD the data but wasn't cutting. Fourth occurrence, and unlike
run 2 there is NO timestamp latch (media ≈ wall at the wedge): either
tsdemux stopped emitting a branch or hlssink3's internal muxer starved on
audio (audio-gate logs stop at 144.7 s media) and backpressure parked
everything in the leaky queues. Wedge is invisible to the gates — nothing
reaches them. Strengthens the egress-watchdog option; a GST_DEBUG run
(tsdemux + hlssink/splitmuxsink) would separate the two suspects.
Script additions: `STRATA_LOCAL_HLS_PORT` (default 8088) serves the
receiver HLS dir at http://localhost:8088/playlist.m3u8 via SSH tunnel
(127.0.0.1-bound on the receiver, nothing public) for VLC/mpv latency
checks without YouTube; verdict now persisted to `runs/<id>/verdict.txt`
(run 4's FAILED verdict existed only on the terminal and had to be
reconstructed from logs).

## 2026-07-04 (night) — egress watchdog implemented: the receiver now self-heals the silent wedge

The pending demux-timeline decision is resolved in favor of the watchdog
(run 4 made the case: ~90 s of dead air it would have recovered), with the
GST_DEBUG diagnostic folded in as trip-time forensics instead of a separate
run. `run_receiver` (strata_pipeline.rs) now runs the pipeline in
**generations**: when hlssink3 adds no segment for 15 s
(`STRATA_EGRESS_WATCHDOG_SEC`, 0 disables; 30 s allowance before a
generation's first segment) it (1) logs the fill level of the three
now-named egress queues q_ts/q_v/q_a — **this splits the two run-4
suspects**: video/audio queues holding data = hlssink3's muxer
starved/blocked, all near-empty = tsdemux stopped emitting; (2) sends EOS
with a 5 s bounded drain so segments the wedged muxer still holds get
flushed and uploaded (run 4's EOS released three); (3) drops the pipeline
to NULL and rebuilds it. Segment names carry the generation
(`seg-gNNNN-%05d.ts`) because the uploader tracks uploads by filename and
holds back the lexicographically-newest file — reused names would never
re-upload; the rebuilt generation's first segment is pre-tagged
`#EXT-X-DISCONTINUITY` (deterministic name, hlssink3 restarts %05d at 0).
The uploader, gate→playlist plumbing, and Ctrl+C handler now span
generations (Ctrl+C EOS-es the *current* pipeline via a shared slot).
Watchdog progress is segments only — strata-stats stayed green through
run 4's 98 s of dead air, so nothing else counts as a heartbeat. Field
script: forwards `STRATA_EGRESS_WATCHDOG_SEC`, shows `wd_restarts=N` per
tick, and the verdict now separates FAILED (still stalled at run end) from
RECOVERED (stalled ≥20 s mid-run but resumed); OK carries a restart count
when the watchdog healed within the warning threshold. clippy clean,
47+1 strata-gst tests pass. GitNexus impact/detect_changes still down
(DB v42 vs MCP server v41 — server binary predates the index format);
blast radius verified by inspection (run_receiver's only caller is main).
Still open: sender AQM self-holes seeding trigger bursts (3 occurrences),
and whether `tsparse set-timestamps=true` should go — the watchdog's
trip-time queue dump should tell us which layer to blame first.

## 2026-07-04 (later) — run 5 (orangepi-123888): watchdog diagnosed the wedge, then died rebuilding — SQPOLL rebind race fixed

Run 5 was the watchdog's first live trip and it **answered the run-4 suspect
question**: at trip time `q_ts: 0`, `q_v: 275 buffers / 10,025 ms` (pegged at
its 10 s cap, leaky and discarding), `q_a: 695 ms` — tsdemux was alive and
emitting; **hlssink3's internal muxer had stopped consuming**. The EOS-flush
salvage recovered 12 held segments (g0000-00193…00204, all uploaded; 203
total). Then the generation-1 rebuild died: `Error: StateChangeError`, run
FAILED at ~95 s with 193 segments produced.

Root cause of the rebuild death: **rebind race against deferred io_uring
teardown**. The link readers run monoio with `setup_sqpoll(1000)`; the kernel
releases ring-referenced socket fds asynchronously (io_ring_exit_work) after
the reader threads join, so a same-process rebind of 5000/5002 can
transiently hit EADDRINUSE. Evidence: gen-0 logged both links'
"monoio runtime with SQPOLL enabled" 1 ms apart; gen-1 logged only link 0's
before dying — link 1's bind failed inside `stratasrc::start()`. A local
repro test (`stratasrc_rebinds_same_ports_after_null`) passes because the
sandbox falls back to non-SQPOLL where teardown is synchronous.

Fixes (strata_pipeline.rs): (1) on `set_state(Playing)` failure the bus is
drained and the failing element's real error is printed — previously the `?`
return skipped the bus entirely, which is why the log never named the bind
error; (2) watchdog rebuilds retry up to 5× with a 1 s pause
(MAX_REBUILD_ATTEMPTS/REBUILD_RETRY_PAUSE) — generation 0 still fails fast
(that's a misconfiguration, not a race). Script fixes: local HLS preview was
double-broken — the remote `pkill -f 'http\.server …'` matched the wrapper
shell's own cmdline and killed it before `setsid python3 -m http.server` ever
ran (the "connection refused" in run 5), and a stale tunnel holding local
8088 made the new tunnel's bind fail silently. Now: `[h]ttp` bracket trick,
broader stale-tunnel kill (`-L <port>:` any form), `ExitOnForwardFailure=yes`
plus a liveness check that warns loudly. FAILED verdict now reports produced
segments, not the ~11-file directory window. clippy clean, 48 strata-gst lib
tests pass (47 + new rebind regression test). GitNexus impact still down
(v42/v41); blast radius by inspection unchanged.

Still open: sender AQM self-holes (the loss burst that seeded this wedge is
occurrence 4), and now that the wedge is **located** — hlssink3 muxer starved
while fed — the `tsparse set-timestamps=true` question sharpens: inflated/
stalled timestamps reaching the muxer would explain it waiting forever for a
segment boundary. Next wedge's trip dump can confirm from a healthy rebuild.

## 2026-07-04 (later still) — run 6 (orangepi-128932): rebind race confirmed under load, retry budget wasn't enough — fixed at the root with SO_REUSEADDR

Ran again with the run-5 fixes deployed. The watchdog tripped once more and
**reproduced the run-5 diagnosis exactly**: `q_v: 120 buffers/9742 ms`
(pegged), `q_ts`/`q_a` empty — hlssink3's muxer starved while fed, tsdemux
exonerated again. EOS flush released 6 more held segments; verdict reports
214 segments produced. The bus-error surfacing worked too: the log now
clearly reads `Failed to bind link 0.0.0.0:5000: Address already in use`.

But the rebuild retry (5×, 1 s pause) still lost: **every single attempt**
failed with the same EADDRINUSE, over a ~5 s window, before giving up with
`Error: StateChangeError`. This run had much heavier prior traffic than run
5 or the local repro (~190k packets across both links, vs. a 50-packet
burst) — the deferred SQPOLL io_uring fd release apparently scales with
in-flight ring state, so it can outlast a 5 s retry budget under real load.
The retry mechanism wasn't wrong, just undersized for the thing it was
covering for.

Fixed at the root instead of widening the budget: `Receiver::add_link`
(`crates/strata-bonding/src/receiver/transport.rs`) now binds its UDP
sockets through a new `bind_udp_reuseaddr()` helper that sets
`SO_REUSEADDR` before `bind()` (via `socket2`, since `std::net::UdpSocket::
bind` doesn't expose a hook to set options first). This lets a same-process
rebind proceed even while the kernel is still finishing async cleanup of the
old socket's io_uring registration — the old socket is genuinely gone from
routing at that point, so this doesn't risk split delivery, it just stops
the bind from tripping on a stale refcount. Added `socket2 = "0.6"` to
`strata-bonding`'s Cargo.toml (already resolved transitively via
`quinn-udp`, no new version pulled in). The rebuild retry loop in
`strata_pipeline.rs` is left as-is — a backstop for a genuinely-held port,
no longer the primary defense.

Verification: `cargo check -p strata-bonding` clean; 11
`receiver::transport` unit tests pass; 48 `strata-gst` lib tests pass
(including `stratasrc_rebinds_same_ports_after_null`, still meaningful as
the non-SQPOLL-path guard); clippy clean on both crates. Blast radius
checked by inspection (GitNexus impact/detect_changes still down, v42/v41):
`add_link`'s callers are `strata_receiver`/`strata_probe`/`strata-sim`'s
CLI mains and `stratasrc::start()` — all pass a `SocketAddr` through
unchanged, behavior is identical except for the new socket option.

Still open: sender AQM self-holes (occurrence 4, unchanged); the
`tsparse set-timestamps=true` question — the muxer is now confirmed twice as
the wedge site, still waiting on a healthy full heal cycle (trip → rebuild
succeeds → RECOVERED/OK verdict) to get a trip dump uncomplicated by a
receiver death.

## 2026-07-04 (packaging) — v1.0 packaging layer created

`packaging/` (systemd units for sender/receiver/control, env examples,
role installer, Caddyfile example, README), `docker-compose.prod.yml`
(postgres + control, required POSTGRES_PASSWORD/JWT_SEED_B64, loopback
:3000), release.yml aarch64 build made blocking (+fail_on_unmatched_files).
Key decision: sender unit grants CAP_NET_RAW via AmbientCapabilities and
strata-pipeline is NOT setcap'd — NoNewPrivileges=true blocks file-cap
elevation but is compatible with inherited ambient caps. Also: control's
CORS_ALLOWED_ORIGINS/METRICS_TOKEN are presence-sensitive (empty ≠ unset),
so compose passes them only as commented opt-ins.

## 2026-07-05 — v1.0 push: platform decisions closed, convergence chain, core-binary refactor

Big session, eight commits on main:

- **Repo hygiene**: root review docs archived to `raw/`, stale `dist/`,
  `.env.bak`, orphaned `crates/strata-portal/` leftovers removed.
- **E3 resolved**: control-plane CORS is same-origin by default
  (`CORS_ALLOWED_ORIGINS` opts in; `*` = old permissive, warned);
  `/metrics` honors `METRICS_TOKEN` (Bearer). **E10 resolved**: portal
  keeps its onboarding API, serves a minimal inline status/enroll page,
  permissive CORS dropped; `PORTAL_DIR`/tower-http gone. dev Dockerfiles
  were unbuildable since the E1 protocol split (missing strata-protocol,
  agent image still built the deleted portal crate) — fixed.
- **Integration bug found & fixed (the convergence milestone's first
  scalp)**: the receiver daemon spawns `strata-pipeline receiver
  --stats-dest …` but the pipeline's arg parser exit(1)'d on the unknown
  flag — every platform-spawned receiver pipeline died instantly. Fixed,
  and the whole egress-intelligence chain from the field script now
  travels natively: pipeline emits `egress` (segments_produced across
  generations, wd_restarts, last_segment_age_ms) in the stats relay →
  `EgressStats` in strata-protocol → receiver daemon forwards → control
  caches per stream_id + replays to late dashboards → dashboard renders
  an HLS Egress card (flowing/stalling/stalled at 5 s/15 s).
- **FEC hot-update gap closed by deletion**: `FecConfigUpdate` had no
  producer (its dashboard knobs died as placebo in E1) and a manual FEC
  pin would fight the adaptive loop — surface removed, wire-compatible.
- **Binary collision fixed**: bonding dev tool is now `strata-probe-recv`
  (explicit [[bin]]); version-check script drops strata-portal, adds
  strata-protocol.
- **strata-pipeline refactored**: clap derive + split into
  `src/bin/strata_pipeline/{main,cli,sender,receiver,gate,hotswap,stats,util}.rs`.
  Flag surface frozen and verified against daemons/scripts/wiki; one
  help-text lie fixed (--bitrate default is and was 1000, not 2000).
- **Netem triage**: the three `#[ignore = "Phase A … work in progress"]`
  reasons were stale (BiscayController landed); reworded to "needs
  root/netns; revalidate locally".
- **Unwrap audit** (daemons + control): effectively clean — everything is
  test helpers, infallible serde-to-String, or `writeln!` to String. One
  real fix: receiver telemetry's fallback UDP bind could panic the whole
  telemetry task; now logs and skips the tick.
- **Docs**: new wiki/Platform-Operations.md (operator manual) and
  wiki/Daemon-Configuration.md (config story); Deployment.md points at
  packaging/ and is marked as the manual path; Strata-Platform.md portal
  section updated; README gained a Production Deployment section.
- Sender daemon lifecycle test suite (fake control-plane WS server,
  drives the real binary) inherited from a spend-limit-killed subagent —
  validated and kept if green.

GitNexus MCP tools were unavailable all session (DB format v42 vs server
v41) — blast-radius checks done manually (grep callers + crate tests).

## 2026-07-05 (evening) — Production deploy to Hetzner + Orange Pi; the convergence bugs surface

Deployed the platform for real over SSH: control plane (native systemd +
Postgres 16, dashboard WASM dist) and receiver daemon on the Hetzner box
(root@65.109.5.169, aarch64), sender agent on the Orange Pi 5 Plus
(192.168.50.55). Full chain validated live: enroll (one-time token) →
ed25519 reconnect → stream.start → bonded links over real Band 8 → HLS
egress to the YouTube field ingest key → egress + receiver-link telemetry
on the dashboard WS. Deploy artifacts came from `make cross-aarch64` (both
boxes are aarch64; glibc floor = Pi's jammy 2.35). No public domain yet →
control speaks plain ws:// on 3000 with SSH-tunnel admin; Caddy TLS is a
one-step upgrade once a domain points at the box.

The deploy shook out exactly the class of integration bugs the
convergence milestone predicted:
- **Platform streams never bonded across modems**: agent spawned the
  pipeline with same-host destinations and no interface bindings; `ip
  route get` resolved every link onto the default route (one modem, one
  carrier NAT IP, 13 watchdog rebuilds in 25 min). Fixed by zipping
  destinations onto connected interfaces via TOML [[links]]. Verified:
  two modems arriving from distinct carrier IPs; dead LAN link correctly
  sidelined by alive-detection.
- **Receiver-side links were always empty**: receiver-mode strata-stats
  uses packets_received_link_N keys, but the relay serialized with the
  sender-style walk. New serialize_receiver_stats + per-link
  bytes_received plumbed from the receiver transport.
- **StartLimitIntervalSec sat in [Service]** (systemd ignores it) — first
  real `install.sh` run caught it. Moved to [Unit] in all three units.
- **aarch64 release never shipped strata-control** (stale x86-cloud
  assumption). Fixed in release.yml + SHA256SUMS now published.
- **Open registration on a public IP**: require_role is a stub, so any
  account controls the fleet → DISABLE_REGISTRATION env, set on the box.
- **updates.install was a placebo** (success:true, did nothing) — now
  reports failure pointing at the real path.

Update story shipped (user asked "how do we stay up to date"): GitHub
Releases as the channel, packaging/strata-update.sh as the pull-based
updater (stamp file, SHA256SUMS verify, atomic swap, refuses while a
stream is live, exit 75), opt-in strata-update.timer for unattended
fleets, wiki/Updates-and-Releases.md for the whole story incl. rollout
order (control → receivers → senders).

Receiver daemon lifecycle suite landed (5/5) — twin of the sender's,
including a stats-relay fidelity test (links + egress). Netem Phase A
revalidated on the Hetzner box: 2/3 pass once the host firewall allows
the mgmt-veth subnets ('No stats received' was ufw, not the tests);
capacity_estimation_converges genuinely fails — the estimate sits at
capacity_floor_bps (1.5 Mbps) vs a 5 Mbps link, matching live logs.
Recorded in the ignore strings as a real finding.

Field notes: the videotestsrc ball pattern compresses to ~100 kbps, so
"bitrate hold high" is unprovable with the test source — final validation
run switched to the real camera (/dev/video0). Capacity floor pinning at
low goodput is now a known estimator signature (netem + field agree).

GitNexus MCP still v42/v41-broken; manual blast-radius checks throughout.

## 2026-07-06 — UX & trust audit after the operator's failed livestream

Operator couldn't get a watchable stream from the dashboard (night of
07-05). Full forensics + code audit: raw/UX_TRUST_AUDIT.md. Verdict: NO
re-architecture — the platform's bones (protocol, enrollment, reconciler,
transport) worked as designed; six edge defects chained: (F1) link 0
pinned to the routeless LAN because pinning ignores routes and the
dashboard toggles; (F2) Go Live modal has NO source picker — always sends
source:None → server defaults to test pattern; (F3) camera hot-switch to
/dev/video1 (metadata node, not capture) crashed the pipeline while the UI
said "Source switched successfully" (fire-and-forget); (F4) dashboard
interface toggle panics the pipeline (hotswap.rs:423 reads pad property
'interface' on the mux data pad — gstreamer-rs panics on missing
property); (F5) stream end reasons discarded at BOTH layers
(ws_agent.rs:447 drops device reasons; dashboard drops control-plane
reasons with `..`) so crashes render as clean "ended"; (F6) trickle +
min-bitrate 3000 → receiver egress-watchdog rebuild loop. 16 U-findings +
P0/P1/P2 fix plan in the doc. Registration answer: relays enroll fine via
REST+token (multi-receiver, least-loaded pick) but there is NO receiver UI
and per-device config is thin (daemon flags overwrite DB; bonding_config
always null). Box hygiene: stopped the broken stream via API; Pi timezone
Asia/Shanghai → Europe/London. Rig cheat-sheet (video0=camera capture,
video1=metadata, eth0=modem 2!, enP4p65s0=routeless LAN) is in the doc.

## 2026-07-06 (later) — implemented every fix from the UX & trust audit

All 16 U-findings from raw/UX_TRUST_AUDIT.md implemented and ticked off in
the doc (each heading carries a "✅ FIXED" note describing the fix).
Highlights: toggle_link no longer panics the pipeline (find_property
guard); stream end reasons persist end-to-end (StreamEnded.error +
end_reason/end_inferred columns, migration 004 — readoption now keys off
end_inferred, not error_message) and render in the dashboard (dismissible
"why it ended" notice + streams Reason column + restarted_from lineage);
Go Live has a source picker defaulting to the first real camera, with a
TEST PATTERN badge when streaming the test source; source.switch and
interface commands are acked end-to-end (request_id) so "success" is now
device-confirmed; interface scans report driver/bus/USB-product/subnet/
gateway/has_default_route and probe HiLink modem gateways over their HTTP
API (new strata-sender/src/hilink.rs — carrier, RAT, band, cell, RSRP,
15 s cache); link pinning uses eligible_interfaces() (admin-enabled ∧
connected ∧ default-routed) and the final link→interface mapping overlays
per-link stats; admin toggles persist to /var/lib/strata/
interface-admin.json; video pickers only list VIDIOC_QUERYCAP capture
nodes and the daemon pre-rejects bad devices on source.switch;
total_bytes is real; the starvation WARN logs transitions only; the
dashboard gained a Receivers page (register/list/delete + one-time
token), real has_role gating, seeded+acked multi-dest state, WS
auth-failure badge, device-status staleness ageing, scoped interface
errors, and local-time timestamps. Verified: protocol 49, sender 8+4,
receiver 5, control integration 25 (Postgres, migration 004 applied),
bonding net 44 — all green; clippy clean on touched crates. Not yet
redeployed to the boxes — needs make cross-aarch64 + install.

## 2026-07-06 (later still) — redeployed all 16 audit fixes to Hetzner + Pi

`make cross-aarch64` (built strata-control/receiver/sender/pipeline +
libgststrata.so together — the Docker cross image already compiles all
four) plus `trunk build --release` for the dashboard WASM bundle. Took a
`pg_dump` backup on Hetzner first (9 rows, no live streams at deploy
time), then rsync'd binaries + dashboard dist to `/tmp` on each box,
stopped services, swapped `/usr/local/bin/*` + the gstreamer plugin +
`/usr/local/share/strata/dashboard`, restarted in control → receiver →
sender order. Migration 004 (`end_reason`/`end_inferred`/`restarted_from`)
applied cleanly on control-plane startup, confirmed via `\d streams`. All
three services reconnected on their persisted ed25519 device keys (no
re-enrollment needed) with zero errors/panics in the post-restart logs —
only expected transient "connection refused" on the sender while the
control plane was mid-restart, which self-healed on the next retry.
Did **not** re-apply `setcap` to the Pi's `strata-pipeline`: the sender
unit grants `CAP_NET_RAW` via `AmbientCapabilities`, and `NoNewPrivileges`
in that unit means a setcap'd binary would silently fail to gain the
capability instead — plain binary swap is correct there.
Next: drive an actual camera stream through the redeployed stack to
confirm the audit fixes hold live (dashboard.md still just says "next
step" for that — genuinely still open).

## 2026-07-06 (live-fire) — first real stream found a HiLink probe bug + confirmed a known transport bug

User went live for real: camera on, bitrate flowing sender→receiver, but
YouTube never showed the stream. Diagnosed two separate things:

1. **New bug in this session's `hilink.rs`.** `http_get` called
   `read_to_end()` expecting the socket to close, but HiLink's httpd
   ignores the client's `Connection: close` and always answers
   `Connection: Keep-Alive` — confirmed with a raw `/dev/tcp` request that
   returned the full body instantly but left the socket open for 45s+.
   Every probe call blocked for the full 1200ms timeout and returned
   `None`, so the dashboard's carrier/RSRP/band fields were silently
   always null. Fixed by reading exactly `Content-Length` bytes instead
   (`read_http_body`); added a regression test that spins up a local
   listener replying with `Connection: Keep-Alive` and never closing the
   socket, asserting the read still returns promptly. Rebuilt, redeployed
   to the Pi (control-plane had no live stream at the time, safe to
   restart), confirmed live: `carrier="3 UK" signal_dbm=-104/-103
   technology=LTE band=20` now populate correctly.
2. **Root cause of "YouTube gets nothing" is the pre-existing
   capacity-floor-pinning bug**, not anything from the audit fixes or the
   redeploy. `min_bitrate_kbps=3000` on the destination sat well above
   the actual per-link capacity at RSRP ~-104dBm (~1.2-1.5 Mbps/link, up
   to 45% loss). Sender logs show `sustained=true → reduce=true` but the
   adapter can't cut below the configured floor, so it keeps forcing 3
   Mbps into links that can't carry it, causing self-inflicted AQM
   congestion collapse (hundreds to 1000+ drops/sec) that starves the
   receiver's HLS segmenter. The egress watchdog then rebuilds the
   pipeline every 30-40s ("no HLS segment for 15s"), and each rebuild
   resets the HLS media-sequence from scratch — so YouTube's ingest never
   accumulates a continuous stream even though every individual segment
   PUT gets HTTP 202 Accepted. Confirmed via curl that the CID/network
   path/ingest endpoint are all fine in isolation. Not fixed in code this
   session (still the known "capacity-floor estimator pinning" item) —
   told the user to retry with saner bitrate settings (min≈500,
   target≈1500, max≈2500 kbps) as an immediate workaround.

Also hit (and diagnosed) the documented `cargo test | tail` wedge hazard
mid-session: a leaked `fake-pipeline.sh` test helper from
`daemon_lifecycle` integration tests got reparented to init and kept a
pipe open for 3+ hours after `cargo test` itself had finished, blocking
`tail` on EOF that would never come. Found it via `/proc/<pid>/fd` pipe-
inode matching, killed it, output flushed immediately. Confirms the
existing workaround note (avoid `cargo test | tail`, or be ready to hunt
down and kill orphaned children by pipe inode).

## 2026-07-06 (night) — YouTube ingest A/B test: key+plumbing proven fine, Strata's output is what's rejected

Second live attempt (same 3000/5000/6000 encoder settings, user didn't
lower them) — but this time the radio was good: band 20, aggregate
capacity 7–9 Mbps, <1% loss, encoder steady at 3000–3250 kbps, only one
egress-watchdog rebuild in the whole run. Clean full-video HEVC+AAC
segments (ffprobe-verified) streamed for minutes, an established TLS
connection to a.upload.youtube.com ACKed megabytes — and YouTube still
showed nothing. Proved YouTube's 202s are meaningless by PUTting literal
garbage: also 202. Ingest failures are fully silent.

A/B test: stopped the stream (correctly recorded
`end_reason=control_plane_stop, end_inferred=false` — migration 004
machinery observed working in prod), then pushed ffmpeg
testsrc2 H.264+AAC HLS directly to the same ingest URL from the Hetzner
box. **User confirmed it appeared on YouTube.** So: stream key, event,
URL format, PUT mechanics, playlist ordering — all fine. What YouTube
silently discards is specifically Strata's output. Two suspects left:
(a) HEVC (mpph265enc Main profile) vs YouTube's HLS ingest; (b) the
`#EXT-X-DISCONTINUITY`/`#EXT-X-DISCONTINUITY-SEQUENCE` tags hls_upload.rs
injects (~45% of segments were tagged early in the run — ingest
endpoints often don't tolerate discontinuity tags at all). Discriminator
queued: same ffmpeg push with libx265; appears → kill/gate the
discontinuity rewriting; doesn't → default YouTube destinations to
H.264 (mpph264enc). Evidence: /root/hls-debug-20260706/ on Hetzner
(final playlist, last 4 segments, ffmpeg test log).

Earlier the same evening: deployed the hilink.rs keep-alive fix to the
Pi (modem probe now live in prod: carrier="3 UK", LTE, band 20, RSRP
-104/-103 dBm on both links) after the Pi came back from a power cycle.

## 2026-07-10 — floor-yield, survivable profile mins, HLS media-sequence continuity, test-harness leak fix

Implemented three of the improvement areas from the 2026-07-06 session
(items 1, 2 and 5 of the post-livestream list):

**1a. Adaptation floor-yield (`adaptation.rs`).** The 2026-07-05 collapse
mode — `min_bitrate_kbps` pinned above deliverable capacity, every reduce
decision logging `reduce=true` but clamping back to the floor while the
paced-queue AQM shredded the stream — now self-heals. New latch: 3+
consecutive self-congested ticks (sustained AQM drops at pressure ≥ 0.7)
with the target already at the configured floor → the floor yields to an
emergency 300 kbps floor (`EMERGENCY_FLOOR_KBPS`). All congestion floors
(`update()` clamp, over-pressure cut, feedback cut, `effective_floor_kbps`,
LinkFailure collapse) route through `floor_kbps()`. The latch releases only
once the target has ramped back UP to the configured floor under real
capacity — releasing earlier would let the min-clamp snap the encoder
straight back into the congestion that latched it. Found + fixed en route:
the `target_changed` >10% commit gate silently blocked every +250 kbps
Recovery ramp step past ~2.5 Mbps (250/2500 = 10%), which would have
stranded a yielded encoder below a 3 Mbps floor forever; Recovery steps
now bypass the pct gate, but ONLY while below the configured floor — an
unconditional bypass made every-tick increases suppress the feedback cut
path via `increased_this_tick` (two existing regression tests caught
that immediately). The general above-floor ramp stall (encoder cannot
climb past ~2.5 Mbps by 250-steps after a deep cut) is still there —
flagged as a follow-up, needs the increase-vs-feedback-cut interplay
resolved first. Two new field-regression tests; 362 bonding lib tests
green.

**1b. Profile mins are now survival floors (`profiles.rs`).** The min
column of the bitrate table was YouTube's *quality-recommended* minimum
(1080p30 = 3000 kbps) but is consumed as the adapter's hard floor by both
streams.rs (control defaults) and strata_pipeline sender.rs (CLI
fallback). Lowered across the board to lowest-watchable (1080p30 → 1000,
720p30 → 800, 4K30 → 3000, etc.); default/max unchanged.

**2. HLS media-sequence continuity (`hls_upload.rs`).** Every watchdog
rebuild restarted hlssink3's `#EXT-X-MEDIA-SEQUENCE` at 0 while uploading
to the same live playlist URL — a backwards jump violates RFC 8216
§6.2.2 and is exactly the kind of thing an ingest server silently drops
a stream over (relevant to the open YouTube-rejection investigation).
The uploader now assigns its own continuous numbering keyed by segment
filename (unique across generations via the `seg-gNNNN-` prefix): within
a generation the uploaded playlist is byte-identical to hlssink3's; after
a rebuild the sequence keeps counting forward. Pairs with the existing
first-segment-of-generation `#EXT-X-DISCONTINUITY` tagging.

**5. daemon_lifecycle leak (`daemon_lifecycle.rs`).** `FakePipelineScript`
now kills its script by pidfile on Drop. The daemon dies by SIGKILL
(`kill_on_drop`) so it never reaps the fake pipeline; the orphan looped
forever holding inherited pipe fds — the exact mechanism that wedged
`cargo test | tail` for 3 hours on 2026-07-06.

All suites green (362 bonding, 13 sender, 27 hls_upload, 7 profiles,
28 control), clippy clean across the four touched crates.

## 2026-07-11 — ramp-stall interplay resolved + profile recommendations in the UI

**Adaptation: the above-floor ramp stall is fixed properly.** Three
coordinated changes in `adaptation.rs`: (1) ramp-up now pauses while a
receiver congestion signal is being tracked toward the sustain gate
(`congestion_started.is_none()` gate in compute_target) — ramping and
cut evaluation can no longer fight across ticks; (2) the
`!increased_this_tick` exclusions in the grace-mode raw_signal were
removed — they were circular (increase blocks signal → sustain clock
never starts → cut never lands → ramp continues), and the signals are
stale-immune by construction since burst_loss and both shortfalls
compare against the PRE-update target; (3) with the interplay gone, the
Recovery bypass of the >10% commit gate is now unconditional, so the
encoder can climb past ~2.5 Mbps again after any deep cut (previously
+250-kbps steps silently never committed above 2.5 Mbps — the
floor-yield unlatch also depends on this climb). Two new regression
tests (`ramp_up_climbs_past_the_relative_commit_gate`,
`ramp_up_pauses_while_congestion_signal_pending`); the clean-channel
reorder test's assertion relaxed from "unchanged" to "no cut" since
ramping up on a clean channel is now correct. 364 bonding lib tests
green.

**Dashboard: profile-based bitrate recommendations.** The Go Live modal
gains Resolution + Framerate pickers (the `RESOLUTIONS`/`FRAMERATES`
constants finally wired; selections now flow into SourceConfig instead
of the hardcoded 1080p30) plus a live recommendation panel — for the
chosen res+fps+codec it shows the profile's start bitrate and adaptive
range, computed client-side via `strata_protocol::profiles::
lookup_profile` (dashboard already links strata-protocol; no new API).
The Encoder Settings card's manual bitrate slider now shows
"Recommended for {res}@{fps} {codec}: N kbps (adaptive range min–max)"
with a Use button — driven by the RUNNING stream's shape parsed from
`StreamDetail.config_json` (the resolved config the control plane
stores at start), so it stays accurate across page reloads.

Suites: 364 bonding, 13 sender, 28 control, 52 gst, 49 protocol;
clippy clean incl. the dashboard; trunk release build OK.

## 2026-07-11 (deploy) — 61f36a6 + 0360774 shipped to production

Deployed both commits (floor-yield + survivable profile mins + HLS
media-sequence continuity + ramp-stall fix + dashboard bitrate
recommendations) to both boxes via `make cross-aarch64` + rsync/systemd
swap, control → receiver → sender order. Pre-deploy: no live streams,
Postgres backed up to `/root/strata-predeploy-20260711.sql`. Hetzner got
strata-control, strata-receiver, strata-pipeline, libgststrata.so and
the new dashboard bundle (served hash changed, login verified); the Pi
got strata-sender, strata-pipeline, libgststrata.so (no setcap — the
unit's AmbientCapabilities grants CAP_NET_RAW). SHA256s verified after
transfer (first scp attempt timed out mid-file on the slow uplink;
rsync --partial resumed it). All services reconnected and authenticated
on persisted ed25519 identity with zero errors; heartbeats flowing with
live modem probe data (both modems "3 UK", LTE band 20, RSRP -98/-101).
Rollback copies of the previous binaries + dashboard are in
`/root/rollback-20260711/` on each box.

## 2026-07-11 — field test: YouTube "excellent connection, no video" root-caused

First live stream on the new binaries reproduced the silent rejection.
Investigation on the live box: segments decode clean (HEVC Main
IDR_W_RADL closed-GOP + AAC — meets YouTube's stated HLS-ingest
requirements, so the codec theory is dead), all PUTs return 2xx, but
hlssink3 writes **no `#EXT-X-MEDIA-SEQUENCE` tag at all** until segments
roll off its window. The 61f36a6 continuity rewrite only *replaced* an
existing tag line, so the first ~5 playlists of every watchdog
generation uploaded with an implicit sequence of 0 — and the egress
watchdog rebuilt 5 times in 14 minutes, so YouTube saw the sequence
bounce `…390 → 0 → 480 → 0…` for the whole stream (docs: tag required,
monotonic increase). Fixed in 225c797: the rewrite now inserts the
continuous number before the first `#EXTINF` when the tag is absent
(regression test `media_sequence_tag_inserted_when_missing`). Deployed
to Hetzner as an on-disk swap of `strata-pipeline` only (uploader lives
in the binary, not the plugin; checksum-verified, rollback in
`/root/rollback-20260711-hlsfix/`) — no service restart, so the fix
takes effect on the next stream start.

Secondary finding, not yet addressed: every watchdog rebuild has the
same signature — q_v full at ~10 s, q_a empty, no segment for 15 s.
hlssink3 stalls waiting for audio to interleave after the audio
Monotonic-DTS gate goes quiet following a loss burst (gate had been
dropping "non-credible" buffers right before each stall). The rebuild
resilience works but costs a ~20 s outage each time; the audio-gate
wedge is the next thing to chase if rebuild frequency stays this high.
