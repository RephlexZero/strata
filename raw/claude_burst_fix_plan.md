# Strata Burst / Bufferbloat Fix Plan

A comprehensive plan to stop strata from inducing the loss it then fights —
**without over-fitting to cellular**. Covers (1) why the bonded feed konks out
when plain UDP wouldn't, (2) the isolation experiment that proves the cause and
*calibrates* the fix, (3) the versatility doctrine that governs every change,
(4) the phased architectural fix, (5) cross-regime verification.

---

## 1. Problem & reframe

This session fixed three real bugs (per-link source-IP blackhole; binary
link-death → continuous demotion; FEC actually reinserting). Links no longer
die, but the stream still fails health checks: **both modems show heavy bursty
loss** (`loss=1.000` spikes, 100–210 ms RTT, deep queues) even though a plain
even-paced UDP upload over the same SIMs is believed to be ~loss-free.

Reframe that drives everything below:

- **You cannot see the buffer that hurts you.** The bloated queue is a chain:
  socket `SO_SNDBUF` → kernel qdisc → USB driver aggregation (cdc_ncm/qmi_wwan
  NTB) → modem firmware TX ring → RLC/MAC at the eNB. The deep, variable
  500 ms–2 s lives in modem firmware + RAN; there is no general API for RLC
  occupancy. "Instrument the modem buffer" is a dead end as a primary strategy.
- Therefore the fix is **own the queue (move it local + instrumented) or never
  fill it (delay-bounded, BDP-bounded), and never probe destructively.**
- And because strata must serve fiber / Wi-Fi / satellite / lossy links too,
  every mechanism must be **path-relative and self-calibrating**, not a constant
  tuned on one SIM.

---

## 2. Versatility doctrine (cross-cutting — governs every change)

**Anti-pattern (the recurring bug this session):** absolute constants
trial-fitted to cellular — `LOSS_DEATH_HI=0.55`, drain at `5×RTprop`, `1.25×`
probe gain, `BLACKHOLE_STALE=3s`, `64 kbps` floor, hardcoded probe intervals.
Each is a landmine on fiber (1 ms RTT, loss is the signal, you *want* to fill the
pipe), satellite (600 ms RTT by design — a stall detector cripples it), or Wi-Fi
(aggregation bursts look like bufferbloat). The fix is never "find a better
constant" — it is "derive it from this link's measured baseline + variance."

**Principles every change must satisfy:**

1. **Path-relative, self-calibrating.** Thresholds expressed against the link's
   own measured stats (min-RTT/OWD, jitter, btl_bw, BDP) so the same expression
   is correct on a 5 ms fiber and a 600 ms satellite link with no branch.
2. **Per-link state, never globally coupled.** A fiber+cellular+satellite bond
   is legitimate; CC state, inflight bounds, queue policy are per-link.
3. **Signal fusion, not signal choice.** React to loss *or* delay *or* ECN —
   whichever fires first. Regime (loss-driven on fiber, delay-bounded on
   cellular) *emerges from measurement*; it is never configured.
4. **Good defaults from measurement + explicit escape hatch.** Auto-detection
   mis-fires (bloated Wi-Fi AP mimics cellular). Expose the *inferred* per-link
   regime/params in metrics; allow a per-link `profile` override (default
   `auto`).
5. **Enforcement gate:** every change is validated on a multi-regime
   `strata-sim` netem matrix (§6). A change that helps cellular but regresses
   the fiber/satellite profile is a caught over-fit. This is what *keeps* it
   versatile over time.

---

## 3. Phase 0 — Diagnose & calibrate (the isolation experiment)

Still fully relevant: it both proves which mechanism causes the loss *and*
measures the per-link path parameters the fix needs (bottleneck rate, buffer
depth). User opted for the **full ladder including saturating sweeps**.

### 3.1 Suspect mechanisms (verified)

| # | Mechanism | Location | Toggle today? |
|---|-----------|----------|---------------|
| 1 | Saturation probe pins ~100% to one link 0.4 s every (interval/num_links≈5 s) | `crates/strata-bonding/src/scheduler/bonding.rs:468-658`; defaults `config.rs:234-236`; hardcoded passthrough `config.rs:360-362` | NO (code change) |
| 2 | PPD probe: 2 pkts every 2 s/link | `bonding.rs:660-688` | NO (same fields) |
| 3 | BBR ProbeBw 1.25× UP-gain | `crates/strata-transport/src/congestion.rs:529-535` | indirect |
| 4 | `seed_bandwidth` ← modem-buffer-inflated probe rate | `congestion.rs:195-209`, called `bonding.rs:~420` | indirect |
| 5 | Token-bucket 10 ms burst cap + GSO idle-gap bursts | `crates/strata-bonding/src/net/transport.rs:280-359` (`flush_paced`, cap 317-318), `send_batch:361-485` | implicit |
| 6 | Failover broadcast: duplicate ALL pkts to ALL links 800 ms on RTT 3× | `bonding.rs:689-733`, `860-892` | YES — `STRATA_FAILOVER_ENABLED` |

### 3.2 Throwaway diagnostic harness (NEW, delete after)

`scripts/diag-udp-loss.sh` — header-marked "DIAGNOSTIC ONLY, safe to delete";
**no strata code in path**. Mirrors the proven pattern at
`scripts/field-test.sh:234-267`:

- Sender (per link): `src = ip -o -4 addr show dev <iface> | awk '{print $4}' | cut -d/ -f1`;
  `socket(AF_INET,DGRAM)`; `setsockopt(SOL_SOCKET, 25 /*SO_BINDTODEVICE*/, iface+"\0")`;
  `bind((src,0))`; **`connect((vps,port))`** (connected 4-tuple — unconnected
  /0.0.0.0 false-negatives, host default route is wlan0). 1200 B datagram = 8 B
  seq + 8 B send-ts-ns + filler.
  - `--mode even` — strictly paced at `--bitrate-kbps`.
  - `--mode burst` — `--baseline-kbps` + every `--burst-period`s a
    `--burst-secs` window at `--burst-rate-kbps` (defaults 5 / 0.4 / link-ceiling
    → mirrors suspect #1's duty cycle).
  - `--mode sweep` — **bufferbloat characterization**: step the rate up in
    stages, the VPS counter timestamps arrivals so we read the OWD knee → yields
    each link's true bottleneck rate **and buffer depth in ms**. This output
    directly calibrates the fix (CAKE shape rate, BDP cap).
  - `--pair iface:port iface:port` — both links concurrently.
- VPS counter: `scp` tiny `python3` to `/tmp/strata-diag-rx.py`, binds
  `0.0.0.0:<port>` under `timeout window+5`, prints final JSON
  `{link,sent,recv,max_seq,loss_pct,reordered,dups,max_gap_ms,owd_knee_ms,btl_kbps}`.
- All SSH/SCP over wlan0 (`-o BindInterface=wlan0`, reuse `SSH_OPTS`
  `field-test.sh:99-108`). `trap EXIT`: `pkill -f strata-diag-rx; rm -f /tmp/strata-diag-rx.*`.
  No ufw change (5000:5004/udp open).

### 3.3 Probe-config toggle (small, backward-compatible, **keep**)

Mirror the `STRATA_FAILOVER_*` plumbing exactly:

- `crates/strata-bonding/src/config.rs`: add `Option<f64>`
  `saturation_probe_interval_s` / `_duration_s` / `ppd_probe_interval_s` to
  `SchedulerConfigInput` (after line 99; struct already `#[serde(default)]`).
  In `resolve()` replace hardcoded `config.rs:360-362` with
  `self.x.unwrap_or(defaults.x)` (pattern of `failover_duration_ms` at 317-320);
  `.max(0.01)` on interval (div-by-zero guard at `bonding.rs:481`).
- `scripts/field-test.sh`: env defaults near line 89 (empty = omit key);
  conditionally emit into `[scheduler]` heredoc near 325-330; document near
  22-25. **Disable sentinel:** interval = `1000000000` (probe never fires; no
  scheduler-logic edit). No `bonding.rs` change — it already reads
  `config().saturation_probe_interval_s` etc. Requires sender rebuild, not
  receiver redeploy.

### 3.4 Isolation ladder & decision tree

All 120 s; per-link loss from VPS sequence counter / strata receiver per-link
raw loss (`RX links: loss_link_N=` — *not* FEC-recovered, FEC masks konk-outs).
Bands: **Clean <0.5 %**, **Mild 0.5–3 %**, **Heavy >3 % or any ≥1 s gap**.

| Tier | What | Build |
|------|------|-------|
| T0 | plain `even` single link/modem @1200 kbps **and** @~8 Mbps | none |
| T0b | plain `even` both links concurrently @600 ea **and** saturating | none |
| T0c | plain `burst` mimic (0.4 s @ceiling every 5 s + baseline) | none |
| T0s | plain `sweep` per modem → btl rate + buffer-depth ms (calibration) | none |
| T1 | full strata defaults (baseline; existing heavy-loss data) | — |
| T2 | strata, `STRATA_FAILOVER_ENABLED=false STRATA_REDUNDANCY_ENABLED=false STRATA_CRITICAL_BROADCAST=false` | env |
| T3 | strata, probes neutralized (sentinel `…PROBE_INTERVAL_S=1e9`) | §3.3 + rebuild |
| T4 | strata, single link only (`STRATA_RECEIVER_PORTS`/`_LINK_IFACES` 1 entry) | env |

Decision tree:

- **T0 Heavy** ⇒ carrier/RF intrinsic (hypothesis falsified); still run rest to
  quantify added strata loss.
- **T0 Clean, T0c Heavy** ⇒ *decisive*: burst **pattern alone** reproduces
  konk-outs ⇒ blame #1 (saturation probe) and/or #5 (token/GSO idle-gap).
- **T0/T0b/T0c Clean, T1 Heavy** ⇒ strata-specific interaction; localize below.
- **T2 fixes it** ⇒ failover broadcast #6.
- **T3 fixes it (T2 didn't)** ⇒ saturation/PPD probe #1/#2 (+ #3/#4 downstream).
- **T4 fixes it (T3 didn't)** ⇒ multi-link scheduler/rebalancing (only ≥2 links).
- **Nothing below T1 fixes it** ⇒ token/GSO idle-gap burst #5 / BBR pacing.

`T0s` output (per-link btl rate + buffer depth) feeds Phase 1+ regardless of
which mechanism is implicated.

---

## 4. Phase 1+ — The fix (path-relative, per the doctrine)

Each item is the *versatile* form of a cellular idea — expressed relative to the
path so it is correct on any link. Sequence cheapest/safest first; each gated on
the §6 sim matrix.

### F1. Delay-bounded probe — keep capacity discovery, kill the bloat (after F3)
**Do NOT go purely passive.** Passive measurement under partial DWRR load is a
decay spiral: route 2 Mbps to a 10 Mbps link → passive `btl_bw` decays to
2 Mbps → DWRR routes even less → death spiral. (This is precisely why the
`CapacityOracle` built earlier this session exists: two bounds, peak-with-decay,
40%-of-peak floor. Keep that model.) Also reject the naive "pin 100 % but
enforce the F2 BDP cap during the probe": the cap is `k·(btl_bw·min_rtt)` and
during decay `btl_bw` *is* the depressed value, so it throttles the probe to the
stale rate and never discovers the ceiling.

Correct mechanism: replace the destructive **fixed-rate, duration-bounded**
saturation probe with a **delay-bounded ramp**. Route real traffic share to the
target link and *increase* send rate/inflight until the F3 normalized
OWD-gradient knee, record that inflection as the capacity ceiling, then retreat.
Discovers true capacity (escapes the partial-load trap) *and* never bloats
(stops at the knee, not after dumping a fixed burst). Keep PPD (2 pkts).
**Depends on F3** (needs the delay signal) → sequenced after it.
Files: `bonding.rs:468-688` (probe drivers → ramp controller),
`congestion.rs:195-209` (`seed_bandwidth` fed from receiver rate, not sender
`observed_bytes`); oracle two-bound model retained as-is.

### F2. Per-link BDP-relative inflight + queue cap (scale-free guardrail)
Re-introduce the cap EDPF removed, as a **ratio**:
`inflight ≤ k·(btl_bw × RTprop)`, `k≈1.25`. Auto-scales: satellite→huge,
fiber→large, cellular→modest — no constant.

**Critical:** `RTprop` MUST be a windowed-min propagation delay (≈10 s sliding
minimum), never a recent RTT. If sampled while the buffer is already bloated
(e.g. 500 ms), the cap goes astronomical and never forces a drain. This depends
on Biscay's existing **ProbeRtt** phase (already drains to 0.5× and resets
`rt_prop`) actually firing periodically to re-sample true propagation delay —
leverage it, do not add a parallel drain. If F1's ramp probe ever suppresses
ProbeRtt, keep a periodic forced drain so RTprop stays honest.

Also apply the same BDP bound to the **userspace `paced_queue`** (it is already
bounded + priority/keyframe-aware oldest-drop): this is what makes F4
unnecessary (see below). Files:
`crates/strata-bonding/src/scheduler/edpf.rs` (`predicted_arrival` /
`capacity_bytes_per_sec`), enforced in `transport.rs` `flush_paced` + the
`paced_queue` bound; `congestion.rs` ProbeRtt is the RTprop source.

### F3. Per-link delay-GRADIENT signal, drift-immune (signal fusion)
**Do not use absolute OWD** — it needs PTP-synced clocks; sender/VPS skew + drift
permanently corrupts a lifetime `min_OWD`. Use the **relative delay gradient**:
the receiver compares inter-packet *arrival* gap vs the sender's *departure* gap
(`TimestampClock` already carries send timestamps; reuse the receiver-report /
`rx link heartbeat` path). Constant clock offset cancels in the gap difference;
only drift *rate* remains and is negligible at packet timescales. Track the
baseline as a **~10 s sliding-window minimum** (mirrors Biscay
`rt_prop_stamp`/`rt_prop_expiry`) so any residual skew expires out of the
baseline exactly like BBR's windowed min_rtt. "Queue building" = gradient
sustained positive beyond `k · gradient_jitter` for *that* link. Feed into
Biscay alongside loss and (if present) ECN — whichever fires first drives
backoff. Replaces the global RTT-3× all-links reaction and the lax `5×RTprop`
drain (`congestion.rs:~381`). Demote the *specific* loading link, not all.
This signal is also the knee detector F1's ramp probe depends on.

### F4. Userspace AQM — NOT tc/Netlink (collapsed into F2)
~~Per-interface `cake`/`fq_codel` with a btl_bw-driven shaper.~~ **Rejected:**
re-running `tc qdisc change` at btl_bw cadence is heavy, racy, and needs
CAP_NET_ADMIN on top of the CAP_NET_RAW we already require. The queue strata
must control already lives in userspace — the bounded, priority/keyframe-aware
`paced_queue`. Make *it* the AQM:
- BDP-bound the `paced_queue` (the F2 cap, applied to the queue) — drop oldest
  low-priority first (existing behavior), keyframe-protected.
- Shrink `SO_SNDBUF` toward BDP with a hard floor of **one GSO superpacket**
  (kernel doubles the set value and enforces `wmem_min`; never starve a batch).
  This turns silent kernel absorption into explicit `EAGAIN` backpressure that
  the userspace pacer/AQM acts on.
Zero Netlink, zero extra privilege, regime-agnostic, transparent on fiber (huge
BDP ⇒ huge bound ⇒ never drops). `tc -s qdisc` / `ss -mi` remain *read-only*
diagnostics only. Net effect: F4 is no longer a separate item — it is F2 applied
to the send queue.

### F5. Opportunistic modem flow-control (no new dependency required)
Where modems expose QMI/MBIM **QMAP DFC** (Qualcomm/rmnet) or vendor AT stats,
feed "slow down" events into the existing modem-supervisor seam
(`on_radio_metrics()` → Biscay). Strictly additive; absence changes nothing.

### F6. Constants → path-relative expressions + observability
Systematically replace the trial-fitted constants (drain threshold, probe gain,
token burst cap, any residual death/stale timers) with expressions normalized to
the per-link measured baseline/variance. Add a per-link `profile`
(`auto`|`cellular`|`fiber`|`satellite`|`wifi`|`lossy`, default `auto`) override
in `config.rs`. Emit the *inferred* per-link regime + chosen params in metrics
so mis-detection is visible.

---

## 5. Critical files

- `scripts/diag-udp-loss.sh` (NEW, throwaway)
- `crates/strata-bonding/src/config.rs` — `SchedulerConfigInput` + `resolve()`; per-link `profile`
- `scripts/field-test.sh` — probe-toggle env plumbing
- `crates/strata-bonding/src/scheduler/bonding.rs` — probe drivers (F1)
- `crates/strata-bonding/src/scheduler/edpf.rs` — BDP-relative cap (F2)
- `crates/strata-bonding/src/net/transport.rs` — `flush_paced`/`send_batch`, `paced_queue` BDP bound + `SO_SNDBUF` floor (F2/ex-F4), delay-gradient plumbing
- `crates/strata-transport/src/congestion.rs` — Biscay signal fusion, windowed-min RTprop / ProbeRtt as RTprop source, delay-bounded probe seed
- `crates/strata-sim/tests/tier3_netem.rs` (+ sibling profiles) — the enforcement matrix

---

## 6. Verification — cross-regime matrix is the gate

**`strata-sim` netem profile matrix (CI gate for every F-item):**

| Profile | netem | Pass criterion |
|---------|-------|----------------|
| fiber | 1 Gbps, 1 ms, 0 loss | ≥95 % link utilization, no added latency |
| cellular | variable BW, deep buffer, bursty loss | no konk-outs; standing queue bounded |
| satellite | 10 Mbps, 600 ms, 0 loss | fills BDP; not mis-flagged as stalled |
| wifi | aggregation + contention + jitter | bursts absorbed, no oscillation |
| lossy | shallow buffer, 1–2 % random loss | loss-driven, high utilization |

A change that improves `cellular` but regresses `fiber`/`satellite` is rejected.

**Field validation (after sim passes):** re-run the Phase 0 ladder; expect T1
(full strata, post-fix) to reach Clean/Mild on the real SIMs at the `T0s`-measured
sustainable rate. Standard build gate: `cargo build/test/clippy -p strata-bonding`,
`bash -n scripts/*.sh`.

**Observability:** per-link inferred regime, btl_bw, min_OWD, OWD-jitter, chosen
inflight cap, AQM backlog — all in metrics, so the system explains its own
decisions in production.

---

## 7. Sequencing & disposition

1. **Phase 0 ladder + `T0s` calibration** — no architectural risk; produces the
   buffer-depth/btl numbers that set `k`, confirms the mechanism.
2. **F2 (BDP-relative inflight + queue cap) + F6 (constants→expressions,
   observability)** — purely local to the sender, scale-free, instantly stops
   the sender out-pacing the network; subsumes F4. RTprop must be windowed-min
   (depends on ProbeRtt firing).
3. **F3 (relative delay-gradient signal fusion)** — the core controller change;
   drift-immune; sim matrix critical. Also provides F1's knee detector.
4. **F1 (delay-bounded ramp probe)** — *reordered after F3* (it needs the
   gradient knee). Replaces the destructive probe; oracle two-bound model kept.
5. **F5 (opportunistic modem flow-control)** — additive hardening; absence is
   a no-op.

(F4 removed as a standalone step — folded into F2.)

**Disposition:** diag harness = throwaway (delete post-investigation).
Probe-config toggle, BDP-relative cap, OWD signal, `profile` override =
**permanent** (genuinely useful, backward-compatible, and the doctrine §2 is the
standing rule for all future congestion/scheduler work).
