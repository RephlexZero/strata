# Hot — what's in flight

> Read me first. ~500 words max. Fast cache: current focus, open questions,
> and the pages that matter right now. Refresh whenever focus shifts.

## Current focus

Branch `fix/delivered-stream-integrity`. End-to-end hardening of the
delivered-stream pipeline: stream discontinuity propagation from receiver
reassembly through the egress gate, plus closing the adaptation↔drain-rate
feedback loop (retransmit admission control, self-congestion gating on
pressure). Recent fixes also tightened paced-queue bounds and AQM drop
visibility in the net layer.

## Open questions / decisions pending

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

Latest (2026-06-28): **FEC death spiral fixed.** Investigated why the
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

**Open:** demonstrate the bitrate *holding high* in a clean-radio window (this
run was link-0-degraded); consider whether one degraded link of two should pin
the global encoder as hard as it does (per-link-loss → bitrate, not just FEC).
Watch adaptive-redundancy duplication as a wire-overhead contributor when spare
is large (was off this run).

---
_Last updated: 2026-06-28_
