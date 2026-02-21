# Update Opportunities — GStreamer 1.28 + gstreamer-rs 0.25 + Rust nightly

Analysis performed against the current codebase on 2026-02-21.

---

## GStreamer 1.28 (released 27 Jan 2026)

### 1. `mpegtsmux` skew-corrections property ⭐ Low effort, immediate win

GStreamer 1.28 adds a `skew-corrections` property to `mpegtsmux`. The project uses
`mpegtsmux alignment=7` in the bonded transport path. When the stream is being
re-encoded or remuxed (rather than live playout), `skew-corrections=false` should be
set to preserve original timestamps and frame spacing instead of correcting for clock
drift. This is the correct behaviour for a bonding transport where the receiver
remuxes to HLS/DASH or writes to file.

**Where:** pipeline fragment construction in `crates/strata-gst/src/codec.rs`

---

### 2. Enhanced FLV multitrack audio ⭐ Low effort, immediate win

The project already uses `eflvmux` for H.265 RTMP relay (`relay_muxer_fragment()` in
`crates/strata-gst/src/codec.rs`). GStreamer 1.28 adds full multitrack audio/video
support to the Enhanced RTMP V2 spec. Multiple audio languages or commentary tracks
can now be carried in the relay path without any muxer code changes — just add extra
audio pads to `eflvmux`.

**Where:** `crates/strata-gst/src/codec.rs` → `relay_muxer_fragment()`

---

### 3. `appsrc`/`appsink` simple callbacks — Medium effort

gstreamer-rs 0.25 exposes a new "simple callbacks" API for `appsink`/`appsrc` that
avoids GObject signal overhead. If the receiver side ever needs to inspect frames
(e.g., for QR code validation, or feeding into the analytics pipeline), this is far
cleaner and lower overhead than GObject signals from Rust.

**Where:** future `stratasrc` output tapping; any analytics integration

---

### 4. `unixfdsink` zero-copy IPC — Medium effort

The `unixfdsink`/`unixfdsrc` elements now copy buffers when needed (previously they
only worked with their own allocator). On the same host, they provide zero-copy
inter-pipeline IPC via Linux shared memory. If the agent and a local display sink
ever run co-located, this eliminates memory copies between `stratasrc` output and a
display/re-encode pipeline.

**Where:** potential co-located agent + display pipeline architecture

---

### 5. Task pool `GstContext` — Low effort

A new `GstSharedTaskPool` context lets multiple elements in a pipeline share a thread
pool. This reduces thread overhead across the encoding pipeline (`x264enc`/`x265enc`
+ `audioconvert` + `audioresample`), which matters given the project already manages
its own stats threads and scheduler thread.

**Where:** pipeline construction in `crates/strata-gst/`; set context on the pipeline
object before setting to PLAYING.

---

### 6. QUIC/WebTransport connection sharing — Future

The `quinn` GStreamer plugin now supports sharing a QUIC connection/session between
elements. The release notes explicitly frame this as groundwork for **Media over
QUIC (MoQ)** — the emerging low-latency streaming standard. Since Strata already uses
`quinn-udp`, this creates a future path for MoQ compatibility without replacing the
bonding layer.

**Where:** future transport layer; `crates/strata-transport/`

---

### 7. RTP Rust payloaders — Future

New Rust-based RTP payloaders/depayloaders for raw audio (L8/L16/L24) and SMPTE
ST291 ancillary data. Not directly used now, but if Strata adds an RTP-compatible
framing option (e.g., for interop with WebRTC endpoints or SRT bridges), these are
production-quality Rust elements with correct multichannel timestamp handling.

---

## Rust Nightly

### 8. Safe architecture intrinsics ⭐ Low effort (stabilised in Rust 1.87)

With safe arch intrinsics, any SIMD operations in packet processing — such as
checksum computation, or FEC encoding in `raptorq` — no longer require `unsafe`
blocks when the target feature is statically confirmed via `#[target_feature]`. The
`build_monoio_runtime!` macro in `crates/strata-bonding/src/runtime.rs` already has a
conditional Linux io_uring/SQPOLL path; the same pattern can be applied to hot path
vectorisation.

**Where:** `crates/strata-bonding/src/runtime.rs`, `raptorq` FEC hot path

---

### 9. `async` closures (`async || {}`) — Medium effort (stabilised in Rust 1.85)

The stats threads in `stratasink` and `stratasrc` use `std::thread::spawn` + `Mutex`
+ sleep poll loops. With `async` closures now stable, these could be refactored into
async tasks on the monoio runtime the project already manages, eliminating the
dedicated OS threads and their stack overhead per element instance.

**Where:** `crates/strata-gst/src/sink.rs` stats thread,
`crates/strata-gst/src/src.rs` stats thread

---

### 10. Rust 2024 edition ⭐ Low effort (stabilised in Rust 1.85)

The project can adopt `edition = "2024"` in each crate's `Cargo.toml`. Key wins:

- Stricter `unsafe fn` requirements — aligns with the GStreamer FFI unsafe boundaries
  and makes violations a hard error rather than a lint
- Better RPIT lifetime capture semantics — cleaner `impl Trait` returns from GStreamer
  element methods
- Improved `if let` / tail expression drop ordering — relevant to the many
  `lock_or_recover()` guard patterns throughout the codebase

**Where:** every `Cargo.toml` under `crates/`

---

### 11. `gen` blocks / generators — Future (nightly only)

The stats polling loop in `StrataSink` (pull metrics → post element message → sleep →
repeat) is a textbook candidate for a `gen {}` block. Once stabilised, this replaces
the manual loop + sleep thread with a lazy iterator that the async runtime can drive,
reducing boilerplate significantly.

**Where:** `crates/strata-gst/src/sink.rs`, `crates/strata-gst/src/src.rs`

---

### 12. Portable SIMD (`std::simd`) — High effort (nightly only)

The bonding scheduler's DWRR weight calculations and `raptorq` FEC encoding are
CPU-intensive. Portable SIMD (`std::simd`) on nightly allows writing vectorised
packet-processing code that targets AVX2/NEON without architecture-specific
intrinsics, keeping the codebase portable to the aarch64 target.

**Where:** `crates/strata-bonding/src/scheduler/`, `raptorq` integration

---

## Summary

| # | Feature | Version | Effort | Status |
|---|---------|---------|--------|--------|
| 1 | `mpegtsmux skew-corrections=false` | GStreamer 1.28 | Low | ✅ Done |
| 2 | Enhanced FLV multitrack audio | GStreamer 1.28 | Low | ✅ Available (eflvmux already used) |
| 3 | appsink simple callbacks | GStreamer 1.28 / gst-rs 0.25 | Medium | Near-term |
| 4 | unixfdsink zero-copy IPC | GStreamer 1.28 | Medium | Near-term |
| 5 | Task pool GstContext | GStreamer 1.28 | Low | ⏳ Blocked (not in gst-rs 0.25) |
| 6 | QUIC/WebTransport / MoQ | GStreamer 1.28 | High | Future |
| 7 | RTP Rust payloaders | GStreamer 1.28 | Medium | Future |
| 8 | Safe architecture intrinsics | Rust 1.87+ / nightly | Low | N/A (no arch intrinsics in codebase) |
| 9 | `async` closures for stats threads | Rust 1.85+ / nightly | Medium | Deferred (minimal gain) |
| 10 | Rust 2024 edition | Rust 1.85+ / nightly | Low | ✅ Done |
| 11 | `gen` blocks for stats loops | Rust nightly only | Low | Future |
| 12 | Portable SIMD (`std::simd`) | Rust nightly only | High | Future |
