# Strata bonded-cellular video — persistent receiver-side artifacts

## System under test

- **Sender** (laptop): GStreamer pipeline `v4l2src → vah265enc → mpegtsmux → stratasink` (the
  `strata-bonding` Rust crate). Two USB LTE modems, distinct cellular carriers, each ~1.5–2 Mbps
  capacity, ~70 ms RTT. 1080p @ 30 fps HEVC, 1 s GOP, ~1500 kbps target.
- **Receiver** (aarch64 VPS): `stratasrc → tsdemux → h265parse → hlssink2`, then the HLS
  segments are PUT to YouTube's HLS ingest endpoint (`a.upload.youtube.com/http_upload_hls`).
- **Wire protocol**: custom UDP datagram with 12-byte header (seq, ts, fragment, kf/cfg flags,
  payload checksum). Sender stripes packets across links, applies RLNC sliding-window FEC
  (default 15 % overhead). Receiver runs a reassembly buffer per generation.

## Symptom

The end user sees continuous low-level grey/blocky/fuzzy artifacts on the YouTube output — not a
single bad event, a steady ~10 % of frames showing motion-compensation breakage until the next
IDR. SRT used on a single one of the same modems works cleanly.

We have done ~10 field runs. Every previous diagnosis turned out to be either a real bug that we
fixed without removing the artifacts, or a contributing factor that did not explain the steady
rate. Bugs already fixed (each in its own commit on `main`):

- Wrong-signal cap_kbps oscillation; phantom 44 Mbps capacity reads; probe-poisoned RTT floor.
- AIMD playout drain asymmetry (40 ms/500 ms drain vs 3 ms/late_hit hit collapsed window faster
  than skew bursts recurred → 312↔933 ms oscillation).
- FEC silent-corruption hazard on recovered packets (added FNV-1a payload checksum on the wire).
- **Per-link `TimestampClock` epoch drift** — each `TransportLink::new()` previously made its own
  `Sender::new()` with its own clock epoch, so packets from the two links carried timestamps in
  different time domains. The receiver's delay_spread metric was therefore measuring inter-link
  *epoch offset* (~1700 ms) rather than real OWD spread, which had the receiver target_latency
  oscillate 933 → 1972 → 933 ms and crash the GStreamer pipeline with "Timestamping error on
  input streams". Fixed by making `TimestampClock` read from a process-wide `OnceLock<Instant>`.
- **Encoder bitrate whipsaw** — bitrate adapter was oscillating 1039 ↔ 2742 kbps every 5 s in
  response to noisy goodput readings (peak_gp would briefly read 7377 kbps — physically
  impossible on 2 × 2 Mbps links — and the adapter would jump toward that, then collapse the
  next tick when goodput corrected). Fixed with an up-only +15 %/tick slew limit in
  `BitrateAdapter::slew_clamp`.
- User asked us to test fixed bitrate (`STRATA_MIN_BITRATE=STRATA_MAX_BITRATE=1500`); confirmed
  encoder VBV churn is *contributing* but not the dominant artifact source.

## What remained, and what we now believe is the dominant cause

**`late_packets` increments at a near-constant ~16 per 5-second window across every run**,
regardless of every change above. This is approximately 3.2 "late" classifications per second
of streaming.

A "late" packet in this codebase is one whose sequence number is less than the receiver's
current `next_seq` at the moment it arrives. `next_seq` advances based on the adaptive playout
window (`ReassemblyBuffer::target_latency`). Late packets are *discarded* — not delivered to the
decoder, not counted as loss in the loss_rate metric. They are silent reference-frame holes.

Sample row from the most recent (fixed-bitrate, encoder pinned at 1500 kbps) run:

```
[Δ5s]    delivered=1191  lost=0  late=18    win_loss=0.0%   current_latency=708ms   target=709ms
[Δ5s]    delivered=991   lost=18 late=23    win_loss=1.8%   current_latency=1601ms  target=823ms
[Δ5s]    delivered=1328  lost=3  late=14    win_loss=0.2%   current_latency=877ms   target=533ms
[Δ5s]    delivered=1414  lost=2  late=16    win_loss=0.1%   current_latency=742ms   target=733ms
[Δ5s]    delivered=1187  lost=1  late=12    win_loss=0.1%   current_latency=684ms   target=684ms
[Δ5s]    delivered=1397  lost=1  late=16    win_loss=0.1%   ...
...continues for 24 windows: late=12,14,15,16,16,16,18,23 — clusters around 14-16
```

`fec_corrupt_dropped` is 0 throughout. `loss_rate` is consistently ≤ 0.01 %. FEC is recovering
~2000 packets per 2-minute run. The wire-level delivery is essentially clean.

The user-visible math is straightforward: 3.2 lates/s × ~30 fps × 1 s GOP HEVC ⇒ a missing
reference packet hits roughly 1 frame in 10. Motion compensation breaks; subsequent inter-frames
inherit the damage until the next IDR. That is exactly the visual pattern reported.

## Why we believe it is the playout window, not the wire

The adaptive playout window is sized in `aggregator.rs::ReassemblyBuffer::tick()` as roughly:

```
target_ms = start_latency_ms
          + max(jitter_component, spread_component * 1.15)
          + loss_component
          + late_pressure_ms

where spread_component = (max - min) of `arrival_us - send_ts_us` over a 4 s sliding window
```

Config defaults: `start_latency=300 ms`, `min_latency_ms=10 ms`, `max_latency_ms=2000 ms`.
In steady state the spread component settles around 100-200 ms, jitter component ~5-20 ms, so
the target settles at ~500-700 ms. We see exactly that in the field logs.

Cellular modems do per-packet HARQ retries on the air interface; a single retry adds 200-400 ms
of OWD on top of base RTT. We also run our own BBR-style ProbeBw phase per link
(`pacing_rate = 1.25 × btl_bw_est` for a short window every ~8 s), which momentarily over-drives
the modem and produces transient L2 queueing on that link. Either mechanism produces a tail of
packets that arrive 600-1500 ms behind their nominal in-order neighbours.

Our adaptive target of 500-700 ms is therefore *below* the 99th percentile of real packet OWDs,
even with no actual packet loss happening. Every packet whose arrival exceeds the target is
discarded as late.

## Why SRT does not have this on the same modems

SRT uses a single fixed user-configured latency (typically 1000-2000 ms), single-link, no
bonding. It always waits the full configured window before declaring loss; it never adapts the
playout window downward to chase the average. The trade is +1 s of glass-to-glass latency for
outlier-tolerant playback. For a YouTube-live use case (which adds 5-30 s of its own buffering
on top) the extra second is invisible.

Strata bonds two links and is therefore subject to inter-link OWD spread *on top of* each link's
own HARQ-retry tail. The adaptive playout was designed to keep glass-to-glass latency low for
real-time use cases — and on a clean wired/wifi link with two senders that's a sound design —
but on bonded cellular it leaves no headroom for either the per-modem HARQ tail or our own
probe-induced queueing.

## Proposed paths forward (we want a second opinion)

1. **Raise the playout latency floor to SRT-like values.** Constants change only:
   `min_latency_ms 10 → 1500`, `max_latency_ms 2000 → 3000`, `start_latency 300 → 1500`. The
   window can still grow upward during fades but cannot shrink below the worst-case HARQ-retry
   tail. Expected effect: `late_packets/sec` drops from ~3.2 to <0.1, artifacts mostly disappear.
   Cost: ~1 s additional glass-to-glass latency. For our YouTube-live use case this is free.

2. **Make playout latency a single fixed user-configured value (true SRT semantics).** Add an
   env / CLI knob (e.g. `STRATA_PLAYOUT_LATENCY_MS=1500`) that pins the window with no
   adaptation. Easier to A/B test. Loses adaptive behavior we don't appear to need.

3. **Keep adaptive but make decreases EWMA-smoothed.** Snap up to spread/jitter spikes
   instantly, but decay down over ~30 s. Preserves the "low latency when calm" property in
   principle but is harder to get right (and the field shows it has been getting it subtly wrong
   for many runs).

4. **Instrument first.** Add per-packet OWD-percentile telemetry, run again, look at the actual
   p95/p99 packet-arrival distribution before changing the constants. Most cautious, slowest.

We are leaning hard toward option 1 — the user has explicitly said latency doesn't matter for
their use case, and the diagnosis is consistent with a long, otherwise-unexplained pattern in
every field run we have.

## Open questions / things we are not certain about

- Is the **3.2 lates/sec figure too uniform** to be HARQ-retry-driven? HARQ events on cellular
  are usually bursty (one bad radio block → several retries in quick succession). A near-constant
  rate across 24 windows could instead indicate a deterministic process — for example, our own
  probe schedule (each link probes ~once every 8 s, two links = ~4 probe events / 8 s window
  = ~2.5 probe events per 5 s window). The 16-per-window number is suspiciously close to "one
  per probe event × packets per probe burst". Probe-induced lates are plausible.
- **Why exactly 16?** A few windows show 12 or 14 or 18, but 16 dominates. Could be an artifact
  of how the late counter samples vs the 5 s stats interval.
- Is there a **second issue lurking** behind the playout-window dominant cause that will become
  visible once the lates drop? E.g. fragment-assembly stale-chain hazard at
  `receiver.rs::find_chain_for` (iterates `.rev()`, could match across a cross-frame splice).
  We have flagged this as a follow-up but not investigated rigorously.
- Could the **saturation probes themselves** be re-tuned (less aggressive, less frequent)
  instead of widening the playout? Trade-off: less probing = stale capacity estimates =
  bitrate adapter goes blind = different failure mode.

## File pointers (for anyone reading the source)

- Playout sizing & late classification: `crates/strata-bonding/src/receiver/aggregator.rs`
  (`ReassemblyBuffer::tick`, search for `late_packets`).
- Per-link sender: `crates/strata-bonding/src/net/transport.rs::TransportLink`.
- Bitrate adapter (with slew limit): `crates/strata-bonding/src/adaptation.rs`
  (`BitrateAdapter::slew_clamp`).
- Wire format: `crates/strata-transport/src/wire.rs`.
- Process-wide TimestampClock: `crates/strata-transport/src/pool.rs::PROCESS_EPOCH`.
- HLS upload to YouTube: `crates/strata-gst/src/hls_upload.rs`.

## What we want from the second opinion

1. Is the diagnosis (late_packets dominant, due to adaptive window sitting below tail OWD)
   convincing given the evidence, or is there a more likely explanation we have missed?
2. Of the four proposed paths, which is the right one for a live-YouTube-streaming use case
   where end-to-end latency is not a constraint?
3. Anything in the wire-protocol design itself (header layout, FEC integration, fragment
   reassembly, sequence numbering) that looks structurally fragile in a way that could
   cause occasional dropped references *independent* of the late-packets pathway?


GEMINI RESPONSE:

This is an excellent, highly rigorous field diagnosis. Your hypothesis that the adaptive playout window is aggressively chasing the average and getting clipped by tail OWDs is spot on, and the evidence points overwhelmingly to it.

Here is a thorough analysis of your diagnosis, answers to your open questions, and my recommendation on the best path forward.

1. Validating the Diagnosis: Why exactly 16 packets?
You noted that late_packets increments at a suspiciously uniform rate of ~16 per 5-second window, and you correctly suspected this might be a deterministic process (like your saturation probes) rather than random cellular HARQ retries.

If we look at the closed-loop late-pressure math in aggregator.rs, it perfectly explains the "16 packets" cluster:

The Hit: Every late packet adds LATE_HIT_MS = 6.0 to the playout window. 16 late packets × 6 ms = ~96 ms of rapid window expansion.
The Absorption: That ~96 ms bump is just enough to stretch the window wide enough to absorb the remainder of the probe-induced delay spike. The late packets stop.
The Drain: The buffer drains DRAIN_STEP_MS = 8.0 every 500 ms of stability (STABLE_DRAIN_MS). To drain the 96 ms of pressure, it takes exactly 6 seconds (96 / 8 * 0.5s).
The Cycle: If your saturation probes fire every ~8 seconds (as mentioned in your notes), the late pressure has just finished draining back to baseline right before the next probe hits. The window is tight again, the probe spikes the OWD, 16 packets are dropped, the window expands by 96ms, and the cycle repeats perfectly.
This confirms your suspicion: The late packets are entirely self-inflicted by the interaction between your periodic saturation probes and the receiver's AIMD late-pressure drain rate.

2. Evaluating the Paths Forward
Given that the use case is YouTube Live (where HLS ingest and YouTube's own processing add 10-30 seconds of buffering), optimizing for glass-to-glass sub-second latency is actively harming you.

Option 1 (Raise the floor - min_latency=1500): Recommended. This is the safest, most pragmatic approach. By pinning the floor to 1.5 seconds, you completely mask the 600-1500ms OWD spikes caused by your probes and cellular HARQ. You retain the ability for the buffer to expand to 3000ms if a catastrophic fade occurs, but you stop the bleeding instantly.
Option 2 (Fixed SRT-style latency): Also perfectly valid. Setting start = min = max = 2000ms turns off the adaptation loop entirely. This is easier to reason about and guarantees no late-packet drops unless the network is truly dead for 2+ seconds.
Option 3 (Smoothed EWMA decreases): Not recommended right now. You actually already built this (the AIMD slow-drain), but tuning it for the exact cadence of your probes across varying networks is a fragile game of whack-a-mole.
Option 4 (Instrument first): Unnecessary delay. Your logical deduction is already mathematically backed by your AIMD parameters and probe schedule. Instrumenting is great for long-term telemetry, but you know enough right now to fix the user's video artifacts.
3. Answering Your Open Questions
Q: Is there a second issue lurking (e.g., stale-chain hazard)? It's possible, but secondary. Right now, you are systematically throwing away ~3.2 valid packets every second. Because you use RLNC sliding-window FEC, dropping an already-recovered/received packet from the playout buffer blows a hole in the H265 reference frame chain. Fixing the playout window is a prerequisite to seeing if any actual decoder/reassembly bugs exist.

Q: Could the saturation probes themselves be re-tuned? Yes, but this is a sender-side architectural fix (Phase 1 from your claude_burst_fix_plan.md regarding BDP bounds). A 400ms saturation probe should not be inducing a 1500ms OWD spike unless the modem's internal buffer is bloated (bufferbloat). Long-term, you should implement the BDP-based queue cap (inflight ≤ k·(btl_bw × RTprop)) so your probes measure capacity without filling the modem's deep queue. However, raising the receiver's playout latency is a much faster, safer, and completely valid fix for the immediate HLS use case.

Advice on the Best Way Forward
1. Immediate Action: Go with Option 1 (or a hybrid of Option 2). Change the config defaults in ReassemblyConfig to:

start_latency: Duration::from_millis(2000)
min_latency_ms: 2000
max_latency_ms: 4000
This effectively gives you SRT-like stability. Because min == start, it will never drain below 2 seconds, completely absorbing your 8-second probe cadence and any HARQ retries.

2. Follow-up Action: Once the artifacts disappear and the user is happy, you can look at the sender-side BDP limits. If you cap the SO_SNDBUF and userspace queues relative to the measured RTT, your saturation probes will stop causing 1000ms+ latency spikes in the first place.

You have a rock-solid diagnosis. Pin the playout floor high, ship it, and let the YouTube buffer do its job! Let me know if you want to proceed with drafting the code changes for this.

CHATGPT RESPONSE:

I’m checking the specific receiver and transport code paths behind your diagnosis so I can validate the late-packet theory against the actual control loop and packet handling.

Searched for regex `late_packets|target_latency|LATE_HIT_MS|DRAIN_STEP_MS|STABLE_DRAIN_MS|find_chain_for|ProbeBw|probe` (`**/crates/strata-bonding/src/**`), 200 results

Searched for regex `PROCESS_EPOCH|TimestampClock|seq|fragment|checksum|header|wire` (`**/crates/strata-transport/src/**`), 200 results

Searched for regex `slew_clamp|BitrateAdapter` (`**/crates/strata-bonding/src/**`), 53 results

Read memory

I’ve confirmed the key anchors exist exactly where your write-up says; next I’m reading the receiver latency loop and the sender probe schedule to test whether the steady late rate is plausibly self-inflicted by control timing.

Read memory [](file:///home/vscode/.vscode-server-insiders/data/User/workspaceStorage/11560a5ac33081f51017b2e14f74c787/GitHub.copilot-chat/memory-tool/memories/repo/telemetry_conventions.md)

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#300-300), lines 300 to 520

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#780-780), lines 780 to 900

Read [](file:///workspaces/strata/crates/strata-bonding/src/config.rs#200-200), lines 200 to 270

Read [](file:///workspaces/strata/crates/strata-bonding/src/net/transport.rs#160-160), lines 160 to 240

Read [](file:///workspaces/strata/crates/strata-bonding/src/net/transport.rs#1320-1320), lines 1320 to 1370

The core loop already matches your narrative closely. I’m checking the exact defaults, probe staggering, and fragment-chain code to separate “dominant cause” from any independent structural risks.

Searched for regex `start_latency|min_latency_ms|max_latency_ms|ReassemblyConfig|ReceiverConfig` (`**/crates/strata-bonding/src/**`), 109 results

Searched for regex `saturation_probe_interval_s|set_saturation_probe_active|probe_allowed|inject_ppd_pair|ProbeBw|probe_active` (`**/crates/strata-bonding/src/**`), 54 results

Searched for regex `find_chain_for|cross-frame|splice|fragment chain|fragment` (`**/crates/strata-bonding/src/**`), no results

Searched for regex `struct PacketHeader|payload checksum|checksum|Fragment|keyframe|config` (`**/crates/strata-transport/src/wire.rs`), 86 results

There isn’t a `find_chain_for` symbol under `strata-bonding`; I’m locating the actual fragment reassembly site before judging whether that stale-chain concern is real or just a note from an older path.

Searched for text `find_chain_for` (`**/crates/**`), 3 results

Searched for regex `fragment.*chain|chain.*fragment|stale.*chain|splice` (`**/crates/**`), 12 results

Read [](file:///workspaces/strata/crates/strata-bonding/src/config.rs#129-129), lines 129 to 170

Read [](file:///workspaces/strata/crates/strata-bonding/src/scheduler/bonding.rs#470-470), lines 470 to 530

Read [](file:///workspaces/strata/crates/strata-bonding/src/scheduler/bonding.rs#660-660), lines 660 to 730

Read [](file:///workspaces/strata/crates/strata-transport/src/wire.rs#250-250), lines 250 to 370

I’ve confirmed the probe cadence is system-staggered off `interval / num_links`, which makes a deterministic late pattern plausible. I’m now reading the transport reassembly path to see whether any non-late structural hazard could independently punch holes in reference frames.

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#90-90), lines 90 to 140

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#1600-1600), lines 1600 to 1665

Read [](file:///workspaces/strata/crates/strata-transport/src/receiver.rs#90-90), lines 90 to 240

Read [](file:///workspaces/strata/crates/strata-transport/src/receiver.rs#520-520), lines 520 to 620

Read [](file:///workspaces/strata/crates/strata-transport/src/receiver.rs#1180-1180), lines 1180 to 1230

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/transport.rs#560-560), lines 560 to 680

I’m checking one implementation detail that affects your proposed fix paths directly: whether the high-latency floor and ceiling are actually user-configurable end-to-end, or only exist as hardcoded reassembly defaults right now.

Searched for regex `ReassemblyConfig \{|with_config\(|start_latency: cfg.receiver.start_latency|max_latency_ms: cfg.scheduler.max_latency_ms|min_latency_ms` (`**/crates/strata-bonding/src/**`), 70 results

Searched for regex `STRATA_.*LATENCY|playout|receiver.*latency|max_latency_ms|min_latency_ms` (`**/crates/strata-bonding/src/**`), 47 results

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/transport.rs#1-1), lines 1 to 120

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/mod.rs#1-1), lines 1 to 120

Read [](file:///workspaces/strata/crates/strata-bonding/src/runtime.rs#1-1), lines 1 to 220

One implementation wrinkle matters for the recommendation: the reassembly floor/ceiling live in `ReassemblyConfig`, and the current public config surface only exposes `start_latency` cleanly. I’m checking whether `scheduler.max_latency_ms` is actually tied to that path or just a misleading adjacent knob.

Searched for regex `scheduler\.max_latency_ms|max_latency_ms` (`**/crates/strata-bonding/src/**`), 23 results

Searched for regex `late_rate|delay_pressure|loss_after_fec|jitter_buffer_ms|ReceiverReportPacket` (`**/crates/strata-bonding/src/**`), 135 results

Read [](file:///workspaces/strata/crates/strata-bonding/src/scheduler/edpf.rs#120-120), lines 120 to 220

Read [](file:///workspaces/strata/crates/strata-bonding/src/adaptation.rs#150-150), lines 150 to 260

Read [](file:///workspaces/strata/crates/strata-bonding/src/adaptation.rs#480-480), lines 480 to 620

I’ve confirmed `late_rate` is only a secondary sender feedback signal, while the receiver still drops late packets locally. I’m checking the CLI/config entry points now, because some of the proposed “constant-only” options aren’t actually exposed as knobs today.

Read [](file:///workspaces/strata/crates/strata-bonding/src/adaptation.rs#700-700), lines 700 to 735

Read [](file:///workspaces/strata/crates/strata-bonding/src/config.rs#390-390), lines 390 to 430

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/transport.rs#360-360), lines 360 to 410

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#220-220), lines 220 to 300

Read [](file:///workspaces/strata/crates/strata-bonding/src/bin/strata_receiver.rs#1-1), lines 1 to 120

I’ve found a mismatch in the config surface: `scheduler.max_latency_ms` is not obviously the reassembly ceiling knob, despite comments that imply it is. I’m reading the receiver-report construction once more to make sure the sender is reacting to the intended latency signal.

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/transport.rs#520-520), lines 520 to 565

Read [](file:///workspaces/strata/crates/strata-bonding/src/net/transport.rs#1275-1275), lines 1275 to 1310

Read [](file:///workspaces/strata/crates/strata-bonding/src/config.rs#150-150), lines 150 to 166

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#520-520), lines 520 to 620

The periodicity story is getting sharper: with the current defaults and two live links, the scheduler starts one saturation probe every 5 seconds system-wide, which lines up almost perfectly with the “~16 late per 5s window” pattern. I’m verifying the fragment emission invariant before I call the transport reassembly path structurally safe.

Searched for regex `with_fragment\(|Fragment::Start|Fragment::Middle|Fragment::End|fragment payload|fragmented` (`**/crates/strata-transport/src/**`), 31 results

Searched for regex `Fragment::Start|Fragment::Middle|Fragment::End|fragment` (`**/crates/strata-bonding/src/**`), no results

Read [](file:///workspaces/strata/crates/strata-transport/src/receiver.rs#1-1), lines 1 to 90

Read [](file:///workspaces/strata/crates/strata-transport/src/sender.rs#1-1), lines 1 to 240

The fragment concern looks secondary so far: the transport sender emits fragment runs contiguously, which narrows the scope of the `find_chain_for` matching risk. I’m pulling the actual fragmenter implementation to confirm that invariant instead of inferring it from tests.

Searched for regex `fragment\(|fn fragment` (`**/crates/strata-transport/src/sender.rs`), 5 results

Read [](file:///workspaces/strata/crates/strata-transport/src/sender.rs#420-420), lines 420 to 480

Read [](file:///workspaces/strata/crates/strata-bonding/src/scheduler/bonding.rs#60-60), lines 60 to 90

I have enough to form the review. I’m just pinning exact source anchors now so the evaluation can point to the decisive lines rather than paraphrase them.

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#94-94), lines 94 to 125

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#332-332), lines 332 to 365

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#423-423), lines 423 to 485

Read [](file:///workspaces/strata/crates/strata-bonding/src/scheduler/bonding.rs#494-494), lines 494 to 505

Read [](file:///workspaces/strata/crates/strata-bonding/src/scheduler/bonding.rs#690-690), lines 690 to 705

Read [](file:///workspaces/strata/crates/strata-transport/src/receiver.rs#140-140), lines 140 to 205

I’ve got the main evaluation. One last useful check: there’s already test coverage for “disable probes by huge interval,” which gives you a much cheaper causality test than building new telemetry. I’m grabbing that anchor so I can suggest it precisely.

Read [](file:///workspaces/strata/crates/strata-bonding/src/config.rs#652-652), lines 652 to 667

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#808-808), lines 808 to 835

Read [](file:///workspaces/strata/crates/strata-bonding/src/adaptation.rs#712-712), lines 712 to 722

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#1614-1614), lines 1614 to 1655

**Findings**
- High: The diagnosis is convincing, but the regularity points more strongly to self-inflicted saturation probes than to random HARQ tails alone. With current repo defaults, saturation probes are configured at 10 s per link in config.rs, then staggered system-wide at interval divided by live-link count in bonding.rs. With two live links, that is one system probe about every 5 s, and each probe pins all traffic to one link in bonding.rs. That lines up almost too neatly with your 5 s stats windows and the steady late count.
- High: The attached Gemini explanation gets the current control-loop math wrong. Ordinary late packets add 3 ms of late pressure in aggregator.rs, not 6 ms. The 6 ms step in aggregator.rs is only the rare rewind-guard path during desync handling. So the “16 packets x 6 ms = 96 ms” proof should not be used as evidence, even though the overall conclusion is still directionally right.
- High: The user-visible failure path is exactly what your write-up says. Late packets are dropped on the receiver side, not treated as duplicates, as codified by aggregator.rs. The reassembly layer also excludes late packets from the loss metric in transport.rs, and sender-side late pressure only engages above 5% late rate in adaptation.rs. At the roughly 1-2% late ratios in your sample, the system has no existing mechanism that will self-correct this.
- Medium: Option 1 is the right operational fix, but it is not really a pure constants flip through the current public config surface. The exposed receiver config cleanly wires start_latency in config.rs, while reassembly max and min live in internal defaults in aggregator.rs. The current CLI path only passes a baseline latency in strata_receiver.rs and transport.rs. Also, the comment claiming the receiver widens “up to scheduler.max_latency_ms” in config.rs is misleading; that is not the actual reassembly ceiling path today.
- Low: I do not see a more likely independent wire-format or FEC bug than the late-packet pathway. The only notable fragility is fragment-chain lookup in receiver.rs, but sender fragmentation is emitted as contiguous runs in sender.rs, which makes overlapping-chain ambiguity unlikely in the current design. I would keep that as follow-up hardening, not the primary explanation for these artifacts.

**Evaluation**
From code inspection, your diagnosis is strong. I would sharpen it to: the dominant cause is a receiver playout deadline that sits below the real tail OWD, and the tail appears to be driven very predictably by the sender’s saturation-probe schedule, with cellular HARQ adding noise on top. The codebase even has a regression test describing this exact class of failure in aggregator.rs.

For a YouTube HLS ingest use case, option 1 is the right move now. The important knob is really start_latency, because target latency is computed as start_latency plus dynamic spread, loss, and late pressure in aggregator.rs. Once start_latency is high, min_latency_ms matters much less. So the pragmatic version is: raise start_latency substantially, raise the ceiling, and see whether late_packets collapses. I would not spend time on EWMA decay tuning first. I also would not build a full fixed-latency mode before validating the high-floor variant unless you specifically want the operator semantics.

The one thing I think is missing from your proposed paths is a very cheap discriminating A/B: effectively disable saturation and PPD probes for one run using the existing huge-interval sentinel already covered in config.rs. That is a faster causality test than building new telemetry, and it will tell you whether the “uniform 16” is mostly the probe clock.

1. Ship the high-floor playout change first for the YouTube path: start around 1500 ms, raise the ceiling to at least 3000 ms, and judge success by late_packets dropping toward zero.
2. If you want one confidence-building experiment before or after that, run a field test with saturation and PPD probes effectively disabled and compare the late rate.

I’m checking the exact delay-spread and release logic one more time so I can answer whether the current internals are conceptually right, or just operationally “good enough” for this case.

Searched for regex `delay_spread_us|rel_min_deque|rel_max_deque|loss_penalty_ms|stability_threshold_ms|jitter_samples|percentile` (`**/crates/strata-bonding/src/receiver/aggregator.rs`), 51 results

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/aggregator.rs#1-1), lines 1 to 90

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/transport.rs#720-720), lines 720 to 830

I have the release logic. I’m looking at the receiver-report payload now to see exactly which latency signals you export today and which important ones are missing.

Searched for regex `struct ReceiverReportPacket|jitter_buffer_ms|delay_gradient_us|late_rate|bytes_delivered` (`**/crates/strata-transport/src/wire.rs`), 41 results

Read [](file:///workspaces/strata/crates/strata-transport/src/wire.rs#720-720), lines 720 to 830

Read [](file:///workspaces/strata/crates/strata-transport/src/wire.rs#830-830), lines 830 to 900

The current wire feedback confirms the gap: you export current buffer depth, late rate, and a per-link delay gradient, but not the receiver’s actual cross-link delay-spread distribution that is driving the playout decision. I’m checking the delay-gradient tracker just to separate “queue build” from “playout deadline” signals cleanly.

Searched for regex `struct DelayGradientTracker|gradient_us|arrival_us - send_ts_us|windowed_min` (`**/crates/strata-bonding/src/receiver/transport.rs`), 12 results

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/transport.rs#200-200), lines 200 to 320

Read [](file:///workspaces/strata/crates/strata-bonding/src/receiver/transport.rs#320-320), lines 320 to 360

I have enough. The short version is: the high-floor, expand-upward model is right for this class of traffic, but the current controller semantics and telemetry are muddier than they should be.

Yes on the control shape, no on the current exact implementation.

For bonded-cellular broadcast, the right model is a fixed use-case baseline plus an adaptive margin that only expands upward and then decays back toward that baseline. You do not want a symmetric controller that keeps chasing calm periods back down toward the average. That is the wrong objective for this traffic class. But the current semantics are muddled: in aggregator.rs, start_latency is added into every target calculation, so start_latency is already the real baseline, while min_latency is mostly just a clamp from aggregator.rs. I would not keep thinking in terms of “min, start, max” long-term. I would make it explicit: base_latency, adaptive_margin, ceiling.

The stats are partly right, but not quite sufficient for a principled dynamic controller. The good part is that you moved from naive inter-arrival jitter to bonded delay spread. The relative delay spread logic in aggregator.rs and aggregator.rs is conceptually the right family of signal, because playout on bonded links is about cross-link skew and tail delay, not average jitter. The per-link delay_gradient_us signal in transport.rs and wire.rs is also good, but that is a sender congestion and queue-building signal, not the main receiver playout-sizing signal. Where I think things need changing inside is here:

- The current spread estimator is raw 4-second max-minus-min. That is safe, but too outlier-sensitive if you want elegant adaptation. I would rather size from a tail percentile of relative delay excess, such as a p99 excess over a rolling minimum, instead of a raw max.
- The current late feedback is too blunt. In aggregator.rs, every ordinary late packet adds the same fixed 3 ms, whether it missed by 5 ms or 700 ms. That is fine as an emergency trim, but not as the main estimator.
- Loss should be secondary for playout sizing. The loss component in aggregator.rs is understandable as a heuristic, but loss is mostly a sender/FEC/bitrate concern. Delay and skew should dominate deadline sizing.
- Your exported receiver feedback is missing the most important receiver-side playout telemetry: direct tail/skew statistics. Today you export current buffer depth, late rate, and delay gradient in wire.rs, but not the actual bonded spread or its percentile structure.

So if I were changing the internals, I would aim for this policy:

$$
target = base\_latency + adaptive\_margin
$$

with:

$$
adaptive\_margin = \max(\text{tail skew margin}, \text{queue margin}) + \text{emergency late trim}
$$

and the rules would be:

- Base latency is fixed by product mode.
- Adaptive margin can grow quickly.
- Adaptive margin decays slowly back to zero.
- The controller never tries to go below base latency in broadcast mode.

That means the “floor-only expand upward” idea is correct. The min/max part is fine as guardrails, but I would change the semantics inside so max is a true ceiling and min is replaced by an explicit base. If you keep the current internals and just raise constants, that is a valid product fix. If you want the algorithm to be clean and durable, then yes, things should change inside.

The single biggest telemetry addition I would make is this: track and export the distribution of relative delay excess and the magnitude of deadline misses, not just late counts. A late count tells you that you missed. It does not tell you whether you missed by 2 ms or 800 ms, and that difference matters a lot for controller design.

So the final answer is: the direction is right, but the current controller is still more of a pragmatic patchwork than a clean latency policy. It is not an Achilles’ heel forever unless you insist on one controller serving both low-latency interactive use and bonded-cellular broadcast. Split those modes, and this becomes much cleaner.

If you want, I can turn that into a concrete proposed redesign of the receiver policy and telemetry surface without changing any code yet.