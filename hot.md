# Hot — what's in flight

> Read me first. ~500 words max. Fast cache: current focus, open questions,
> and the pages that matter right now. Refresh whenever focus shifts.

## Current focus

`fix/adapt-goodput-not-residual` **merged to `main`** (2026-07-01): all four
fixes below, plus HLS egress hardening. 415 tests pass, clippy clean.

**Both audits (2026-07-01) are now PARTIALLY IMPLEMENTED (2026-07-02, in
progress) — plan at `.claude/plans/rosy-squishing-treasure.md`.** Landed on
`main`: L1/L5/L6/L8/N1/N2/N6/N9/L7/§2.4.1 (all of `strata-bonding`/
`strata-transport`'s congestion.rs/oracle.rs/bonding.rs/transport.rs fixes
from [review_findings.md](review_findings.md)) plus E5/E7/E10 (SQL bug,
receiver-stop wiring, bonding-config override removal, portal retirement)
from [PLATFORM_REVIEW.md](PLATFORM_REVIEW.md). See the 2026-07-02 log entry
for the full per-item list, plus one real gap it surfaced: `strata-sender`'s
local onboarding portal (`portal.rs`, :3001) has nothing left to serve now
that `strata-portal` is retired — needs a follow-up decision.

**Still to do** (see the plan file for scope): `adaptation.rs`'s
L2/L3/L4/N4/N5/N7 fixes + the §2.2 ranked-decision consolidation (highest
remaining value, highest risk — the encoder control loop); dashboard WS
auth/scoping (E3); platform timing/jitter hygiene (E9); then the larger
executive items in dependency order — E1 (one `strata-protocol` crate,
unblocks E2/E8), E2 (stream state machine + reconciliation), E4 (device
identity, kills the O(n·argon2) reconnect-storm risk), E6 (real per-stream
port allocation), E8 (surface receiver-side telemetry on the dashboard).

**Sandbox note:** this environment's `RLIMIT_MEMLOCK` is hard-capped at
8 MB, which now persistently fails 8 `strata-bonding` monoio/io_uring tests
(`OS OutOfMemory`) — confirmed on unmodified `main`, not a regression.
User approved `--no-verify` commits (reason noted in each message) since
the full-workspace pre-commit hook can't pass here regardless of code
correctness; every change was still verified with crate-scoped
`cargo test`/`clippy` before committing.

**The discontinuity /
playout-window investigation is RESOLVED — root cause was a single mux constant.**
`mpegtsmux pat-interval=1 pmt-interval=1` (those are 90 kHz ticks, default 9000,
NOT a packet count) emitted PAT/PMT before nearly every packet, **tripling wire
bandwidth** (2.3 Mbps video → 7 Mbps muxed). That overflowed the per-link
paced-queue AQM into ~243k self-inflicted holes/run — which masqueraded as
channel loss, reorder, FEC death spirals, bufferbloat, and a pinned 3 s playout
window. Fixed: `pat-interval=9000 pmt-interval=9000` (100 ms) at all 3 sender
sites. Field-validated (orangepi-57909): AQM drops 243k→330, discontinuities
~1200→121, playout off the 3 s cap to ~1.8 s. Companion: `rc-mode=cbr` on the
Rockchip encoder (AQM bursts −98%). See
[MPEG-TS-Mux-Overhead](wiki/MPEG-TS-Mux-Overhead.md).

Earlier on this branch (still valid, all field-validated): removed the post-FEC
**residual override on the encoder bitrate** (`ewma_loss_fec > 0.15` gates that
cut the encoder / blocked ramp-up), and gated the burst reflex on a real goodput
collapse. The encoder follows the continuous capacity path, goodput shortfall,
AQM self-congestion and `link_collapse`. See
[Adaptation-Encoder-Cut-Signals](wiki/Adaptation-Encoder-Cut-Signals.md).

**New (2026-06-30): receiver-side HLS egress hardening**, separate from the
adaptation work above — addresses YouTube stutter/freezes during the fade
window even though the bonding transport delivered everything on time. Three
parts, all implemented and unit-tested (368+47 tests pass, clippy clean):
1. Latency cut: `target-duration` 2→1 s, uploader poll 500→250 ms.
2. `hlssink3` migration + real `#EXT-X-DISCONTINUITY` tagging on gate resumes
   (reconstructed in `hls_upload.rs`, hlssink3 doesn't do this itself — see
   [HLS-Egress-Discontinuity-Tagging](wiki/HLS-Egress-Discontinuity-Tagging.md)
   for the two false premises this corrected and the pipeline-topology change
   it required).
3. Dropped: a proposed "drop CORRUPTED-flagged partial AUs" fix — no such
   signal exists anywhere in this pipeline (every loss, of any size, already
   reaches the gate via `pending_discont`/DISCONT).
**Field-validated** (run orangepi-72665, real Band 8, 2 links, 120 s): 6 gate
resumes (1 harmless startup + 5 real mid-stream splices from 655 lost
packets), all 5 real ones correctly attributed and tagged discontinuous 1:1;
32 segments uploaded, `damaged=0` throughout. See
[HLS-Egress-Discontinuity-Tagging](wiki/HLS-Egress-Discontinuity-Tagging.md).

## Open questions / decisions pending

- **Field-validate this change**: finally watch the bitrate HOLD HIGH in a
  clean-radio window (prior runs were link-0-degraded) — confirm the encoder
  no longer cuts/pins when headroom exists.
- ~~Stream discontinuities under real Band 8~~ — **resolved**: they were
  self-inflicted AQM loss from the `pat-interval=1` mux bloat, not a genuine
  recovery-signalling question. See [MPEG-TS-Mux-Overhead](wiki/MPEG-TS-Mux-Overhead.md).
- Long-term: move capacity oracle from PPD to a hybrid BBR+PPD model once
  the drain-rate loop is stable?
- Field test on Orange Pi: validate redundancy/broadcast flag behaviour under
  real Band 8 conditions.
- ~~Field-validate HLS discontinuity tagging~~ — **resolved** (run
  orangepi-72665): 5/5 real gate resumes correctly tagged discontinuous. Still
  worth double-checking `--audio` stays on in every production sender config —
  hlssink3 silently never cuts a single segment without audio data flowing.

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

Latest (2026-06-28): **Residual override removed from the encoder, then the
burst path gated on goodput.** First the two `ewma_loss_fec > 0.15` gates
(encoder cut `loss_pressure` + ramp-up block `loss_suppressed`) were removed.
Field test **orangepi-10360** then showed the encoder STILL floored 34% of ticks
(~5.3 Mbps spare) — via `burst_loss`/`severe_burst`, which keyed on the
*instantaneous* post-FEC residual (72 burst windows at mean 5.3 Mbps goodput =
reorder, not loss). Fix: the burst path now also requires a real
delivered-throughput collapse (goodput < 0.7× offered). FEC overhead held 16.8%
(no death spiral); ramp-up recovered to 2.5 Mbps. 368 tests pass, clippy clean.
**Field re-test pending.**
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
_Last updated: 2026-07-02 (batch 1-3 fixes landed on main; adaptation.rs consolidation + platform E1-E9 items remain)_
