# Real-World Snags — Strata Cellular Streaming Setup

Snags encountered during real-hardware runs with 2× Huawei E3372h-320
HiLink dongles and a Hetzner aarch64 receiver. Recorded in discovery order
across three debugging sessions.

Each entry includes a **regression guard** — a test or check that prevents
the bug from silently returning.

---

## 1. Dongles in HiLink mode — no AT command access

**Symptom:** Expected `/dev/ttyUSB*` serial ports for AT commands. None appeared.
The E3372h-320 presents only as a CDC-Ethernet NIC in HiLink (router) mode.

**Impact:** No band selection, signal monitoring, or modem control via AT
commands.

**Workaround:** Use the HiLink HTTP API at the modem gateway IP
(`http://192.168.8.1/api/...`). Covers signal metrics, network mode, PLMN.

**Regression guard:** The `scripts/field-test.sh` health-check script queries
each modem's HiLink API and warns if signal or registration is degraded.

---

## 2. Both dongles on the same 192.168.8.x subnet

**Symptom:** Both dongles default to 192.168.8.0/24. The kernel can only route
one default gateway per destination subnet, so the second dongle's gateway was
unreachable.

**Impact:** Only one link was genuinely reachable; bonding was non-functional.

**Fix:** Change one dongle's LAN subnet to 192.168.9.x via the admin UI. The
device stores this setting persistently.

**Regression guard:** `scripts/field-test.sh` detects overlapping link subnets and
aborts with a clear message.

---

## 3. Dongle stuck on 2G — hardware/firmware state issue

**Symptom:** After subnet fix, one dongle reported `workmode=GSM` with no LTE
metrics. It was physically cold (idle).

**Root cause (across 2 sessions):** Setting `NetworkMode=03` (LTE-only) locked
the dongle to an LTE PLMN where the Vodafone SIM had no roaming agreement.
SIM-swap tests proved it was a dongle firmware state issue, not the SIM.

**Fix:** Physical reset button on the dongle, then set NetworkMode to `00`
(Auto 4G+3G+2G). Dongle registered on Vodafone and acquired a WAN IP.

**Regression guard:** `scripts/field-test.sh` queries each modem's `workmode`
and warns if any link is on 2G or has no WAN IP.

---

## 4. `voaacenc` hardcoded — not installed anywhere

**Symptom:** Sender pipeline hardcoded `voaacenc bitrate=128000`. Neither machine
had the VO-AAC GStreamer plugin installed.

**Impact:** `strata-pipeline sender` failed at pipeline construction with
`Error: no element "voaacenc"`.

**Fix applied:** Runtime AAC encoder probe: tries `fdkaacenc → faac → avenc_aac`
in order, with clear error if none found.

**Regression guard:** `test_aac_encoder_probe()` in `strata_pipeline.rs` unit
tests verifies the probe logic returns a valid encoder name and does not panic
on missing elements.

---

## 5. Docker cross-compile for aarch64 — syntax frontend timeout

**Symptom:** Dockerfile used `# syntax=docker/dockerfile:1.7-labs` which caused
BuildKit to fetch a frontend image from Docker Hub. Timed out in test
environments.

**Fix applied:** Removed `# syntax=docker/dockerfile:1.7-labs` directive.
Added `make cross-aarch64` Makefile target that invokes `docker buildx build`
with proper `--output` for artifact extraction.

**Regression guard:** `make cross-aarch64` target uses standard Dockerfile
features only. No Labs syntax required.

---

## 6. `SO_BINDTODEVICE` silently fails without `CAP_NET_RAW`

**Symptom:** The TOML config named interfaces correctly, but `setsockopt(SO_BINDTODEVICE)`
only logged a `warn!()` (via GStreamer debug, invisible on stderr) when it
returned `EPERM`. All sockets went through the default route = no bonding.

**Impact:** The entire multi-path architecture was non-functional.

**Fix applied:** `create_transport_link()` in `runtime.rs` now returns a hard
error with an actionable message (including the `setcap` hint) instead of a
warning. `make install` applies `cap_net_raw+ep` automatically.

**Regression guard:** `test_bind_to_device_rejects_eperm()` in `runtime.rs`
verifies that a simulated `EPERM` returns `Err`, not `Ok` with a log line.

---

## 7. Source-based routing needs policy routing tables

**Symptom:** Binding a socket to a source IP (e.g. 192.168.8.102) still routes
through the kernel's single default route, not the dongle's gateway.

**Impact:** Without policy routing (`ip rule` + per-interface route tables),
`SO_BINDTODEVICE` is the only way to force traffic onto a specific dongle.

**Fix:** Document the policy routing setup in the wiki. The `scripts/field-test.sh`
script checks for `SO_BINDTODEVICE` capability as a prerequisite.

**Regression guard:** Covered by snag #6 — the hard error on `EPERM` ensures
users know binding failed even without policy routing.

---

## 8. Dongles take over WiFi default route

**Symptom:** Dongle DHCP announces default routes at metric 100. WiFi gets
metric 600. All internet traffic (SSH, Cargo downloads) routes via a dongle.

**Fix:** Pin WiFi route to metric 50:
```
nmcli connection modify "YourWiFiSSID" ipv4.route-metric 50
```

**Regression guard:** `scripts/field-test.sh` warns if the primary internet
route goes through a modem interface.

---

## 9. HLS segments never close — `send-keyframe-requests=true` stall

**Symptom:** `segment00000.ts` created but stayed at 0 bytes. `hlssink2` was
waiting for a keyframe that `stratasrc` (a network source) cannot produce.

**Fix applied:** Set `send-keyframe-requests=false` on `hlssink2`. Natural
keyframes from `x265enc` (key-int-max=60 at 30fps = 2s) align with
`target-duration=2`.

**Regression guard:** `test_hlssink2_keyframe_requests_disabled()` verifies the
pipeline string contains `send-keyframe-requests=false`.

---

## 10. HLS uploader sent 0-byte files and stale segments

**Symptom:** `find_new_segments()` included 0-byte files (created by hlssink2
before any data is written). Uploader PUT empty bodies to YouTube, exhausting
retries.

**Fix applied:** Skip 0-byte files. Skip the lexicographically latest segment
during live polling (it may still be open for writing).

**Regression guard:** `test_find_new_segments_skips_zero_byte()` and
`test_find_new_segments_skips_latest()` in `hls_upload.rs`.

---

## 11. SSH quoting breaks with YouTube HLS URL

**Symptom:** YouTube URL `?cid=...&copy=0&file=` breaks double-quoted SSH
strings — `&` backgrounds the remote command.

**Fix:** Write receiver commands to a script file, then execute the script.

**Regression guard:** `scripts/field-test.sh` uses script files for all remote
commands, never inline URLs in SSH strings.

---

## 12. TOML interface map overridden by routing-table lookup

**Symptom:** All links showed `(via wlan0)` in the sender log even though the
TOML had per-link `interface = "enp2s0f0u4"` etc. The routing-table fallback
(`resolve_interface_for_uri()`) was always winning because it ran unconditionally.

**Impact:** All links went through WiFi. No cellular bonding occurred even though
`SO_BINDTODEVICE` was working.

**Fix applied:** In `strata_pipeline.rs`, parse `[[links]]` from the TOML config
*before* the pad loop. Per-link `interface` in TOML takes priority over
`resolve_interface_for_uri()`. The routing-table function is only called as a
fallback when no TOML interface is specified.

**Regression guard:** `test_toml_interface_overrides_routing_table()` in
`strata_pipeline.rs` verifies TOML-specified interfaces are not overwritten.

---

## 13. Hot-swap source switch crashes receiver

**Symptom:** Switching from test source to v4l2 via the control socket
(`{"cmd":"switch_source","mode":"v4l2",...}`) caused a timestamp
discontinuity. The receiver's `hlssink2` crashed with:
`"Timestamping error on input streams"`.

**Impact:** Receiver process dies. All HLS segments stop.

**Workaround:** Never hot-swap sources after the pipeline is streaming. Always
kill the sender and restart with the correct `--source` argument from the start.

**Regression guard:** Documented as a known limitation in `--help` output.
`scripts/field-test.sh` starts the sender with the final intended source.

---

## 14. `max_latency_ms` not wired through to receiver jitter buffer

**Symptom:** TOML `[scheduler] max_latency_ms = 3000` was parsed correctly but
never reached the `ReassemblyBuffer`. The GStreamer element `StrataSrc` only
passed `start_latency` to `ReceiverBackend::new()`. The `ReassemblyConfig`
default of 500ms was always used, causing 25%+ of packets to be classified as
"late" and dropped on LTE links with >500ms jitter spikes.

**Fix applied:**
- Added `max_latency_ms` to the `Settings` struct in `src.rs`
- `apply_config_toml()` now reads `cfg.scheduler.max_latency_ms`
- `ReceiverBackend` gained `new_with_config(ReassemblyConfig)` method
- Element startup builds a full `ReassemblyConfig` with both `start_latency`
  and `max_latency_ms` from the TOML

**Regression guard:** `test_max_latency_ms_wired_from_config()` in
`aggregator.rs` integration tests verifies that constructing a
`ReassemblyBuffer` with a custom `max_latency_ms` actually changes the
ceiling, and that packets arriving within the configured ceiling are NOT
counted as late.

---

## 15. Duplicate interface in TOML causes self-congestion

**Symptom:** Using 3 links with only 2 dongles meant one interface carried 2/3
of the traffic. IDR keyframes (large bursts) on the doubled interface caused
immediate congestion, pushing the sender into `KeyframeOnly` mode permanently.

**Fix:** Use exactly one link per physical interface. With 2 dongles, use 2
links.

**Regression guard:** `scripts/field-test.sh` validates that no interface
appears more than once in the TOML config.

---

## 16. `critical_broadcast` doubles IDR burst on all links

**Symptom:** With `critical_broadcast = true`, every IDR keyframe was sent on
*all* links simultaneously. At 640×360 H.265, an IDR spans ~30 UDP packets.
Sending the same burst on both links simultaneously doubled the instantaneous
load on both congested LTE paths, causing 99% loss during the burst window.
Only the first segment was ever produced — every subsequent IDR was lost.

**Fix:** Set `critical_broadcast = false` in the sender TOML. With 2 links
and no redundancy, the scheduler splits packets across links normally. IDR
packets are distributed, not duplicated.

**Regression guard:** `scripts/field-test.sh` defaults to
`critical_broadcast = false` for LTE configurations. A comment in the
generated TOML explains when to enable it (only when links have significant
spare capacity).

---

## Summary Table

| # | Snag | Severity | Fixed? |
|---|------|----------|--------|
| 1 | HiLink mode, no AT commands | Medium | ✅ Workaround (HTTP API) |
| 2 | Both dongles same subnet | High | ✅ Manual fix |
| 3 | Dongle stuck on 2G | High | ✅ Physical reset |
| 4 | `voaacenc` not installed | Critical | ✅ Runtime probe |
| 5 | Docker cross-compile frontend | Medium | ✅ Fixed (no Labs syntax) |
| 6 | `SO_BINDTODEVICE` silent EPERM | Critical | ✅ Hard error |
| 7 | Source routing needs policy tables | High | ✅ Documented |
| 8 | Dongles take over WiFi route | Medium | ✅ Documented |
| 9 | `send-keyframe-requests` stall | Critical | ✅ Set to false |
| 10 | HLS uploader 0-byte files | High | ✅ Skip logic |
| 11 | SSH quoting with URLs | Low | ✅ Script files |
| 12 | TOML interface map overridden | Critical | ✅ TOML-first priority |
| 13 | Hot-swap crashes receiver | High | ⚠️ Known limitation |
| 14 | `max_latency_ms` not wired | Critical | ✅ Full config path |
| 15 | Duplicate interface self-congestion | High | ✅ 1 link per dongle |
| 16 | `critical_broadcast` IDR doubling | Critical | ✅ Disabled for LTE |
