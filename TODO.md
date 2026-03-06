# Strata Project Status & Handoff (2026-03-06 evening)

## Session Summary

This session ran a field test, observed 0 HLS segments despite 13K packets delivered,
and diagnosed + fixed a three-bug cluster in `stratasrc` that caused the receiver to
silently do nothing.

---

## Bugs Fixed This Session

### 1. `stratasrc` not declared as live source — Bug A (PRIMARY)
**File**: [crates/strata-gst/src/src.rs](crates/strata-gst/src/src.rs)

`BaseSrcImpl::start()` was missing `set_live(true)` and `set_format(Format::Time)`.
GStreamer treats non-live sources as seekable/file-like. The HLS pipeline
(`stratasrc → queue → tsdemux → h265parse/aacparse → hlssink2`) would block in
PAUSED state waiting for `hlssink2` to pre-roll on both audio+video pads
simultaneously — which requires `tsdemux` to have already parsed PAT/PMT and
created dynamic pads. With ~50% initial packet loss the PAT/PMT delay was so long
the pipeline never transitioned to PLAYING.

**Fix**: Added `self.obj().set_live(true); self.obj().set_format(gst::Format::Time);`
at the start of `BaseSrcImpl::start()`.

### 2. DISCONT flag was commented out — Bug B (CRITICAL)
**File**: [crates/strata-gst/src/src.rs](crates/strata-gst/src/src.rs) — `PushSrcImpl::create()`

```rust
// if discont {
//     let buf_ref = buffer.get_mut().unwrap();
//     buf_ref.set_flags(gst::BufferFlags::DISCONT);
// }
```

Without DISCONT, `tsdemux` never knows bytes are missing. On lossy LTE with
initial 50%+ loss, `tsdemux` enters a confused state trying to complete PES
packets that started in dropped buffers. It never produces output pads, so
`hlssink2` gets no input and produces no segments.

**Fix**: Uncommented the DISCONT block.

### 3. `unlock()` permanently destroyed the receiver — Bug C (CRITICAL)
**File**: [crates/strata-gst/src/src.rs](crates/strata-gst/src/src.rs)

`unlock()` was calling `receiver.shutdown()` which drops `input_tx`/`output_tx`,
permanently killing the output channel. GStreamer calls `unlock()` during any
state transition (PLAYING→PAUSED) expecting `create()` to return quickly; the
receiver must survive to be reused when the pipeline resumes.

**Fix**: Added `flushing: AtomicBool` field. `unlock()` sets it; `unlock_stop()`
clears it. `create()` now polls `recv_timeout(100ms)` in a loop and returns
`FlowError::Flushing` when the flag is set, instead of blocking forever.

### All three were applied atomically and build cleanly:
```
cargo build --release -p strata-gst  →  Finished (no errors)
cargo test -p strata-bonding          →  39 tests, 0 failures
```

---

## State of the Goodput-Capped Ramp-Up (from earlier in the session)

**File**: [crates/strata-bonding/src/adaptation.rs](crates/strata-bonding/src/adaptation.rs) lines 628-634

The prior session's fix caps ramp-up target at `1.3× EWMA goodput` to prevent
the overshoot-crash cycle. This was already built and deployed in the last field
test. The adaptation trace showed it working: bitrate oscillated 500–1400 kbps
instead of the prior 500–2000+ kbps.

---

## What Needs to Happen Next

### 1. Re-run the field test with the `stratasrc` fixes (HIGHEST PRIORITY)
```bash
cd /home/jake/Documents/strata
source .env
bash scripts/field-test.sh
```

Expected outcome: `tsdemux` now produces HLS segments. The first run may still
show partial failure if initial packet loss is severe — watch for `segments=N`
climbing in the monitor output.

### 2. Write a reproduction test (was next on the todo list when session ended)
A test that verifies DISCONT propagation through the reassembly pipeline and
confirms the receiver survives a `unlock()` call between `create()` calls.
The most useful level is a `strata-bonding` transport test that:
- Sends 20 packets, drops packets 5-10 (simulating initial loss), sends 10 more
- Verifies the output channel yields items with `discont=true` where expected
- Calls a simulated `unlock` (just drop the flag) and verifies the receiver
  continues to produce output after re-enabling

The GStreamer-level test (pipeline integration) is harder to write portably but
would be the definitive regression test. File would go in `crates/strata-gst/tests/`.

### 3. Remaining known issues (from previous session analysis)
These are still open:

| # | Issue | Severity | Location |
|---|-------|----------|----------|
| P1 | Capacity estimation stuck at BBR cold-start ~5000 kbps | HIGH | `transport.rs:729` |
| P2 | Link death detection too slow (100% loss for 15+ samples before marking dead) | HIGH | `transport.rs` |
| P3 | 500 EAGAIN at startup (send buffer overflow) | MEDIUM | `transport.rs` |
| P4 | Sawtooth bitrate oscillation even with goodput cap | MEDIUM | `adaptation.rs` |

See the bottom of this file for the original detailed root cause analysis.

---

## Network State (last measured 2026-03-06)

| Modem | Interface | Band | RSRP | SINR | Upload |
|-------|-----------|------|------|------|--------|
| Modem 1 | enp2s0f0u4 | 8 (re-locked) | -101 dBm | -1 dB | ~6.6 Mbps |
| Modem 2 | enp11s0f3u1u3 | 8 | -100 dBm | 5 dB | ~3.3 Mbps |

Band lock on Modem 1 is fragile — reverts to Band 7 on reconnect. Consider
`scripts/band-lock.sh` in a cron job.

---

# Original Root Cause Analysis (kept for reference)
