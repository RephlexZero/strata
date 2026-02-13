# Codebase Coherence Issue Report

> **Date**: 2026-02-13
> **Scope**: Full-project audit — architecture, config, data flow, receiver, GStreamer layer
> **Goal**: Identify anything undermining throughput maximisation and reliability across bonded unreliable links

---

## Round 1 (8 issues — all fixed)

### 1. Dead Config Fields

**Severity**: Low | **Status**: Fixed

Removed 4 dead fields (`congestion_headroom_ratio`, `congestion_trigger_ratio`,
`rtt_headroom_ratio`, `nada_logwin_ms`) from `SchedulerConfigInput`,
`SchedulerConfig`, defaults, `resolve()`, tests, and wiki docs.

### 2. SBD Module Built but Never Wired

**Severity**: Medium | **Status**: Fixed

Wired `SbdEngine` into `Dwrr::refresh_metrics()` — instantiation, link
add/remove, OWD and loss feeding after per-link loop, interval-based
`process_interval()` + `compute_groups()`.

### 3. `max_capacity_bps` Stale in Stats Thread

**Severity**: Medium | **Status**: Fixed

Changed `scheduler_config` to `Arc<Mutex<SchedulerConfig>>` in sink.rs.
Stats thread reads `max_capacity_bps` from the shared handle each iteration.

### 4. Redundant Sender-Side Throughput Tracking

**Severity**: Low | **Status**: Fixed

Removed link-level `observed_bps` delta computation and associated
`observed_prev_bytes` / `observed_prev_ts_ms` from `LinkStats`.

### 5. Receiver Config Not Plumbed

**Severity**: Low | **Status**: Fixed

Plumbed `jitter_latency_multiplier` and `max_latency_ms` from TOML config
through `Settings` to `ReassemblyConfig` in src.rs.

### 6. Jitter Thread 1-Packet-Per-Tick

**Severity**: Medium | **Status**: Fixed

Added `while let Ok(pkt) = input_rx.try_recv()` drain loop after
`recv_timeout()` in the jitter thread.

### 7. Receiver Stats Schema Mismatch

**Severity**: Low | **Status**: Fixed

Bumped receiver stats to `schema_version: 3`, added `duplicate_packets` field.

### 8. `estimated_capacity_bps` Not Reset on Link Revival

**Severity**: Low | **Status**: Fixed

On `phase_reset`, reset `estimated_capacity_bps` to `capacity_floor`,
clear `x_prev`, `gradual_mode`, `loss_free_samples`.

---

## Round 2 (10 issues — all fixed)

### 9. OWD Always 0 → SBD Delay Tracking Dead

**Severity**: Critical | **Status**: Fixed

`link.rs` returned `owd_ms: 0.0`, making the SBD engine's delay guard
(`if owd_ms > 0.0`) always false. Fixed by using `rtt_ms / 2.0` as a proxy
for one-way delay. Also wired `BondingHeader::with_timestamp()` into all
three `BondingScheduler::send()` code paths via a `make_header()` helper.

### 10. `capacity_floor_bps` Default Mismatch

**Severity**: Medium | **Status**: Fixed

Wiki documented default as 5 Mbps; code default is 1 Mbps. Updated wiki
to match code (`1000000`).

### 11. ~30 AIMD/NADA/SBD Config Fields Undocumented

**Severity**: Medium | **Status**: Fixed

Added three new wiki sections: "AIMD Capacity Estimation", "NADA Congestion
Control (RFC 8698)", and "Shared Bottleneck Detection (RFC 8382)" with full
parameter tables.

### 12. Wiki Lifecycle Diagram Incorrect Transitions

**Severity**: Medium | **Status**: Fixed

Removed two transitions that don't exist in code:
- `Degrade → Reset: stale stats` (code goes Degrade → Cooldown)
- `Cooldown → Reset: cooldown timer expires` (code goes Cooldown → Probe)

### 13. Receiver Stats Thread Hardcoded 1s Interval

**Severity**: Medium | **Status**: Fixed

Added `stats_interval_ms` to receiver `Settings`, plumbed from TOML config.

### 14. Sender Stats Interval Not Live-Updatable

**Severity**: Low | **Status**: Fixed

Stats thread now reads `stats_interval_ms` from `Arc<Mutex<SchedulerConfig>>`
each iteration.

### 15. `queue_depth` / `max_queue` Placeholder

**Severity**: Low | **Status**: Documented

Added TODO comment explaining librist does not expose per-peer send-queue depth.

### 16. `BondingHeader::with_timestamp()` Dead Code

**Severity**: Low | **Status**: Fixed (part of Issue #9)

### 17. `aggregate_nada_ref_bps` Conflates AIMD and Raw Capacity

**Severity**: Low | **Status**: Fixed

`total_capacity` now sums raw `capacity_bps`; `aggregate_nada_ref_bps`
separately sums `estimated_capacity_bps` with raw fallback per-link.

### 18. Cooldown Timer Off-by-One Tick

**Severity**: Low | **Status**: Fixed

`last_transition` set on entry into Cooldown/Reset via `old_phase` comparison.

---

## Round 3 (3 issues — all fixed)

### 19. SBD Groups Computed but Not Used by Coupled Alpha

**Severity**: Medium | **Status**: Fixed

Coupled AI (RFC 6356 §3) now partitions links by SBD group membership.
Per-group alpha is computed and applied to each link. Links in group 0 (no
shared bottleneck) get alpha = 1.0.

### 20. `total_dead_drops` Dead Observable

**Severity**: Low | **Status**: Fixed

Simplified from `Arc<AtomicU64>` to plain `u64` (only used for log messages).
Added `total_dead_drops` field to `StatsSnapshot` for future observability.

### 21. Receiver Stats Interval Not Live-Updatable

**Severity**: Low | **Status**: Fixed

Refactored `RsRistBondSrc.settings` from `Mutex<Settings>` to `Arc<Mutex<Settings>>`.
Stats thread now holds an `Arc::clone` of the settings handle and re-reads
`stats_interval_ms` each tick, matching the sender pattern (Issue #14).

---

## Final Status

| # | Issue | Severity | Status |
|---|-------|----------|--------|
| 1 | Dead config fields | Low | **Fixed** |
| 2 | SBD unwired | Medium | **Fixed** |
| 3 | Stale `max_capacity_bps` | Medium | **Fixed** |
| 4 | Redundant throughput tracking | Low | **Fixed** |
| 5 | Receiver config not plumbed | Low | **Fixed** |
| 6 | Jitter thread 1-pkt-per-tick | Medium | **Fixed** |
| 7 | Receiver stats schema mismatch | Low | **Fixed** |
| 8 | Capacity not reset on revival | Low | **Fixed** |
| 9 | OWD always 0 / SBD dead | Critical | **Fixed** |
| 10 | `capacity_floor_bps` wiki mismatch | Medium | **Fixed** |
| 11 | Config fields undocumented | Medium | **Fixed** |
| 12 | Wiki lifecycle diagram wrong | Medium | **Fixed** |
| 13 | Receiver stats interval hardcoded | Medium | **Fixed** |
| 14 | Sender stats interval not live | Low | **Fixed** |
| 15 | `queue_depth` placeholder | Low | **Documented** |
| 16 | `with_timestamp()` dead code | Low | **Fixed** |
| 17 | NADA vs raw capacity conflation | Low | **Fixed** |
| 18 | Cooldown timer off-by-one | Low | **Fixed** |
| 19 | SBD groups unused by coupled alpha | Medium | **Fixed** |
| 20 | `total_dead_drops` dead observable | Low | **Fixed** |
| 21 | Receiver stats interval not live | Low | **Fixed** |

All 21 issues resolved. 179 tests pass (164 core lib + 4 network-sim + 11 GStreamer plugin).
