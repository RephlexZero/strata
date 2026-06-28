# Hot — what's in flight

> Read me first. ~500 words max. Fast cache: current focus, open questions,
> and the pages that matter right now. Refresh whenever focus shifts.

## Current focus

Branch `fix/adapt-goodput-not-residual` (off `main`, which now carries the
merged `fix/delivered-stream-integrity` work incl. the FEC death-spiral fix).
Removed the post-FEC **residual override on the encoder bitrate**: the
`ewma_loss_fec > 0.15` gates that cut the encoder (`loss_pressure`) and blocked
ramp-up (`loss_suppressed`) are gone. The encoder is now driven by the
continuous capacity path (per-link channel-loss discount → pressure), goodput
shortfall (headroom-aware, reorder-immune), AQM self-congestion and genuine
per-link melt (`link_collapse`). Residual now only feeds jitter context + FEC
burst-lift. See [Adaptation-Encoder-Cut-Signals](wiki/Adaptation-Encoder-Cut-Signals.md).

## Open questions / decisions pending

- **Field-validate this change**: finally watch the bitrate HOLD HIGH in a
  clean-radio window (prior runs were link-0-degraded) — confirm the encoder
  no longer cuts/pins when headroom exists.
- How to handle stream discontinuities when FEC partially recovers a gap —
  should the egress gate still signal a discontinuity or suppress it?
- Long-term: move capacity oracle from PPD to a hybrid BBR+PPD model once
  the drain-rate loop is stable?
- Field test on Orange Pi: validate redundancy/broadcast flag behaviour under
  real Band 8 conditions.

## Most-relevant pages right now

- [wiki/Architecture.md](wiki/Architecture.md) — transport protocol, FEC/ARQ, congestion control
- [wiki/Testing.md](wiki/Testing.md) — test matrix, simulation framework
- [index.md](index.md) — full map of all wiki pages

## Recent context

The receiver now propagates stream discontinuities end-to-end to the egress
gate (1e4c012). Net layer drain-time paced-queue bound is fixed and AQM drops
are now visible (9833b84). Adaptation loop closes on drain rate with retransmit
admission control (4e6a017). Self-congestion is now gated on pressure so a
bursty link can't permanently pin the bitrate (4cbda48).

Latest (2026-06-28): **Residual override removed from the encoder.** The two
`ewma_loss_fec > 0.15` gates (encoder cut `loss_pressure` + ramp-up block
`loss_suppressed`) are gone; the encoder follows the continuous capacity path +
goodput shortfall instead. New regression test
`high_residual_loss_with_headroom_does_not_cut_encoder` (fails on old code).
367 unit + all integration tests pass, clippy clean.
See [Adaptation-Encoder-Cut-Signals](wiki/Adaptation-Encoder-Cut-Signals.md).

Prior (2026-06-28, now on main): **FEC death spiral fixed.** Investigated why the
post-fix run stayed loss-bound — and it was NOT "lever 2" (links were clean,
~2% wire loss; EDPF already routes around loss/delay). `recommended_fec_overhead`
was sizing parity from `ewma_loss_fec` (the *post-FEC residual*, which includes
cross-link reorder + late loss parity can't fix). That fed a loop: reorder loss
→ more parity → repair microbursts overflow buffers → more late loss → more
parity. Field run pinned FEC at **41.6%** with the encoder at the 500 floor and
3.7 Mbps spare. Fix: size parity to per-link CHANNEL loss (`max_link_loss`),
not the residual (see [Adaptation-FEC-Sizing](wiki/Adaptation-FEC-Sizing.md)).
366 tests pass. **Field-validated** (run orangepi-3870): on *worse* radio than
the baseline, post-FEC residual 7%→1.5%, discontinuities 925→285, gate drops
1772→348; death-spiral signature gone (0 ticks high-FEC + low channel loss);
floor time now tracks real loss, not a phantom.

Prior (2026-06-27): removed the `max_queue_depth >= 90/60` packet-count gates
from `delay_pressure`/`late_pressure` (deep paced queue ≠ bufferbloat); see
[Adaptation-Delay-Pressure](wiki/Adaptation-Delay-Pressure.md).

**Open:** demonstrate the bitrate *holding high* in a clean-radio window (prior
runs were link-0-degraded). The "should one degraded link of two pin the global
encoder?" question is now addressed in code — per-link channel loss reaches the
encoder only via the continuous capacity-path discount, and the residual
override that pinned it is gone — but still needs field confirmation. Watch
adaptive-redundancy duplication as a wire-overhead contributor when spare is large.

---
_Last updated: 2026-06-28_
