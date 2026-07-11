# Hot — what's in flight

> Read me first. ~500 words max. Fast cache: current focus, open questions,
> and the pages that matter right now. Refresh whenever focus shifts.

## Current focus

**2026-07-10: three of the post-livestream improvements implemented
(NOT yet deployed).** (1) **Floor-yield in the adapter** — the 2026-07-05
collapse mode (min_bitrate pinned above deliverable capacity → AQM
shreds the stream while `reduce=true` clamps back to the floor) now
self-heals: 3+ self-congested ticks at the floor and the floor yields to
300 kbps until the target ramps home (`adaptation.rs`, `floor_kbps()`,
two field-regression tests). Profile `min_kbps` also lowered from
YouTube-quality mins to survival floors (1080p30: 3000 → 1000) since
that column feeds the adapter floor via streams.rs and sender.rs.
(2) **HLS media-sequence continuity** — the uploader now keeps
`#EXT-X-MEDIA-SEQUENCE` counting forward across watchdog rebuilds
instead of letting hlssink3 reset it to 0 mid-URL (RFC 8216 violation,
plausibly related to YouTube's silent rejection). (3) **daemon_lifecycle
orphan fix** — FakePipelineScript kills its script on Drop; no more
wedged `cargo test | tail`. **2026-07-11:** the above-floor ramp stall
is now fixed for real (ramp-up pauses while a congestion signal is
pending, the circular `increased_this_tick` exclusions are gone, and
Recovery steps bypass the >10% commit gate unconditionally — log.md
2026-07-11), and the dashboard now shows profile-based bitrate
recommendations: res+fps pickers + recommendation panel in the Go Live
modal, and a per-running-stream "Recommended: N kbps" hint with Use
button on the Encoder Settings slider. **Deploy + the libx265
discriminator test are the next field actions.**

**2026-07-06: ALL 16 AUDIT FINDINGS FIXED AND DEPLOYED.** Every U-finding
in [raw/UX_TRUST_AUDIT.md](raw/UX_TRUST_AUDIT.md) is implemented, ticked,
and now running on both boxes: toggle_link panic guard; end reasons
persisted end-to-end (`end_reason`/`end_inferred`, migration 004 — applied
live, verified via `\d streams`) and rendered (dismissible notice, Reason
column, `restarted_from` lineage); Go Live source picker (camera default +
TEST PATTERN badge); acked source.switch + interface commands; rich
interface identity (driver/bus/USB product/subnet/gateway/default-route +
live HiLink modem probe — carrier/RAT/band/RSRP via the gateway HTTP API,
`hilink.rs`); pinning filters on enabled∧connected∧default-routed and
reports the link→interface mapping in stats; persisted admin toggles;
capture-only video pickers; real total_bytes; Receivers page (register +
one-time token); real has_role; staleness ageing; local-time timestamps.
Suites: 49+8+4+5+25+44 green, clippy clean. Deployed via `make
cross-aarch64` + manual rsync/systemd swap to both boxes (control →
receiver → sender order; DB backed up to `/root/strata-*.sql` before the
migration ran); all three services reconnected on persisted ed25519
identity with zero errors in the post-deploy logs. Rig facts:
`/dev/video0` = the camera (MJPG 1080p30/60); `eth0` = **modem 2**;
`enP4p65s0` = routeless LAN. Pi TZ now Europe/London.

**2026-07-06 (live-fire test): first real camera stream to YouTube found
two bugs.** (1) `hilink.rs`'s `http_get` used `read_to_end()`, but the
HiLink httpd ignores our `Connection: close` and always replies
`Connection: Keep-Alive` — every probe blocked for the full 1200ms
timeout and silently returned `None`, so carrier/RSRP/band were `null`
on the dashboard despite the modems being reachable. Fixed: read by
`Content-Length` instead (`read_http_body`, regression test simulates a
keep-alive server that never closes the socket). Redeployed to the Pi,
confirmed live: `carrier="3 UK" signal_dbm=-104/-103 technology=LTE
band=20`. (2) **The actual "YouTube gets no data" symptom is
capacity-floor pinning** (previously flagged below, not fixed by the
audit pass): the stream's `min_bitrate_kbps=3000` floor sat well above
the real per-link capacity at RSRP ~-104dBm (~1.2-1.5 Mbps/link with up
to 45% loss). The adapter logs `sustained=true → reduce=true` but can't
go below the floor, so the encoder keeps forcing 3 Mbps into links that
can't carry it → self-inflicted AQM congestion collapse (200-1000+
drops/sec) → receiver's HLS segmenter starves → egress watchdog
rebuilds the pipeline every 30-40s → each rebuild resets the HLS
media-sequence, so YouTube's ingest never accumulates a continuous
stream even though every individual segment PUT gets HTTP 202.
**Fixed in code 2026-07-10** (floor-yield + media-sequence continuity,
see Current focus) — not yet deployed; until then the operational
workaround stands (set bitrate to real conditions, e.g. min≈500,
target≈1500, max≈2500 kbps).

**2026-07-06 (evening retry): YouTube ingest itself was the blocker —
codec/playlist suspected, key+plumbing PROVEN FINE.** Second live
attempt, same 3000/5000/6000 settings, but radio much better (band 20,
agg 7-9 Mbps, <1% loss, one watchdog rebuild total) — transport
delivered clean full-video HEVC segments for minutes, PUTs flowed
(TLS conn to a.upload.youtube.com, MBs ACKed), and YouTube still
showed nothing. Key insight: **YouTube returns HTTP 202 even for
garbage PUTs** — acceptance failures are completely silent. A/B test:
stopped the stream, pushed ffmpeg testsrc **H.264+AAC HLS to the same
ingest URL → appeared on YouTube immediately** (user confirmed).
Remaining suspects for why Strata's output is silently discarded:
(a) **H.265** — mpph265enc HEVC Main; YouTube docs claim HLS HEVC
support but unverified for this shape; (b) **`#EXT-X-DISCONTINUITY`
tags** — the uploader injected them on ~45% of segments early on
(+ `#EXT-X-DISCONTINUITY-SEQUENCE` forever after); ingest endpoints
often don't tolerate these. **Next: discriminator test — same ffmpeg
push with libx265**; if H.265 testsrc appears → codec fine, kill the
discontinuity-tag rewriting (or gate it); if it doesn't → add H.264
(mpph264enc) as the platform default / YouTube-destination default.
Evidence preserved: /root/hls-debug-20260706/ on the Hetzner box
(playlist + segments + ffmpeg test log). Stream stop correctly
recorded `end_reason=control_plane_stop, end_inferred=false` — audit
machinery working in prod.

**2026-07-05 evening: DEPLOYED — the platform runs in production.**
Control plane (systemd + Postgres 16 + dashboard) and receiver daemon on
the Hetzner box (`root@65.109.5.169`, aarch64, ufw open on 3000/tcp +
5000-5006/udp), sender agent on the Orange Pi 5 Plus (192.168.50.55).
Full chain proven live: token enroll → ed25519 reconnect → dashboard
stream start → bonded Band 8 links → HLS to the YouTube field key →
egress + receiver-link cards fed over the dashboard WS. Credentials:
`/root/strata-credentials.txt` on the Hetzner box; registration is now
CLOSED (`DISABLE_REGISTRATION=1`). No domain yet → plain ws:// on 3000,
admin via SSH tunnel; pointing a domain at the box + Caddy
(packaging/caddy/) is the TLS upgrade. Field checklist status:
- (2) platform end-to-end under real Band 8: **DONE live** (two modems
  from distinct carrier NAT IPs after the link-pinning fix; dead LAN
  link correctly sidelined).
- (3) egress telemetry across watchdog rebuilds: **DONE live**
  (segments_produced kept counting across 13 generations).
- (5) install.sh on real boxes: **DONE** (Ubuntu 24.04 + Jammy; caught
  the StartLimitIntervalSec-in-wrong-section bug).
- (1) heal cycle: watchdog tripped repeatedly in production and rebuilt
  every time with no `Address already in use` — SO_REUSEADDR holds,
  though a deliberate under-load repro is still worth one field session.
- (4) bitrate HOLD HIGH: **still open** — videotestsrc ball compresses
  to ~100 kbps so the test source can't prove it; last run of the session
  switched to the real camera (/dev/video0). Related real finding: the
  capacity estimate pins at capacity_floor_bps (1.5 Mbps) at low
  goodput — netem revalidation and live field logs agree
  (capacity_estimation_converges records it; needs estimator work).
Update story shipped: GitHub Releases + SHA256SUMS, strata-update.sh
(refuses mid-stream), opt-in strata-update.timer —
[wiki/Updates-and-Releases.md](wiki/Updates-and-Releases.md). Receiver
lifecycle suite landed (5/5). See log.md 2026-07-05 (evening).

**2026-07-05: v1.0 push landed (9 commits)** — E3/E10 closed
(CORS_ALLOWED_ORIGINS + METRICS_TOKEN; portal serves an inline page),
packaging layer shipped (`packaging/` systemd units + installer,
docker-compose.prod.yml, aarch64 release now blocking), strata-pipeline
ported to clap and split into modules (flag surface frozen), bonding dev
binary renamed `strata-probe-recv`, dead FEC hot-update surface deleted,
and **the convergence chain is in**: the field script's egress
intelligence (segment heartbeat, wd_restarts, stall state) now travels
pipeline → daemon → control → dashboard natively ("HLS Egress" card).
Found en route: platform-spawned receiver pipelines died instantly
(`--stats-dest` was rejected by the pipeline's arg parser) — exactly the
class of integration bug the convergence milestone predicted; fixed.
See log.md 2026-07-05 for the full list. New operator docs:
[wiki/Platform-Operations.md](wiki/Platform-Operations.md),
[wiki/Daemon-Configuration.md](wiki/Daemon-Configuration.md).

**Next field session — the v1.0 validation checklist:**
1. Transport heal cycle (top blocker, unchanged): reproduce a watchdog
   trip and confirm SO_REUSEADDR lets generation N+1 bind under load —
   verdict RECOVERED/OK, `wd_restarts≥1`, no `Address already in use`.
2. Platform end-to-end under real Band 8: enroll the real Orange Pi via
   strata-sender (portal or env token), click start in the dashboard,
   receiver daemon spawns the pipeline, video lands on YouTube — and the
   HLS Egress card tracks reality (compare against the script's verdict
   on a parallel run).
3. Confirm the new egress telemetry survives a watchdog rebuild
   (segments_produced keeps counting across generations).
4. Bitrate HOLD HIGH in a clean-radio window (carried over).
5. `packaging/install.sh sender` on a fresh Orange Pi image — three
   commands, stream works under the systemd unit (ambient CAP_NET_RAW,
   no setcap).

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
agent/receiver/dashboard). ~~Remaining deliberately-open platform flags:
E3 (CORS/metrics posture) and E10 (empty portal)~~ — **both resolved
2026-07-05** (see Current focus above); the E-list is now fully closed.

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

- **Blackholed link is never marked dead** (found live 2026-07-05): the
  Pi's LAN interface has no internet route; the pinned link 0 delivered
  zero packets to the receiver, yet the sender kept it alive=true with
  loss=0.000 and queued up to ~800 kbps onto it — dead-link detection is
  loss-window-based and a link with NO receiver feedback never opens a
  window. Transport-side fix wanted: "bytes in flight but no ACK for N
  seconds → dead". Mitigation today: don't pin links onto interfaces
  without a route (or wire the operator enable/disable toggle into the
  agent's link-pinning filter — currently it filters on Connected only;
  the enabled flag at scan level is always true, the real toggle lives in
  HardwareMonitor and isn't threaded into spawn_pipeline yet).
- **Capacity estimate pins at capacity_floor_bps at low traffic volume**:
  netem (capacity_estimation_converges: 1.5 vs 5 Mbps) and live field
  logs agree. Rises once real traffic flows (camera source: floor →
  ~1.4 Mbps per link estimates within a minute). Estimator work, not a
  field blocker.

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
_Last updated: 2026-07-06 (all 16 audit findings implemented — pending redeploy to the boxes)_
