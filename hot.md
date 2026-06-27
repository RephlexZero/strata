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

Latest (2026-06-27): removed the `max_queue_depth >= 90/60` packet-count gates
from the adapter's `delay_pressure`/`late_pressure` — a deep paced queue during
keyframe bursts was misread as bufferbloat and pinned the encoder to the 500
floor despite ~4.7 Mbps usable and loss ≈ 0. Bufferbloat is now AQM-drop +
receiver-delay based (see [Adaptation-Delay-Pressure](wiki/Adaptation-Delay-Pressure.md)).
Field-confirmed the false-positive class is gone (pure-bufferbloat reduces
57→0). **Open:** the post-fix run was loss-bound (~40 % post-FEC) so the
average-bitrate win isn't demonstrated yet; the remaining real loss-driven
collapse is **lever 2** (per-link loss → EDPF, route around the bad link).

---
_Last updated: 2026-06-27_
