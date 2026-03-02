# Bursty Fix Plan

## Goal

Stabilize per-link capacity estimation so it tracks physical link limits under realistic impairments, with low latency and high CPU efficiency.

This plan treats burstiness as a systems-timing problem first, and uses a "physics guard" as a safety rail, not as the primary estimator.

## Why We Still See Bursty Samples

Even with smooth `tc netem` parameters, burstiness can be injected by software timing and batching:

- ACK emission is timer-driven (`~50ms` cadence).
- Feedback processing is tied to scheduler refresh cadence (`~100ms`).
- GSO batches multiple packets into one transmit call.
- DWRR allows short burst windows by design.
- BBR-style `btl_bw` uses high-percentile/peak-biased samples.

Result: sampled delivery rate can momentarily exceed sustainable long-term rate.

## Validated Findings From Deep Review

The latest deep dive surfaced additional high-value points. Confirmed against current code:

- `BiscayController::tick()` is not wired into the production loop, so ProbeRtt lifecycle is effectively inactive.
- ProbeRtt timing logic is fragile because entry/exit relies on `rt_prop_stamp` semantics that are easy to mis-handle.
- Pacing tokens are tracked but not strictly enforced on send-path egress.
- ACK-based delivery rate still uses synthetic bytes (`packet_count * 1200`) rather than exact delivered byte stamps.

Important nuance:

- The old `newly_acked * 1200` path has already been improved to `AckPacket.total_received` progression, which removed the worst cumulative-jump artifacts.
- However, it remains an inferred byte model (fixed payload assumption), not true per-packet delivered-byte accounting.

Priority impact:

- Move strict pacing + `tick()` wiring + robust ProbeRtt handling to Critical path.
- Keep physics guard as an essential safety rail, but not as a replacement for timing correctness.

## Design Principles

- Keep the fast path lock-light and event-driven.
- Separate control-plane timing from scheduler refresh timing.
- Use multi-timescale estimation: fast responsiveness + slow truth.
- Bound estimator output with physically plausible rails.
- Prefer bounded channels and explicit backpressure over unbounded queues.

## Phased Architecture Plan

## Phase 0: Instrumentation Baseline (No Behavior Change)

- [x] Add low-cost histograms/counters for:
  - [x] ACK inter-arrival (`us`)
  - [x] feedback queue depth
  - [x] `delta_bytes/interval` sample distribution (P50/P90/P99)
  - [x] GSO batch sizes
  - [x] DWRR burst service lengths per link
- [x] Add compile-time feature flag (e.g. `bursty_diag`) for deep diagnostics.

Exit criteria:

- Reproducible before/after burst metrics in convergence test.

## Phase 1: Decouple Feedback From Scheduler Tick

Current anti-pattern: feedback is drained during metric refresh.

Changes:

- [x] Introduce a per-link feedback worker/task that continuously drains control packets.
- [x] `refresh_metrics()` reads atomics/snapshots only; it does not own feedback I/O.
- [x] Keep lock scope small in `process_feedback` and avoid lock convoy with send path.

Expected effect:

- Lower ACK compression at sender.
- Better temporal fidelity for delivery-rate samples.

## Phase 2: Hybrid ACK Policy (Packet-Driven + Max Delay)

Replace timer-only ACK generation with hybrid policy:

- [x] Send ACK when either:
  - [x] `ack_every_n_packets` threshold reached (e.g. 8-16 packets), or
  - [x] `max_ack_delay` elapsed (e.g. 10-20ms), whichever comes first.

Keep NACK generation periodic but independent.

Expected effect:

- Smoother ACK clock without excessive control overhead.

## Phase 3: Calibration De-Burst Mode

During startup/probing (and optionally high-uncertainty periods):

- [x] Reduce DWRR burst window further (e.g. 2-4ms in Probe).
- [x] Optionally cap/disable GSO batching in calibration mode.
- [x] Relax once estimator confidence rises.

Expected effect:

- Cleaner early samples.
- Less cross-link positive feedback from startup artifacts.

## Phase 4: True Time-Paced Egress (Optional, High Impact)

Current pacing accounts tokens but does not strictly shape transmission time.

Changes:

- [x] Introduce per-link paced send queue with deadline-based dequeue.
- [x] Use high-resolution monotonic time and small batch coalescing only near deadline.
- [x] Preserve a "minimum burst" for syscall efficiency, bounded by strict max pacing debt.

Expected effect:

- Strong control over burst envelopes independent of scheduler bursts.

Implementation note:

- If full paced queue is too invasive initially, add an intermediate mode:
  - reject/defer when pacing debt exceeds threshold,
  - retry on another link or on next pacing quantum.

## Phase 5: Multi-Timescale Estimator + Physics Guard

Keep existing ACK-based estimator for responsiveness, add a slower guardrail.

- [x] Fast estimator: ACK-based `btl_bw_fast` (existing path).
- [x] Slow estimator: EWMA of
  - [x] socket send rate (`observed_bps`), and
  - [x] receiver goodput (prefer unique delivered payload basis).

Compute:

- [x] `guard_cap_bps = min(send_rate_ewma, goodput_ewma * k_goodput)` with conservative margin.
- [x] `effective_btl_bw = min(btl_bw_fast, guard_cap_bps * headroom)`.

Recommended initial constants:

- `slow_ewma_alpha`: 0.03-0.08
- `k_goodput`: 1.05-1.20
- `headroom`: 1.10-1.25
- require persistent exceedance for 1-3s before clamping hard
- release clamp with hysteresis (avoid flapping)

Important:

- Physics guard is a safety rail for spike resistance.
- It should not replace fast probing logic.

Additional hardening:

- Add a strict stale-sample policy: if feedback cadence degrades, reduce trust in fast estimator.
- Add temporary clamp escalation when retransmission ratio and RTT inflation both rise.

## Phase 6: Correctness Upgrades For Rate Sampling

Move from inferred-byte sampling to explicit delivered-byte accounting:

- Stamp send context with delivered counters/timestamps at packet send time.
- On ACK, compute delivery rate from stamped deltas (BBR-style per-packet accounting).
- Use exact payload bytes where possible, not fixed MTU guesses.

Expected effect:

- Lower susceptibility to ACK compression artifacts.
- Better physical plausibility without over-clamping.

## Rust Performance Strategy (Modern Features)

## Concurrency and Ownership

- Use dedicated async tasks/threads per responsibility:
  - data path
  - feedback path
  - metrics aggregation
- Prefer lock-free atomics for hot counters/rates.
- Use `crossbeam`/bounded MPSC channels where handoff is needed.

## Data Structures

- Replace dynamic allocations in hot loops with reusable buffers.
- Use fixed-capacity ring buffers for sample windows.
- Keep per-link state contiguous and cache-friendly.

## Timing and Sampling

- Standardize on monotonic `Instant`-based timing for all rate math.
- Avoid mixed clocks between modules.
- Store timestamps as `u64 us` only at module boundaries when needed.

## Tooling and Compiler Leverage

- Use `cargo +nightly bench`/criterion for microbench hot spots.
- Add `#[inline]` only for measured hot tiny helpers.
- Use LTO/profile-guided builds for release images where beneficial.
- Enable tracing with compile-time filtering to keep production overhead low.

## Observability and Safety

- Add feature-gated structured tracing spans for:
  - ACK processing latency
  - feedback queue lag
  - estimator clamp events
- Emit explicit metrics for guard activity:
  - `guard_clamp_ratio`
  - `guard_active_seconds`
  - `raw_vs_effective_btlbw`

## Validation Plan

- Deterministic sim matrix:
  - low loss / medium loss / correlated loss
  - with and without GSO
  - varied ACK max delay
- Success metrics:
  - reduced P99 delivery-rate spikes
  - improved monotonic proportionality for 3/5/8 links
  - lower capacity overshoot duration
  - no throughput collapse regressions

## Suggested Implementation Order

1. Phase 1 (feedback decoupling)
2. Wire `BiscayController::tick()` + clean ProbeRtt entry/exit semantics
3. Phase 2 (hybrid ACK policy)
4. Phase 4 (or intermediate strict pacing mode)
5. Phase 5 guard in soft-observe mode (log-only)
6. Turn guard enforcement on with conservative margins
7. Phase 3 startup de-burst tuning
8. Phase 6 per-packet delivered-byte accounting

## Risks and Mitigations

- Risk: Over-clamping hurts utilization.
  - Mitigation: hysteresis + minimum hold time + gradual clamp ramp.
- Risk: More ACKs increase CPU/network overhead.
  - Mitigation: hybrid ACK threshold and dynamic adaptation by bitrate.
- Risk: Additional concurrency complexity.
  - Mitigation: isolate components, bounded channels, clear ownership.

## Bottom Line

- No architecture can make cellular estimation "perfect" at every instant.
- This plan can make it robust, proportional, and hard to overestimate for long.
- The physics guard is recommended, but only after improving timing architecture so the estimator sees cleaner signals.
