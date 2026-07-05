# UX & Trust Audit — why the 2026-07-05 livestream failed, and whether the platform needs re-architecting

Date: 2026-07-06 (session following the production deploy).
Trigger: operator failed to get a watchable YouTube stream from the dashboard,
reported that hot-swap settings "seem to restart the stream", that interface
and video device names are unidentifiable, and asked for a full audit and a
re-architecture verdict.

**Verdict up front: no re-architecture.** The architecture (control plane ↔
daemons over the envelope protocol, per-stream pipeline children, bonded
transport) is sound and was proven end-to-end on real Band 8 earlier the same
day. Tonight's failure was a *chain of six specific edge defects* — every one
located, every one fixable in place. What the platform lacks is not a new
architecture but **truthful feedback**: the UI acts on guesses (name-prefix
interface types, unfiltered device scans), swallows failure reasons, and
renders optimistic state. Fix the trust layer; keep the bones.

---

## Part 1 — Field forensics: the failure chain (all confirmed from logs/DB)

The operator ran ~8 stream attempts 21:27–21:52 UTC. None could have worked.
Each attempt failed on one or more of:

**F1. Link 0 pinned to the routeless LAN interface.**
`/tmp/strata-stream-*.toml` on the Pi proves it: link 0 → `enP4p65s0`
(192.168.50.x LAN, no default route), link 1 → `enx001e101f0000` (modem 1).
Modem 2 (`eth0`!) was never pinned. Sender log: link 0 `packets_sent=776
packets_acked=0 stale_ms=420000+`. The pinning code
(`crates/strata-sender/src/pipeline.rs` spawn_pipeline) filters
`scan_network_interfaces()` on `Connected` only — it checks neither
routability nor the operator's enable/disable state (see U4/U5).

**F2. The stream was a test pattern — and the UI gave no way to avoid it.**
Every stream row in the DB tonight: `"source":{"mode":"test","device":null}`.
Root cause is worse than a bad default: the dashboard's Go Live modal
(`strata-dashboard/src/pages/sender_detail/tabs.rs:31-195`) has **no source
picker at all** and always sends `source: None`; the control plane then
substitutes its own default (`streams.rs:191-212`) — `mode: "test"` unless
the server env `STRATA_DEFAULT_SOURCE` says otherwise. The only device
picker lives in the Source tab, which is **disabled until a stream is
already live** (`tabs.rs:676-681`) — so the operator's only camera path is
"start a test pattern, then hot-switch", which is exactly what they tried
(→ F3). Nothing on the stream page says "you are broadcasting a test
pattern"; the metadata card even renders the test source's 1080p30 as if it
were the camera.

**F3. The camera attempt crashed on the wrong device node.**
21:49:52: `Error: Device '/dev/video1' is not a capture device` → pipeline
exit(1) after 7 s. On this rig `/dev/video1` is the USB camera's **metadata**
node (`Device Caps: Metadata Capture`); the capture node is `/dev/video0`
(MJPG or YUYV, 1080p up to 60 fps in MJPG; YUYV 1080p only 5 fps).
The agent's hardware scan (`crates/strata-sender/src/hardware.rs:204`) lists
`/dev/video*` without checking `V4L2_CAP_VIDEO_CAPTURE`, so the picker offers
non-capture nodes.

**F4. The dashboard interface toggle crashed the live pipeline.**
21:50:47: toggle `enP3p49s0` disable → pipeline printed
`property 'interface' of type 'GstPad' not found` and died (signal,
`exit_code=None`, 50 s into the stream).
Root cause `crates/strata-gst/src/bin/strata_pipeline/hotswap.rs:423`:
`handle_toggle_link` iterates **all** sink pads of stratasink and calls
`pad.property("interface")`; the muxer's static data pad has no such
property and gstreamer-rs **panics** on missing properties. Any toggle of an
interface that isn't a bonded link pad kills the stream. This is the
operator's "hot swap seems to restart the stream" — it's a crash, and the
UI showed it as nothing at all (see F5).

**F5. Crash reasons never reach the operator.**
`crates/strata-control/src/ws_agent.rs:447-477`: `StreamEnded` handling
discards `payload.reason` — the DB `error_message` stays NULL and the
dashboard event carries `error: None`. The 7-second video1 crash and the
toggle-link crash both rendered as ordinary "ended" streams. The reconciler
then killed the receiver-side halves 30 s later
("receiver running a stream the DB considers done"), producing more
state churn the operator couldn't interpret.

**F6. The remaining trickle stream couldn't feed HLS.**
With one dead pinned link + min-bitrate 3000 kbps forced against a ~20 kbps
delivered trickle, the receiver's egress watchdog rebuild-looped
(generations 4→6+, "no HLS segment for 15 s") — YouTube received unusable
fragments. Side finding: the sender's `link delivery-starved: crushing to
probe trickle` WARN logs at ~10 Hz — journal spam (`net/transport.rs`).

**Environment noise fixed during audit:** the Pi's timezone was
Asia/Shanghai (now Europe/London); the broken stream was stopped via the API
(HTTP 204, both sides confirmed down).

### This rig's devices (operator cheat-sheet)

| Device | What it actually is |
|---|---|
| `/dev/video0` | USB "FHD Camera" **capture** node — use this (MJPG 1080p30/60) |
| `/dev/video1` | same camera's metadata node — never usable |
| `/dev/video-enc0` / `-dec0` | RK3588 hardware codec nodes, not cameras |
| `enx001e101f0000` | modem 1 (HiLink, 192.168.8.100) |
| `eth0` | **modem 2** (HiLink that enumerated as eth0, 192.168.9.101) |
| `enP4p65s0` | onboard LAN, 192.168.50.55 — no internet route, must never carry a link |
| `enP3p49s0` | second onboard LAN port, unplugged |

---

## Part 2 — Findings (U-numbered, ranked)

### U1 (critical) — `toggle_link` panics the pipeline on non-link pads
`hotswap.rs:378-443`. Reads `pad.property("interface")` on every sink pad;
static data pad lacks it → panic → SIGABRT → stream dies. Fix: only inspect
request pads named `link_%u` (or check
`pad.has_property("interface")`-equivalent via `find_property` on the pad's
class before reading). Same latent hazard on the `uri` read at `:431`.

### U2 (critical) — stream end reasons are discarded at BOTH layers
Layer 1: `ws_agent.rs:447-477` maps every device-reported end — including
`PipelineCrash` — to `error: None` / DB NULL.
Layer 2: even for the reasons the control plane *does* populate (reconciler
"not running on sender", sweeper "sender unobserved", "stop timeout" —
`stream_state.rs:168-176/371-379/412-420`), the dashboard's event handler
destructures them away with `..`
(`strata-dashboard/src/pages/sender_detail.rs:257-262`), and neither
`StreamDetail.error_message` nor `ended_at` is rendered anywhere (streams
list has no reason column). When a stream dies, the live banner and cards
simply vanish and the header reverts to "Go Live" — no toast, no reason.
This converts every other bug into "I can't trust the dashboard".
Fix: persist `payload.reason` (+ optional detail) into `error_message`,
broadcast it, render it on the stream row and as a dismissible banner.
Note the deliberate readoption semantics in `stream_state.rs:112-117`
(inferred ends carry error_message, confirmed ends don't) — persist the
reason in a way that doesn't re-arm readoption, e.g. only treat
*reconciler-inferred* markers as readoptable rather than keying on
`error_message IS NOT NULL`.

### U3 (critical) — interface identity is a name-prefix guess
`hardware.rs:113-121`: `eth*`/`en*` → "Ethernet", `wwan*` → Cellular.
A HiLink USB modem enumerating as `eth0` is shown as Ethernet; carrier,
band, signal, technology are all hardcoded `None` (TODO ModemManager).
The operator cannot tell modem from LAN — which is precisely how the LAN
got left enabled and a modem got disabled tonight. Fix minimum: report
driver (`/sys/class/net/<if>/device/driver`), USB vs PCI bus, and the
IPv4 subnet; label HiLink subnets (192.168.8/9.x) as cellular. Fix proper:
ModemManager/NetworkManager probe.

### U4 (critical) — link pinning ignores routability and operator intent
`pipeline.rs:395-423` pins `Connected` interfaces sorted by name onto
destinations. Tonight that selected the routeless LAN and skipped modem 2.
Three compounding gaps:
 (a) no default-route/routability check;
 (b) the free `scan_network_interfaces()` hardcodes `enabled: true` —
     the HardwareScanner's admin map (dashboard toggles) is not threaded in;
 (c) the control plane counts links from the *filtered* heartbeat view
     (`streams.rs:126-131`) while the sender pins from the *unfiltered*
     scan — so link counts and pinned sets can disagree.
Fix: pin only interfaces with a default route (or a route to the receiver),
honor the admin map, and log the final mapping back to the control plane so
the dashboard can display which interface each link runs on.

### U5 (high) — "disable interface" is an in-memory fiction
`hardware.rs:67-78`: sets a HashMap flag, returns success, touches nothing
at OS level; effect on a live stream is only the (crashy, U1) toggle_link
pad removal, and the flag doesn't survive daemon restart nor influence the
next spawn (U4b). The REST handler is fire-and-forget — it returns
`{ok:true}` the moment the message is *enqueued* (`senders.rs:531-535`),
never awaiting the agent's `InterfaceCommandResponse`. The dashboard
checkbox does render heartbeat-confirmed state (it snaps to the next
`device.status`), but nothing tells the operator the toggle (a) did nothing
at OS level, (b) does not affect which interfaces the NEXT stream pins, and
(c) may have just crashed the current one (U1). Fix: await the ack, persist
the flag (file or control-plane DB), honor it at spawn, and caption the
toggle with its actual scope.

### U6 (high) — video device picker offers non-capture nodes
`hardware.rs:204` scans `/dev/video*` blind. Fix: keep only nodes with
`V4L2_CAP_VIDEO_CAPTURE` (open + `VIDIOC_QUERYCAP`, or parse
`v4l2-ctl --list-devices`), and carry the card name ("FHD Camera") so the
dashboard shows a human label instead of a bare path.

### U7 (medium) — no restart/continuity concept, so respawns look random
There is genuinely **no** kill-and-respawn path in the daemon (PATH A trace):
encoder bitrate/tune/keyint, bonding config, source switch, and link toggles
are true hotswaps over `/tmp/strata-pipeline.sock`; everything else
(resolution, codec, framerate, destinations) exists only in
`StreamStartPayload` and requires stop→start, which mints a **new
stream_id** with no link to the old one. Combined with U2, an operator sees
streams appear/vanish with no lineage. The 15 s `stopping` window
(`streams.rs:358-365,421-444`) also lets a start overlap a drain, adding
churn. Fix options: carry a `restarted_from` field on the new stream, or
add an explicit stream-level "apply requires restart" flow in the UI.

### U8 (medium) — placebo/zero metrics on the dashboard
`streams.total_bytes` is 0 for every stream ever (sender reports 0 in
`StreamEnded`; nothing accumulates it). Rendering a永-zero number erodes
trust. Either wire it (sum `bytes_sent` from stats) or remove the column
from the UI.

### U9 (low) — delivery-starved WARN spams at 10 Hz
`net/transport.rs` logs the crush warning every pacing tick while a link is
starved (hundreds of thousands of lines over a stalled night). Rate-limit
to state *transitions* (starved↔recovered) or once per N seconds.

### U10 (low) — timestamps and timezones
Pi was on Asia/Shanghai (fixed to Europe/London during audit). Dashboard
should render device `last_seen`/stream times in the browser's locale with
explicit UTC offsets so a mis-zoned device is visible, not confusing.

### Dashboard-specific findings (from the UI code audit)

### U11 (critical) — Go Live cannot start a camera
The start modal has only destination + codec radios; it always sends
`source: None` (`sender_detail.rs:326-353`, `api.rs:133-134`) and the
server defaults to the test pattern (`streams.rs:191-212`). The device
picker (Source tab) is disabled until a stream is live (`tabs.rs:676-681`).
Fix: source/device/resolution picker in the start modal, fed by the
(U6-filtered) device list from the sender's heartbeat, and a visible
"TEST PATTERN" badge on the live banner when mode=test.

### U12 (high) — fire-and-forget actions report success
`switch_source` (`senders.rs:749-776`) and `interface_command`
(`senders.rs:475-536`) return 200 on enqueue; the UI then shows
"Source switched successfully" (`tabs.rs:664-670`). Tonight the operator's
camera switch to /dev/video1 crashed the pipeline seconds after the UI said
success. Contrast: `update_stream_config`/jitter/destinations DO await agent
acks — the confirmation semantics are inconsistent across visually identical
Apply buttons. Fix: route all agent-bound commands through the acked
request_id path.

### U13 (high) — placebo state in the UI
- Multi-Dest Routing "Active" badges render a client-local vector that is
  never loaded from the server (`cards.rs:1004,1024-1045`) — pure placebo.
- `has_role()` returns `true` unconditionally (`lib.rs:69-71`) — every
  role-based `disabled` guard in the UI is cosmetic.
- Sidebar "Live" badge is driven by WS open/close only (`ws.rs:118,131`);
  an auth-rejected socket still shows "Live" while delivering nothing
  (`ws.rs:161-168` logs to console only).
- `streams.total_bytes` / "Est. Latency" labeling (see U8).

### U14 (medium) — stale-data rendering
Heartbeat-merged fields persist indefinitely when a partial `device.status`
omits them (`helpers.rs:15-32`); CPU/RAM/interface cards have no
last-updated age. Only live stream stats have a staleness heuristic (the
client-invented 5 s `signal_lost`). Fix: age indicators on device-sourced
cards; grey-out after N missed heartbeats.

### U15 (medium) — one catch-all error banner
Start failures, unenroll, interface and test errors all funnel into a
single top-of-page banner (`sender_detail.rs:458-463`) detached from the
control that failed. Alert-rule cards refresh on a fixed 500 ms timer
instead of on server confirmation (`cards.rs:564-594`).

### U16 (low) — naming traps
"Destinations" page is RTMP/HLS/SRT egress, not relays — an operator
looking for "add my relay" lands there. Bonding link rows fall back to
"Link {id}" when the interface name is missing (`tabs.rs:367-371`), so a
struggling link often can't be mapped to a physical modem. Timestamps are
hard-coded UTC strings (`streams.rs:98`, `tabs.rs:1560-1564`).

---

## Part 3 — "Are we set up to register new relays as well as new senders, and unique config of each?"

**Backend: yes. UI: senders yes, receivers no. Per-device config: thin.**

- Senders: `POST /api/senders` (admin) → one-time `<id>.<secret>` token →
  daemon enrolls over `/agent/ws`, ed25519 key bound, token consumed.
  Dashboard has a senders page.
- Receivers (relays): identical flow via `POST /api/receivers` (operator) and
  `/receiver/ws` — **multiple receivers are fully supported** and stream
  starts pick the least-loaded online receiver owned by the user
  (`streams.rs:538 pick_receiver`), request/ack port allocation included.
  **But there is no receivers page in the dashboard** — registration is
  curl-only, and there is no way to see relay fleet state, pick a specific
  relay for a stream, or pin by region.
- Unique config per device: senders store only `name`; receivers store
  `bind_host/link_ports/max_streams/region` but the **daemon's CLI flags
  overwrite them on every connect** (`ws_receiver.rs:187-213`) — the control
  plane is not the source of truth. `bonding_config` in every stream start is
  hardcoded `null` (`streams.rs:249,590`). Runtime commands (APN, band lock,
  priority, jitter buffer, interface toggles) are not persisted anywhere and
  vanish on daemon restart.

Gap list an operator will hit next: receiver UI (register + list + health),
per-sender camera/source defaults, persistent interface roles
(this-is-a-modem / never-use-this-LAN), receiver selection, central bonding
config, editable device names.

---

## Part 4 — Recommended fix plan (priority order)

**P0 — stop lying to the operator (small, surgical):**
1. U1 toggle_link panic guard.
2. U2 persist + display stream end reasons (both layers).
3. U11 source/device picker in the Go Live modal + TEST-PATTERN badge.
4. U12 make switch_source + interface commands await agent acks.
5. U6 filter video devices to capture-capable, show card names.

**P1 — make link selection safe:**
6. U4 pin only routable interfaces, honor the admin map, report the final
   link→interface mapping to the dashboard.
7. U3 interface metadata: driver/bus/subnet at minimum.
8. U5 persist interface enable/disable; caption its real scope.

**P2 — coherence & polish:**
9. U13 remove/wire placebo state (multi-dest badges, has_role, Live badge).
10. U7 stream lineage (`restarted_from`) or explicit restart flow.
11. U8 wire or drop total_bytes; U14 staleness ages; U15 scoped errors.
12. U9 rate-limit starvation WARN. U10/U16 timezone + naming cleanups.

**Deliberately NOT recommended:** re-architecting. The envelope protocol,
enrollment/auth, stream-state reconciler, port-allocation request/ack, and
the bonded transport all behaved exactly as designed tonight — the
reconciler even correctly cleaned up orphaned receiver halves. The failures
were in the *edges* (device identification, pad property access, discarded
error fields, UI defaults), which is normal first-contact-with-a-real-user
territory, and each fix above is local.

Related open transport findings (pre-existing, tracked in hot.md): a
blackholed link is never marked dead (no-ACK-based death wanted); capacity
estimate pins at `capacity_floor_bps` at low traffic volume.
