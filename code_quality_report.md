# Strata Code Quality Report

> Automated review of ~110 `.rs` source files across 9 workspace crates.

---

## Table of Contents

1. [Critical Issues](#1-critical-issues)
2. [Bugs & Correctness](#2-bugs--correctness)
3. [Security Concerns](#3-security-concerns)
4. [Performance](#4-performance)
5. [Code Duplication](#5-code-duplication)
6. [Long Functions](#6-long-functions)
7. [Magic Numbers](#7-magic-numbers)
8. [Error Handling](#8-error-handling)
9. [Dead Code & Suppressed Warnings](#9-dead-code--suppressed-warnings)
10. [Type Safety & API Design](#10-type-safety--api-design)
11. [Unsafe Code](#11-unsafe-code)
12. [Minor / Stylistic](#12-minor--stylistic)
13. [Positive Observations](#13-positive-observations)

---

## 1. Critical Issues

### 1.1 Duplicate GF(2⁸) Implementations

**Files:** `strata-transport/src/codec.rs`, `strata-transport/src/rlnc.rs`

Two independent Galois Field GF(256) implementations exist with *different* irreducible polynomials:

| File | Polynomial | Generator |
|------|-----------|-----------|
| `codec.rs` | `0x11B` | `3` |
| `rlnc.rs` | `0x11D` | `2` |

These are mathematically valid but **incompatible** — data encoded with one cannot be decoded with the other. If both codecs are ever used on the same stream, data corruption will result silently.

**Suggested fix:** Unify into a single `gf256` module with one polynomial choice, or make the polynomial a generic parameter and keep both as named instantiations.

### 1.2 Incomplete FEC Recovery (Receiver Discards Recovered Data)

**File:** `strata-transport/src/receiver.rs`

```rust
let _ = (idx, data);  // recovered FEC data is discarded
```

The receiver's `handle_control_packet` processes FEC repair packets but discards the recovered payload with `let _ = ...`. This means **FEC recovery is non-functional** — repair packets are received and decoded but their results are thrown away.

**Suggested fix:** Feed recovered `(idx, data)` back into the reorder buffer via `self.insert_packet(idx, data)` or equivalent.

### 1.3 SequenceGenerator Falsely Documented as Thread-Safe

**File:** `strata-transport/src/pool.rs`

The doc comment says "Thread-safe sequence number generator" but the struct uses a plain `u32` counter with no synchronization primitive (`AtomicU32`, `Mutex`, etc.). Concurrent use from multiple threads would cause duplicate sequence numbers and data corruption.

**Suggested fix:** Use `AtomicU32` with `fetch_add(1, Ordering::Relaxed)`, or remove the "thread-safe" documentation if single-threaded use is intended.

---

## 2. Bugs & Correctness

### 2.1 Misleading Variable Name in Congestion Controller

**File:** `strata-transport/src/congestion.rs:301`

```rust
let latest_rsrq = self.rsrp_history.last().map(|(_, v)| *v).unwrap_or(0.0);
```

Variable named `latest_rsrq` reads from `rsrp_history`. RSRP and RSRQ are different cellular metrics. This is either a bug (wrong data source) or a misleading variable name.

**Suggested fix:** Rename to `latest_rsrp` if reading RSRP is intentional.

### 2.2 Double-Counting in Adaptation Controller

**File:** `strata-bonding/src/adaptation.rs` — `update_with_feedback()`

Calls `self.update(links)` internally, then potentially overrides the result. Both paths mutate `consecutive_decreases`/`consecutive_increases`, leading to double-counting of trend counters when feedback triggers a reduction.

**Suggested fix:** Extract the shared metric-gathering logic from `update()` so `update_with_feedback()` can call it without side effects on the trend counters.

### 2.3 Diversity Preference Logic Is Ineffective

**File:** `strata-bonding/src/scheduler/dwrr.rs:218`

```rust
if is_diverse || selected.len() < n
```

When `selected.len() < n`, the condition is always true regardless of `is_diverse`, so the diversity check is a no-op during the initial fill phase — the first N links are always selected by quality alone.

**Suggested fix:** Gate insertion on `is_diverse` during the fill phase too, or add a second pass that swaps non-diverse links for diverse ones.

### 2.4 Silently Swallowed Parse Errors in Agent Control

**File:** `strata-agent/src/control.rs` — `handle_control_message()`

Every `envelope.parse_payload::<T>()` failure is silently swallowed via `if let Ok(...)`. A malformed payload produces no log entry and no error response to the control plane.

**Suggested fix:** Log at `warn!` level on parse failure and send an error envelope back to the control server.

---

## 3. Security Concerns

### 3.1 Directory Traversal in File Listing

**File:** `strata-agent/src/control.rs:341` — `list_directory()`

`fs::canonicalize` resolves symlinks but does not restrict the resolved path to a safe root directory. An attacker who controls the `files.list` message payload can list any directory on the filesystem.

**Suggested fix:** After canonicalization, verify the resolved path starts with an allowed prefix (e.g., `/opt/strata`).

### 3.2 Path Traversal in Interface Resolution

**File:** `strata-bonding/src/net/interface.rs:15`

```rust
format!("/sys/class/net/{}/", iface)
```

If `iface` contains `../` sequences, this creates paths outside `/sys/class/net/`.

**Suggested fix:** Validate that `iface` matches `^[a-zA-Z0-9_-]+$` before constructing the path.

### 3.3 Predictable Temp File Path

**File:** `strata-agent/src/pipeline.rs:250`

```rust
format!("/tmp/strata-stream-{}.toml", payload.stream_id)
```

Uses `/tmp` with a predictable filename. In multi-user environments this enables symlink attacks.

**Suggested fix:** Use `tempfile::NamedTempFile` or create files under a Strata-owned directory with restricted permissions.

### 3.4 SIM PIN in Plaintext Telemetry

**File:** `strata-common/src/models.rs:108`

`NetworkInterface.sim_pin: Option<String>` is serializable and will appear in JSON telemetry sent over WebSocket.

**Suggested fix:** Add `#[serde(skip_serializing)]` to `sim_pin`, or move it to a separate non-serializable config struct.

### 3.5 Password Hash Deserialization Not Blocked

**File:** `strata-common/src/models.rs:6`

`User.password_hash` has `#[serde(skip_serializing)]` but not `skip_deserializing`. A client could inject a known password hash via the API.

**Suggested fix:** Change to `#[serde(skip)]` to prevent both serialization and deserialization.

### 3.6 SQL Error Detection via String Matching

**File:** `strata-control/src/api/auth.rs`

```rust
e.to_string().contains("duplicate key")
```

Brittle and locale/version-dependent. Could misidentify errors or miss them.

**Suggested fix:** Match on the `sqlx::Error` variant and check the PostgreSQL error code (`23505` for unique violation) directly.

---

## 4. Performance

### 4.1 `Vec::remove(0)` — O(n) Where VecDeque Would Be O(1)

**5 occurrences** — all shift the entire Vec on every removal from the front:

| File | Line | Collection |
|------|------|-----------|
| `strata-transport/src/congestion.rs` | 213 | `bw_samples` |
| `strata-transport/src/congestion.rs` | 230 | `rtt_samples` |
| `strata-transport/src/congestion.rs` | 260 | `cqi_history` |
| `strata-transport/src/congestion.rs` | 277 | `rsrp_history` |
| `strata-transport/src/rlnc.rs` | 155 | `window` |

**Suggested fix:** Replace `Vec` with `VecDeque` and use `pop_front()`.

### 4.2 O(n²) Link Selection

**File:** `strata-bonding/src/scheduler/dwrr.rs:229`

The second pass in `select_best_n_links()` calls `.any(|l| l.id() == *id)` for each candidate, making the overall algorithm O(n²) in the number of links.

**Suggested fix:** Use a `HashSet<usize>` of already-selected link IDs for O(1) lookup.

### 4.3 Cloning LinkMetrics in Hot Loop

**File:** `strata-bonding/src/scheduler/dwrr.rs:286`

`state.metrics.clone()` performs a heap allocation per link per scheduler tick inside `select_link()`.

**Suggested fix:** Use references or cache the needed fields (alive, capacity) as scalars.

### 4.4 Linear Scan in Reorder Buffer

**File:** `strata-bonding/src/receiver/aggregator.rs:268`

`find_next_available()` does a full linear scan of a 2048-element `Vec<Option<Packet>>`.

**Suggested fix:** Track the minimum available index in a `BTreeSet` or use a different buffer structure.

### 4.5 O(n) Authentication Token Scanning

**File:** `strata-control/src/ws_agent.rs`

Each WebSocket authentication attempt scans ALL senders' enrollment tokens, performing Argon2 verification (expensive) on each until a match is found.

**Suggested fix:** Index enrollment tokens by a non-secret identifier (e.g., agent ID) so the lookup is O(1), then verify the token only for the matching entry.

### 4.6 Stats `RateCounter` Uses Vec with `retain()`

**File:** `strata-transport/src/stats.rs`

`RateCounter` stores timestamps in a `Vec` and calls `retain()` to prune expired entries — O(n) per update. Should use `VecDeque` and pop from the front since timestamps are naturally ordered.

### 4.7 Double-Collection Pattern in ARQ

**File:** `strata-transport/src/arq.rs` — `drain_pending()`

Collects pending packets into a `Vec`, then iterates with `inspect` to remove from the pending `HashMap`, requiring two passes over the data.

**Suggested fix:** Use `drain_filter` (nightly) or a single loop that collects and removes in one pass.

---

## 5. Code Duplication

### 5.1 Unix Socket Boilerplate in Pipeline

**File:** `strata-agent/src/pipeline.rs`

`switch_source()` (L155), `toggle_link()` (L185), and `send_command()` (L215) contain near-identical connect-and-write logic for Unix socket communication.

**Suggested fix:** Extract a helper: `fn send_to_control_socket(cmd: &str) -> bool`.

### 5.2 Repetitive Config Resolution

**File:** `strata-bonding/src/config.rs`

`LinkLifecycleConfigInput::resolve()` (12 lines) and `SchedulerConfigInput::resolve()` (18 lines) repeat the same `self.field.unwrap_or(defaults.field)` pattern for every field.

**Suggested fix:** Use a macro like `resolve_field!(self, defaults, field1, field2, ...)` or a generic merge trait.

### 5.3 Prometheus Metrics `write!` + `unwrap()`

**File:** `strata-bonding/src/metrics.rs`

~50 occurrences of `writeln!(out, ...).unwrap()` follow an identical pattern. Writing to a `String` cannot fail, but this is still noisy.

**Suggested fix:** Use `write!` with `?` and return `fmt::Result`, or use a macro to generate metric blocks.

### 5.4 Duplicate GStreamer Property Handling

**Files:** `strata-gst/src/sink.rs`, `strata-gst/src/src.rs`

Both files have identical `set_property()` / `property()` patterns with `.expect("type checked upstream")`.

**Suggested fix:** If the properties overlap, extract a shared settings struct and handler.

---

## 6. Long Functions

Functions exceeding ~80 lines are harder to test, review, and maintain.

| File | Function | ~Lines | Recommendation |
|------|----------|--------|----------------|
| `strata-agent/src/control.rs` | `handle_control_message()` | ~200 | Extract each match arm into its own method |
| `strata-control/src/api/streams.rs` | `start_stream()` | ~150 | Split config building, validation, and spawning into separate functions |
| `strata-gst/src/sink.rs` | `BaseSinkImpl::start()` | ~90 | Extract metrics server init, link registration, stats thread spawn |
| `strata-bonding/src/scheduler/dwrr.rs` | `refresh_metrics()` | ~82 | Extract per-link update logic |
| `strata-bonding/src/adaptation.rs` | `update()` | ~76 | Acceptable but borderline |
| `strata-agent/src/pipeline.rs` | `spawn_strata_node()` | ~70 | Extract argument construction |

---

## 7. Magic Numbers

Numeric literals without names make code harder to understand, tune, and configure.

### High-Priority (affects protocol/algorithm behavior)

| File | Line(s) | Value(s) | Context |
|------|---------|----------|---------|
| `congestion.rs` | various | `0.7` | Cautious rate reduction factor |
| `aggregator.rs` | 169, 175 | `0.1` | EWMA jitter alpha |
| `aggregator.rs` | 141, 180 | `128` | Jitter sample window size |
| `aggregator.rs` | 198-199 | `0.5` | Ramp-up/down change threshold |
| `aggregator.rs` | 254-256 | `0.95`, `0.05` | Loss rate smoothing factors |
| `adaptation.rs` | 272 | `0.01`, `500`, `0.7` | Loss/jitter/goodput thresholds |
| `adaptation.rs` | 223-232 | `0.95`, `0.90`, `0.80`, `1.2` | Capacity trend thresholds |
| `dwrr.rs` | 45 | `0.05`…`1.0` | Phase-based burst windows |
| `dwrr.rs` | 163 | `1_000_000.0` | Capacity floor (1 Mbps) |

### Medium-Priority (affects operational behavior)

| File | Line(s) | Value(s) | Context |
|------|---------|----------|---------|
| `main.rs` (agent) | 131 | `128` | Channel capacity |
| `pipeline.rs` | 113 | `5` | Kill timeout seconds |
| `pipeline.rs` | 288 | `100` | Polling interval ms |
| `sink.rs` | 314 | `50` | Stats polling sleep ms |
| `control.rs` | 262 | `5`, `3` | TCP reachability timeout/retries |

**Suggested fix:** Introduce named constants (e.g., `const EWMA_JITTER_ALPHA: f64 = 0.1;`) or move to config structs.

---

## 8. Error Handling

### 8.1 `Envelope::new()` Panics on Serialization Failure

**File:** `strata-common/src/protocol.rs:30`

```rust
.expect("payload serialization")
```

Any type that fails `serde_json::to_value()` will crash the process. This is called throughout the agent and control server.

**Suggested fix:** Return `Result<Envelope, serde_json::Error>`.

### 8.2 GStreamer Production Panics

**Files:** `strata-gst/src/sink.rs`, `strata-gst/src/src.rs`

Multiple `.unwrap()` / `.expect()` calls in non-test code:

- `add_pad(&pad).unwrap()` / `remove_pad(pad).unwrap()`
- `.expect("failed to spawn stats thread")`
- `.expect("type checked upstream")` (×8 across both files)

GStreamer elements should propagate errors via `gst::FlowError` or `gst::ErrorMessage`, not panic.

### 8.3 Silent Config Write Failure

**File:** `strata-agent/src/pipeline.rs:260`

```rust
let _ = std::fs::write(&config_path, &toml_str);
```

If the config file write fails, `strata-node` starts without bonding config and will silently misbehave.

**Suggested fix:** Log a warning and return an error if the write fails.

### 8.4 `serde_json::to_string().unwrap()` in Production

**Files:** `strata-control/src/ws_agent.rs:232,363`

Serializing envelopes to JSON should not panic in production. While unlikely to fail for well-typed structs, converting to `Result` propagation is safer.

---

## 9. Dead Code & Suppressed Warnings

**8 `#[allow(dead_code)]` annotations found:**

| File | Item | Action |
|------|------|--------|
| `transport/congestion.rs:123` | Field or method | Remove if unused, or use it |
| `transport/codec.rs:272` | Item | Remove or integrate |
| `transport/receiver.rs:211,219` | Two items | Clean up |
| `transport/rlnc.rs:39` | Item | Remove or integrate |
| `control/state.rs:42` | `AgentHandle.hostname` | Wire it up or remove |
| `agent/pipeline.rs:81` | `stream_id()` | Use or remove |
| `bonding/net/signal.rs:63` | Item | Remove or integrate |

**4 TODOs found:**

| File | Line | Description |
|------|------|-------------|
| `control/ws_agent.rs` | 174 | Device key auth not implemented |
| `agent/control.rs` | 123 | Saved device key for re-auth |
| `agent/hardware.rs` | 238 | Read carrier from ModemManager |
| `agent/hardware.rs` | 323 | `v4l2-ctl --list-formats-ext` |

**Test-only code in production modules:**

- `strata-gst/src/sink.rs:21` — `compute_congestion_recommendation()` is `#[cfg(test)]` only, never called in production.

---

## 10. Type Safety & API Design

### 10.1 String-Typed Enums

**File:** `strata-common/src/protocol.rs`

Several fields use `String` where an enum would provide compile-time safety:

| Field | Current | Suggested |
|-------|---------|-----------|
| `SourceConfig.mode` | `String` | `enum SourceMode { V4l2, Uri, Test }` |
| `StreamStopPayload.reason` | `String` | Reuse `StreamEndReason` |
| `InterfaceCommandPayload.action` | `String` | `enum InterfaceAction { Enable, Disable }` |

### 10.2 Inconsistent Request ID Optionality

**File:** `strata-common/src/protocol.rs`

`ConfigSetPayload.request_id` is `String` (required), but `ConfigUpdateResponsePayload.request_id` is `Option<String>`.

### 10.3 Cross-Crate Type Inconsistency

`LinkStats.id` is `u32` (in `strata-common/src/models.rs`), but `LinkConfig.id` in the bonding crate uses `usize`.

### 10.4 Heavy Mutex Fragmentation

**File:** `strata-bonding/src/net/transport.rs`

`TransportLink` uses many individual `Mutex` fields. `get_metrics()` acquires 4 separate locks. This increases contention and risks deadlocks.

**Suggested fix:** Group related fields into a single `Mutex<TransportLinkState>` struct.

### 10.5 Wide Structs

| Struct | Fields | File | Suggestion |
|--------|--------|------|------------|
| `NetworkInterface` | 17 | `models.rs` | Group cellular fields into `CellularInfo` |
| `LinkMetrics` | 16 | `net/interface.rs` | Group transport metrics into sub-struct |
| `LinkState` (DWRR) | 17 | `scheduler/dwrr.rs` | Group trend fields into `TrendState` |
| `AgentState` | 13 | `agent/main.rs` | Group into `Mutex<AgentConfig>` |

---

## 11. Unsafe Code

**6 `unsafe` blocks found** — all in FFI-adjacent code:

| File | Line | Purpose | Issue |
|------|------|---------|-------|
| `bonding/runtime.rs` | 393 | Socket options | Needs `// SAFETY:` comment |
| `bonding/runtime.rs` | 435 | Socket options | Needs `// SAFETY:` comment |
| `bonding/bin/strata_receiver.rs` | 484 | Socket options | Needs `// SAFETY:` comment |
| `agent/pipeline.rs` | 129 | `libc::kill()` | PID `u32→i32` cast can overflow; needs safety comment |
| `bonding/net/interface.rs` | 13 | `getifaddrs` | Missing null check on `ifa_name`; needs safety comment |
| `bonding/net/zerocopy.rs` | 91 | Zero-copy I/O | Needs `// SAFETY:` comment |

**Suggested fix:** Add `// SAFETY:` comments to all unsafe blocks. Fix the PID cast in `pipeline.rs` to use `libc::pid_t` or check for overflow.

---

## 12. Minor / Stylistic

### 12.1 Missing Config Validation

**File:** `strata-bonding/src/config.rs`

No bounds checking on:
- `good_loss_rate_max` (should be `[0.0, 1.0]`)
- `congestion_headroom_ratio` (should be `[0.0, 1.0]`)
- `congestion_trigger_ratio` (should be `[0.0, 1.0]`)
- `redundancy_spare_ratio` (should be `[0.0, 1.0]`)
- Negative RTT/capacity thresholds

### 12.2 Version 0 Silently Accepted

**File:** `strata-bonding/src/config.rs:303`

`BondingConfigInput::resolve()` treats `version == 0` as valid, mapping it to `CONFIG_VERSION`. This should produce a warning or error.

### 12.3 Missing `PartialEq`/`Eq` Derives

**File:** `strata-common/src/models.rs`

`NetworkInterface`, `MediaInput`, `SenderStatus`, `Sender`, `Stream`, `Destination` lack `PartialEq`/`Eq`, making test assertions harder.

### 12.4 `tracing_subscriber::fmt().init()` Panics on Double-Init

**File:** `strata-agent/src/main.rs:91`

Using `.init()` instead of `.try_init()` will panic if a subscriber is already set. Fragile in integration tests.

### 12.5 Unused `_reconnect_rx`

**File:** `strata-agent/src/main.rs:139`

The receiver half of the reconnect channel is created but immediately discarded — dead code.

### 12.6 Blocking Call in GStreamer Streaming Thread

**File:** `strata-gst/src/src.rs:230`

`PushSrcImpl::create()` calls `rx.recv()` (blocking `std::sync::mpsc::Receiver`) on a GStreamer streaming thread. If the source is slow, this blocks the entire pipeline.

### 12.7 Infinite Loop Without Bound

**File:** `strata-gst/src/sink.rs:240`

`request_new_pad` uses `loop { i += 1 }` to find a free pad name with no upper bound.

---

## 13. Positive Observations

- **Good test coverage** — transport wire format uses proptest, bonding scheduler has extensive unit tests, config parsing is well-tested.
- **Clean separation of concerns** — crate boundaries are well-defined (transport, bonding, control, agent, common).
- **Appropriate use of `DashMap`** for concurrent state in the control server.
- **Slab-based packet pool** (`transport/pool.rs`) provides O(1) allocation/deallocation.
- **Auth is well-implemented** — Ed25519 JWT tokens, Argon2id password hashing, proper secret key management.
- **Error propagation** is generally good outside of the specific cases noted above.
- **BBR-inspired congestion controller** is sophisticated and well-structured with clear state machine phases.
- **DWRR scheduler** is well-designed with link lifecycle management, penalty tracking, and adaptive redundancy.

---

## Summary by Severity

| Severity | Count | Examples |
|----------|-------|---------|
| **Critical** | 3 | Dual GF(256), FEC recovery discarded, false thread-safety claim |
| **Bug** | 4 | Variable naming, double-counting, ineffective diversity, swallowed errors |
| **Security** | 6 | Directory traversal, path traversal, predictable tmp, plaintext SIM PIN |
| **Performance** | 7 | `Vec::remove(0)`, O(n²) selection, hot-path cloning |
| **Duplication** | 4 | GF(256), socket helpers, config resolution, metrics format |
| **Error Handling** | 4 | Panicking `Envelope::new()`, GStreamer panics, silent failures |
| **Design** | 6 | String-typed enums, type inconsistencies, wide structs |
| **Minor** | 7+ | Magic numbers, dead code, missing validation |
