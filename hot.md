# Hot — what's in flight

> Read me first. ~500 words max. Fast cache: current focus, open questions,
> and the pages that matter right now. Refresh whenever focus shifts.

## Current focus

**2026-07-04: three field runs, three faces of ONE defect — the
tsparse/tsdemux retiming layer is unstable over bonded-loss input.**
Full forensics: `runs/INVESTIGATION-2026-07-04.md`. Run 1: playout
step-shrink → DTS re-base → fatal mpegtsmux "Timestamping error" (fixed:
50 ms/s window slew + gate DTS watermark, `8317ed7`). Run 2: corrupt-PES
splice latched video +107 s → mpegtsmux interleave-deadlock → silent
egress stall at 25 s that every transport metric slept through (fixed:
`timeline_step()` WildJump bound, audio-gate logging, script egress
heartbeat + STALLED/FAILED verdict, `04a2aa5`). Run 3 (validation): 117
segments/77 s — 10× run 2 — then progressive timeline inflation (+60 s vs
wall) stalled egress; caught live by the new detector. **Run 4
(`orangepi-118293`): 4th occurrence — best stretch yet (128 segments/148 s
under rough radio, gates absorbing ~14 splice storms) then a silent wedge
for the final 98 s after a 39 %-residual loss burst: no latch, no
inflation, media ≈ wall — tsdemux stopped emitting or hlssink3's muxer
starved on audio; EOS flushed 3 held segments. Wedge sits where the gates
can't see it.** Gates contain, can't repair. **Decision resolved
(2026-07-04 night): egress watchdog implemented** — `run_receiver` runs
the pipeline in generations; 15 s without a segment
(`STRATA_EGRESS_WATCHDOG_SEC`, 0=off) → dump q_ts/q_v/q_a fill levels
(queues-full = hlssink3 muxer starved, queues-empty = tsdemux dead — the
trip itself now splits the two suspects), EOS-flush held segments (5 s
bound), rebuild at NULL. Generation-prefixed segment names
(`seg-gNNNN-%05d.ts`, uploader is filename-keyed) + first new segment
pre-tagged `#EXT-X-DISCONTINUITY`. Script shows `wd_restarts=N`, verdict
gained a RECOVERED tier. **Run 5 (`orangepi-123888`): first live trip —
SUSPECT NAMED: q_v pegged at 10 s, q_ts empty → tsdemux alive, hlssink3's
muxer stopped consuming.** EOS salvage recovered 12 held segments. But the
generation-1 rebuild died on StateChangeError: a rebind race against the
kernel's deferred SQPOLL io_uring teardown (old sockets released async
after the reader threads join → transient EADDRINUSE on 5002). Fixed:
watchdog rebuilds retry up to 5×/1 s pause (gen 0 still fails fast), and
Playing-failure now drains the bus so the real element error is printed.
Local preview was double-broken (remote pkill self-match killed the
http.server before it started; stale tunnel held local 8088 silently) —
both fixed. **Run 6 (`orangepi-128932`): confirmed the SUSPECT again
(q_v pegged 120 buf/9742 ms, q_ts/q_a empty) and confirmed the rebind-race
root cause, but broke the retry fix — all 5 attempts over ~5 s still hit
`Failed to bind link 0.0.0.0:5000: Address already in use` under real
sustained load (~190k pkts across both links vs. the light local repro),
so the retry budget was undersized, not wrong.** Real fix landed: receiver
UDP sockets now bind with `SO_REUSEADDR` (`bind_udp_reuseaddr`, new
`socket2` dep) so a same-process rebind no longer depends on winning a race
against the kernel's deferred SQPOLL io_uring fd release — the retry loop
stays as a backstop for genuine external port conflicts. **Awaiting field
validation of a full heal cycle.** Sender AQM self-holes seeded the trigger
burst for the 4th time — tuning question still open; `tsparse
set-timestamps=true` removal sharpened now that the wedge is located at the
muxer twice: stalled/inflated timestamps reaching it would explain it
waiting forever for a segment boundary. Dev QoL:
`STRATA_LOCAL_HLS_PORT` (default 8088) now
tunnels the receiver HLS dir to http://localhost:8088/playlist.m3u8 for
VLC/mpv; script verdict persists to `runs/<id>/verdict.txt`. Playout is
**adaptive under every profile** (`fixed_playout` was fb487f7-reverted;
dead config deleted `38c842a`).

`fix/adapt-goodput-not-residual` **merged to `main`** (2026-07-01): all four
fixes below, plus HLS egress hardening. Clippy clean throughout.

**Both outstanding reviews are now FULLY IMPLEMENTED (2026-07-04).**

*Control-loop audit* (`raw/review_findings.md`): everything was already done
except §2.4.2 — landed 2026-07-04 (commit `abee62b`): FEC parity sizing now
reads `max_link_loss_sustained` (asymmetric EWMA, rise ~3 ticks / fall
fast) so one HARQ-burst tick can't inject a parity burst; plus the
remaining §1a bare literals and §1b's EWMA-α naming pass (per-file consts
pointing at [wiki/Adaptation-EWMA-Conventions.md](wiki/Adaptation-EWMA-Conventions.md)).
360 bonding + 196 transport lib tests pass.

*Platform review* (`raw/PLATFORM_REVIEW.md`): E1/E2/E4/E6/E7-rest/E8 all landed
2026-07-04 in four commits:
- **E1** (`3422861`) — new wasm-safe `strata-protocol` crate is the single
  source of truth for the wire format (envelope + `proto_version`, all ~30
  messages as direction-enum variants, shared REST types); all four
  hubs/daemons dispatch exhaustively; the dashboard deleted its 41-type
  hand-copy (and with it several dead placebo controls that never had a
  server-side producer). `strata-common` is now auth/ids/identity/metrics
  only.
- **E2** (`a2b2f67`) — `stream_state.rs` owns every streams.state write;
  heartbeats carry `running_streams`; hubs reconcile every heartbeat
  (readopt inferred ends, enforce confirmed ones); WS drop = "unobserved",
  never "dead"; 30 s sweeper backstops devices that never return.
- **E4** (`8b6c04a`) — composite `<id>.<secret>` one-time enrollment tokens
  (one argon2 verify — the O(n·argon2) scan and its CPU-DoS surface are
  gone), ed25519 challenge reconnect auth, daemons persist identity before
  spending the token, decorative session JWT deleted.
- **E6/E7/E8** (`e8eb5a9`) — receiver owns its port pool
  (`receiver.stream.start` is request/ack with real allocated ports;
  `max_streams` is finally true); capacity is COUNT(*)-derived (counter
  arithmetic deleted); receiver-side stream stats broadcast to the
  dashboard and rendered beside the sender view.

Control-plane integration suite is now 25 tests (real WS handshakes for
agent/receiver/dashboard). Remaining deliberately-open platform flags: the
`CorsLayer::permissive()`/unauthenticated-`/metrics` posture decision (E3),
and `strata-sender`'s empty portal (`portal.rs`, :3001) still needing a
follow-up decision after strata-portal's retirement (E10).

*Docs*: ARCHITECTURE_REVIEW item 9 done — the three 2026-05 root-level
review/diagnosis docs are archived to `raw/`, their durable content merged
into [wiki/Control-Loop-Map.md](wiki/Control-Loop-Map.md) and
[wiki/Observability-Semantics.md](wiki/Observability-Semantics.md).

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
_Last updated: 2026-07-04 late night (run 6: rebind race confirmed under load — retry budget wasn't enough, SO_REUSEADDR fixes it at the root — awaiting a full heal cycle)_
