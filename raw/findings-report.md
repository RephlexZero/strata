# Findings Report: 2026-05-21 Field Run

## Executive Summary

- Failover was disabled for this run and did not trigger.
- The stream damage was real. This was not a clean run with a misleading YouTube player. The transport and receiver entered burst loss / queue collapse, and the receiver logged an MPEG-TS continuity mismatch before the large loss window fully surfaced.
- The reassuring `loss=0.000` readings are not trustworthy as a top-level health signal:
  - receiver `loss_rate` is a smoothed EWMA, not cumulative damage;
  - sender `[link] loss=` is a sender-side transport retransmission ratio for that window, not receiver-visible media damage.
- The current control loop reduces bitrate during collapse, but the routing layer still treats both links as alive and eligible. That means the system can be in `ReduceBitrate` while still balancing traffic over the path that is hurting the receiver.
- The field-test monitor already prints enough receiver-side evidence to catch this, but the most visually prominent numbers are the wrong ones, so the run looks healthier than it is.

## What Happened

### 1. Failover was not involved

- The generated sender config in `latest_run.log` shows `failover_enabled = false`.
- Sender `send path trace` lines repeatedly show `failover=false` throughout the run.
- This was a normal dynamic balancing run, not a broadcast failover episode.

### 2. The receiver showed trouble early

- The receiver reached very high latency almost immediately. Early `strata-stats` messages show `current_latency_ms` and `target_latency_ms` rising toward 3000 ms while FEC recoveries were already accumulating.
- At about 10 seconds into the receiver log, `tsdemux` reported:

```text
CONTINUITY: Mismatch packet 6, stream 7 (pid 0x0041)
```

- That continuity warning is the earliest clear media-layer damage signal in the logs.

### 3. The first major collapse window was visible in the monitor

At the 5 second field-test snapshot:

- `queue_depth=1341`
- `late_packets=20`
- receiver per-link diagnostic loss was already elevated:
  - `loss_link_0=0.0439`
  - `loss_link_1=0.0580`
- the sender simultaneously reported:
  - link 0: `loss=0.758`, `queue=269`
  - link 1: `loss=0.000`, `queue=16`

At the 10 second field-test snapshot:

- `Δ5s: delivered=2849 lost=325 late=16 win_loss=10.2%`
- receiver global smoothed `loss_rate=0.139`
- adaptation reported `loss_fec=0.342`
- target bitrate had already been cut to `810 kbps`

The final quality gate correctly summarized the run as degraded:

```text
PARTIAL: Segments produced but quality degraded (worst_loss_fec=0.342 max_window_loss=10.2% max_delta_late=20 unhealthy_windows=1)
```

### 4. Later "loss looks fine" windows were deceptive

After the collapse, many later monitor windows showed near-zero receiver `loss_rate` or sender `[link] loss=0.000`, but:

- cumulative `lost_packets` stayed elevated;
- cumulative `late_packets` stayed elevated;
- FEC recoveries kept climbing;
- the media pipeline had already logged a continuity mismatch.

So the transport could look calm again while the media chain was still paying for earlier damage.

## Why The Logs Misled Us

### Receiver `loss_rate` is explicitly smoothed

The receiver metric exported in `metrics.rs` is labeled:

```text
Smoothed loss rate (0-1)
```

And the reassembly buffer updates it as:

```text
loss_rate_smoothed = 0.95 * previous + 0.05 * instant_loss
```

Implication:

- this number decays after a burst;
- it is not a cumulative run-health metric;
- it must be read together with `lost_packets`, `late_packets`, queue depth, continuity warnings, and FEC activity.

### Sender `[link] loss=` is a different metric entirely

The sender transport computes per-interval `loss_rate` from retransmissions divided by total on-the-wire traffic in that interval. That is useful for congestion control, but it is not the same thing as receiver-visible post-FEC loss or media corruption.

Implication:

- a sender link can show `loss=0.000` after a burst window closes;
- the receiver can still be recovering from queued, late, or already-lost media;
- the operator sees "zero loss" even though the stream is visibly damaged.

### The sender and receiver are telling different truths

The strongest example is the 5 to 10 second window:

- the receiver shows queue growth, late arrivals, and then a 10.2% loss window;
- the sender later shows both links with `loss=0.000` and `queue=0`;
- the adaptation layer still sees high `loss_fec` and cuts bitrate.

This is not a contradiction. It means the observability surfaces are describing different layers and different time windows.

## What The Code Says About The Control Path

### Receiver feedback sent to the adapter is global, not per-link routing truth

Receiver report generation takes the worse of:

- transport-layer residual loss after FEC;
- reassembly-layer loss.

That is the right global control signal for bitrate adaptation, but it is not a per-link routing penalty.

Per-link diagnostic stats remain transport-local diagnostics for each link.

### `link_collapse` changes bitrate pressure, not routing eligibility

The adapter flags a collapse when any alive link has roughly:

- `loss_rate >= 0.55`
- `queue_depth >= 60`

But that signal is used to:

- suppress recovery / ramp-up behavior;
- add pressure to bitrate reduction.

It does not directly suppress or remove the collapsing link from routing.

### The transport layer uses continuous liveness on purpose

`TransportLink::get_metrics()` explicitly documents that a configured, OS-up link is always treated as alive. The model is:

- never binary-kill the link on ordinary cellular bursts;
- continuously demote it by crushing capacity if delivery goes stale;
- keep it in the bond so probe traffic can re-admit it automatically.

In code, `alive = true` unless the OS says otherwise.

The only hard demotion in this path is when the link becomes delivery-starved for about 3 seconds after meaningful send volume. Then its capacity is pinned to a probe floor, but it is still not removed from the bond.

I did not find evidence in this run that either link hit that delivery-starved fallback. The sender kept reporting two alive links throughout the collapse window.

### EDPF discounts by sender-side loss and delay, not receiver-side residual loss

EDPF capacity is roughly:

```text
capacity_bps * (1 - transport_loss) * queue_penalty * jitter_penalty
```

Important details:

- `loss` is the transport-side link loss metric from the sender;
- receiver-side jitter buffer depth is used as a penalty;
- receiver-side residual loss / late rate do not directly enter EDPF routing.

That means routing can remain optimistic about a path once sender-side transport loss calms down, even while receiver-visible damage is still happening or has just happened.

### Net effect

The code supports exactly what the logs showed:

- the adapter can enter `ReduceBitrate` because the receiver is hurting;
- at the same time, the scheduler can still route across two `alive` links;
- failover stays off;
- the bad path is not explicitly suppressed, only continuously discounted.

## Why YouTube Showed Grey / Artifacting

The evidence points upstream of HLS upload:

- the receiver logged an MPEG-TS continuity mismatch;
- the receiver accumulated real lost and late packets;
- post-FEC residual loss spiked to `0.342`;
- HLS upload itself completed normally: `HLS uploader: stopped (70 segments uploaded)`.

Most likely sequence:

1. burst degradation caused queued / late / lost transport units;
2. some damage escaped FEC / reassembly;
3. MPEG-TS continuity broke;
4. H.265 decode artifacts propagated into uploaded segments;
5. YouTube displayed the resulting greying / corruption.

So the visible corruption was created before HLS upload, not by the uploader.

## Observability Gaps

### 1. The right receiver signals already exist, but they are not prominent enough

`scripts/field-test.sh` already prints:

- `RX links: ...`
- `Δ5s: delivered=... lost=... late=... win_loss=...`
- final degraded-run quality summary

This is useful data. The main problem is presentation:

- the eye is drawn to `loss_rate=` and sender `[link] loss=`;
- neither of those is the primary health signal for visible stream quality.

### 2. The standalone receiver log is too thin

The periodic `strata_receiver` log currently prints only:

- packets
- bytes
- queue depth
- lost
- late
- duplicates

It does not surface:

- `current_latency_ms`
- `target_latency_ms`
- that `loss_rate` is smoothed
- per-link diagnostic loss
- FEC recoveries / corrupt drops
- continuity / discontinuity counts

That is a real logging gap.

### 3. There is no single explicit line tying routing state to receiver damage

What is missing operationally is a summary line like:

```text
receiver_damaged=true adapter_reduce=true scheduler_alive_links=2 collapsing_link=1
```

Without that, the operator has to mentally reconcile three different layers.

### 4. We do not retain the first damaged media artifacts

This run no longer had the HLS directory available, and `ffprobe` is not installed in this environment, so I could not do segment-level forensic analysis after the fact.

That is a tooling gap, not just a one-off inconvenience.

## Root-Cause Assessment

### Primary cause

Burst degradation on one or both links caused queue growth, late arrivals, and residual loss that escaped FEC / reassembly and damaged the MPEG-TS stream.

### Secondary control weakness

The routing plane does not directly suppress a receiver-collapsing path. It relies on continuous sender-side loss and delay discounting plus a slow delivery-starved fallback. That is too indirect for the 1 to 5 second collapse windows seen here.

### Tertiary operational weakness

Telemetry presentation makes the run look cleaner than it is by foregrounding smoothed or sender-local metrics instead of receiver damage indicators.

## Recommended Next Changes

### Priority 1: Fix the operator-facing monitor first

1. Rename surfaced receiver `loss_rate` to `smoothed_loss_rate` everywhere it is shown.
2. In the field-test monitor, print these first:
   - `Δ5s lost / late / win_loss`
   - `current_latency_ms / target_latency_ms`
   - per-link receiver diagnostic loss
3. Add a one-line `DAMAGE` banner whenever any of these trip:
   - continuity mismatch seen
   - `loss_fec >= 0.05`
   - `win_loss >= 1%`
   - `delta_late > 0`

### Priority 2: Expand receiver periodic logging

Add to `strata_receiver` periodic logs:

- `current_latency_ms`
- `target_latency_ms`
- `smoothed_loss_rate`
- per-link diagnostic loss
- cumulative FEC recoveries and corrupt drops
- continuity / discontinuity counters

### Priority 3: Feed receiver-side collapse into routing, not just bitrate adaptation

The current 3 second delivery-starved demotion is too slow for these bursts.

Likely options:

1. Add a temporary per-link routing penalty / trickle floor when receiver-side per-link diagnostics and queue pressure indicate collapse.
2. Feed receiver-derived badness into EDPF candidate scoring, not only into the bitrate adapter.
3. Add a short-lived "avoid this link" window that is lighter than binary failover but stronger than today’s continuous discounting.

### Priority 4: Preserve artifacts automatically on the first damage signal

On first unhealthy window or first continuity warning:

1. preserve the current HLS directory;
2. save the first N `.ts` segments and playlist;
3. run `ffprobe` continuity / timestamp dumps automatically;
4. include those artifacts in the field-test bundle.

## What I Could Not Prove From This Environment

- I could not inspect the exact damaged `.ts` segments from this run because the HLS directory no longer existed.
- I could not do `ffprobe` segment analysis in this dev container because `ffprobe` is not installed.

## Bottom Line

The run broke because the bonded transport briefly collapsed hard enough to damage MPEG-TS continuity and media delivery. The system did reduce bitrate, but it did not clearly route away from the bad path fast enough, and the logs made the run look much cleaner than it was. The immediate next step should be to fix the monitor and receiver logging so the real damage signals are impossible to miss, then tighten routing behavior so receiver-side collapse influences path selection directly.