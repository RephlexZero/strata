# Test Buildout Plan

Prioritised list of tests that address real gaps — untested conversion logic,
unsafe code paths, error handling, and edge cases that could silently break.

Tests are grouped by module against a "would this catch a real bug?" bar.

---

## 1. `stats.rs` — `LinkStatsSnapshot::from_metrics()` (0 tests → 5)

**Why:** This is the sole conversion layer between internal `LinkMetrics` and
the JSON telemetry emitted on the GStreamer bus. A field-mapping typo here
silently breaks monitoring dashboards. Zero tests today.

| Test | What it catches |
|------|----------------|
| `from_metrics_maps_all_fields` | Every field round-trips from `LinkMetrics` → `LinkStatsSnapshot` |
| `from_metrics_phase_string` | `LinkPhase` enum → human string (all 7 variants) |
| `from_metrics_optional_fields` | `os_up`, `mtu`, `iface`, `kind` are `None` when source is `None` |
| `snapshot_json_roundtrip` | Serialize → deserialize produces identical snapshot (guards serde attrs) |
| `stats_snapshot_schema_version` | `StatsSnapshot` default has expected `schema_version` |

---

## 2. `net/util.rs` — `resolve_iface_ipv4()` / `bind_url_to_iface()` (0 tests → 5)

**Why:** `resolve_iface_ipv4` uses `unsafe libc::getifaddrs` — raw pointer
traversal of kernel data. `bind_url_to_iface` constructs RIST URLs that
librist parses. Zero tests today.

| Test | What it catches |
|------|----------------|
| `resolve_loopback_returns_127` | `lo` always exists; validates happy path through FFI |
| `resolve_nonexistent_returns_none` | Non-existent interface → `None` (not crash) |
| `bind_url_inserts_local_ip` | `rist://1.2.3.4:5000` + `lo` → `rist://127.0.0.1@1.2.3.4:5000` |
| `bind_url_preserves_existing_binding` | Already-bound URL `rist://10.0.0.1@1.2.3.4:5000` → unchanged |
| `bind_url_non_rist_scheme_returns_none` | `http://...` → `None` |

---

## 3. `net/interface.rs` — `LinkPhase::as_str()` (0 tests → 2)

**Why:** Phase strings are embedded in JSON telemetry and used for log
filtering. No test that the string values are stable.

| Test | What it catches |
|------|----------------|
| `link_phase_as_str_all_variants` | All 7 variants produce expected strings |
| `link_metrics_default_values` | `Default` impl produces sane zeroed state |

---

## 4. `scheduler/bonding.rs` — untested error/edge paths (3 tests)

**Why:** `BondingScheduler::send()` has an all-links-dead escalation path
(warn @ 1, error @ 100, error @ 1000) that is never exercised.
`remove_link()` and `get_all_metrics()` are only tested on the inner DWRR.

| Test | What it catches |
|------|----------------|
| `send_all_links_dead_returns_error` | Verifies error when no alive links exist |
| `remove_link_on_bonding_scheduler` | `remove_link()` actually removes from wrapper |
| `get_all_metrics_reflects_links` | `get_all_metrics()` returns expected link set |

---

## 5. `config.rs` — nested `deny_unknown_fields` (2 tests)

**Why:** Only top-level `[scheduler]` unknowns are tested. Unknown keys in
`[[links]]` and `[lifecycle]` are silently accepted if `deny_unknown_fields`
doesn't cover them (it does, but no test proves it).

| Test | What it catches |
|------|----------------|
| `unknown_key_in_links_rejected` | `[[links]]` with typo field → error |
| `unknown_key_in_lifecycle_rejected` | `[lifecycle]` with typo field → error |

---

## 6. `net/link.rs` — `infer_kind_from_iface_name` edge cases (2 tests)

**Why:** The function handles case-insensitive matching and prefix detection.
Missing edge tests for mixed case and `cdc*` cellular prefix.

| Test | What it catches |
|------|----------------|
| `infer_kind_case_insensitive` | `WLAN0`, `Eth0` still match |
| `infer_kind_cdc_cellular` | `cdc-wdm0` is cellular |

---

## 7. `rist-network-sim/scenario.rs` — degenerate inputs (2 tests)

**Why:** `total_steps` divides `duration / step`. A zero-duration or zero-step
config could produce division by zero or infinite loops.

| Test | What it catches |
|------|----------------|
| `zero_duration_produces_single_frame` | `duration=0` → one frame at t=0, no crash |
| `values_stay_within_bounds` | After 100 frames, all values are within [min, max] |

---

## 8. `runtime.rs` — `update_scheduler_config()` (1 test)

**Why:** `update_scheduler_config()` is a public API used for live NADA rate
adaptation but has no direct test (only tested indirectly via `apply_config`).

| Test | What it catches |
|------|----------------|
| `update_scheduler_config_reaches_worker` | Config update is processed without error |

---

## Excluded (not implementing)

| Idea | Why excluded |
|------|-------------|
| `net/wrapper.rs` null-pointer FFI tests | Cannot force librist OOM; mocking C FFI isn't realistic |
| `src.rs` / `sink.rs` GStreamer element unit tests | Requires GStreamer runtime init; integration tests already cover |
| `lock_or_recover()` poisoned mutex test | Intentionally panicking threads is fragile; the function is 3 lines |
| `stats_cb` / `log_cb` invalid pointer tests | Would require unsafe test harness calling C callbacks directly |
| Multi-threaded DWRR stress test | DWRR is single-threaded by design (owned by worker thread) |
| `u64::MAX` sequence wrap test | `u64::MAX` packets is unreachable in practice (~584 billion years at 1Mpps) |

---

**Total: 22 new tests across 8 modules**
