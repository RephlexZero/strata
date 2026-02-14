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

---

## Phase 2: GStreamer Element Tests (25 new tests)

Element-level coverage for the `gst-rist-bonding` crate that sits between the
existing pure-function unit tests (7 in sink.rs) and the heavyweight network
integration tests (17 in tests/). Covers properties, config wiring, pad
lifecycle, buffer flags, metadata, and error paths that previously had zero
coverage.

### Tier 2 — Pure Rust (no GStreamer init)

| Module | Test | What it catches |
|--------|------|----------------|
| `util.rs` | `lock_or_recover_normal` | Happy-path mutex lock works |
| `util.rs` | `lock_or_recover_poisoned` | Poisoned mutex is recovered (not panic) |
| `sink.rs` | `nada_rate_signals_at_exact_rmin_boundary` | r_vin clamps exactly at RMIN floor (150 kbps) |

### Tier 1 — Element factory (gst::init, no pipeline, no network)

| Module | Test | What it catches |
|--------|------|----------------|
| `lib.rs` | `test_sink_property_roundtrip` | links, config, max-bitrate get/set fidelity + defaults |
| `lib.rs` | `test_src_property_roundtrip` | links, latency, config get/set fidelity + defaults |
| `lib.rs` | `test_sink_config_toml_applies` | Config TOML stored after set_property |
| `lib.rs` | `test_src_config_toml_applies_latency` | apply_config_toml wires receiver start_latency through |
| `lib.rs` | `test_sink_config_file_rejects_traversal` | `..` path guard rejects path traversal |
| `lib.rs` | `test_src_config_file_rejects_traversal` | Same guard on source element |
| `lib.rs` | `test_sink_config_file_nonexistent` | Missing file doesn't panic |
| `lib.rs` | `test_src_config_file_nonexistent` | Same on source element |
| `lib.rs` | `test_sink_invalid_config_no_panic` | Garbage TOML doesn't panic |
| `lib.rs` | `test_src_invalid_config_no_panic` | Same on source; invalid TOML not stored |
| `lib.rs` | `test_sink_element_metadata` | long-name, klass, description correctness |
| `lib.rs` | `test_src_element_metadata` | Same for source element |
| `lib.rs` | `test_sink_pad_templates` | 2 templates: sink (Always) + link_%u (Request) |
| `lib.rs` | `test_src_pad_templates` | 1 template: src (Always) |
| `lib.rs` | `test_request_pad_lifecycle_multiple` | 3 pads create/release, pad count returns to 1 |
| `lib.rs` | `test_request_pad_auto_naming` | Auto-naming assigns unique link_N names |
| `lib.rs` | `test_sink_empty_links_no_crash` | Empty and comma-only links strings don't crash |
| `lib.rs` | `test_sink_pad_uri_property` | Pad URI default, set, update round-trip |
| `lib.rs` | `test_sink_max_bitrate_default_and_live_update` | max-bitrate default 0, set/read/reset |

### Tier 3 — Minimal pipeline (appsrc/fakesrc, no network)

| Module | Test | What it catches |
|--------|------|----------------|
| `lib.rs` | `test_buffer_flags_to_profile` | All 6 flag combos flow through render() without error |
| `lib.rs` | `test_sink_stop_without_start` | NULL→READY→NULL with no start() call |
| `lib.rs` | `test_src_stop_without_start` | Same for source element |

### Bug fix discovered during testing

| Fix | Description |
|-----|-------------|
| `request_new_pad` auto-naming | Pad names were not reserved in `pad_map` during creation, causing duplicate `link_0` when requesting multiple pads with `name=None`. Fixed by calling `get_id_for_pad()` immediately after name generation. |
