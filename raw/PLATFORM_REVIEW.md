# Strata Platform — Top-Down Architecture Review

**Date:** 2026-07-01 (Fable 5)
**Scope:** The management plane and web: `strata-common`, `strata-control`,
`strata-sender`, `strata-receiver` (daemon), `strata-dashboard`,
`strata-portal`. The transport stack is covered separately in
[review_findings.md](review_findings.md); this document is about the plane
that starts, stops, observes and configures it.

> **Implementation status (2026-07-04): ALL executive items are done and
> merged to `main`.** 2026-07-02 landed E5, E7 (SQL bug + stop-orphan),
> E10, E3, E9; 2026-07-04 landed E1 (protocol crate, commit 3422861), E2
> (state machine + reconciliation, a2b2f67), E4 (device identity,
> 8b6c04a), and E6 + the rest of E7 + E8 (e8eb5a9). The only deliberately
> open flag is E3's `CorsLayer::permissive()`/unauthenticated-`/metrics`
> production-posture decision.

---

## 0. Verdict

The platform is a competent **demo-grade** control plane: clean crate split,
sensible axum/sqlx/Leptos choices, readable code, good docs. What makes it
lackluster is not any one file — it's that four architectural properties were
never established, and every gap below is a symptom of one of them:

1. ✅ FIXED 2026-07-04 (E1) — **No single source of truth for the protocol.**
   The message schema existed three times; now the wasm-safe
   `strata-protocol` crate is the only definition site, and every
   hub/daemon dispatches exhaustively on direction enums.
2. ✅ FIXED 2026-07-04 (E2) — **No reconciliation — events only.**
   Heartbeats now carry `running_streams`; the control plane reconciles on
   every heartbeat, readopts inferred-dead streams, and a WS drop is
   "unobserved", never "dead".
3. ✅ FIXED 2026-07-02/04 (E3 + E4) — **Security is declared, not
   enforced.** Dashboard WS auth + owner scoping (E3); one-time composite
   enrollment tokens, single argon2 verify, and ed25519 challenge
   reconnect auth (E4).
4. ✅ FIXED 2026-07-02 (E5) — **The plane reaches into the transport's tuning.** The control plane
   hardcodes a bonding config that silently reverses field-validated
   transport defaults — the exact cross-layer override failure class the
   transport audit keeps finding inside the bonding crate.

None of this needs a rewrite. It needs ~10 deliberate, ordered changes.
Properties 1-2 (protocol, reconciliation) are still unaddressed; property 3
(security) is partially addressed (E3 done, E4 not started) as of
2026-07-02 — see the status markers on E1/E2/E3/E4 below.

---

## 1. Executive change list (ranked)

### E1. ✅ DONE 2026-07-04 — One protocol crate, one dispatch path

Today the protocol lives in three places that can drift independently:

- `strata-common::protocol` defines `AgentMessage`/`ControlMessage` enums —
  but they cover only **8 of ~28** message types; the other 20 are loose
  payload structs with no enum variant.
- Every hub and daemon dispatches on **raw strings**
  (`match envelope.msg_type.as_str()` — [ws_agent.rs:253](crates/strata-control/src/ws_agent.rs#L253)
  has a 17-arm string list just for RPC responses; the enums are decorative).
- The dashboard and portal **do not depend on strata-common at all** — they
  hand-copy 41 struct/enum definitions into `types.rs`. A field rename in
  strata-common compiles everywhere and silently breaks the UI.

**Change:** extract a wasm-safe `strata-protocol` crate (serde types only, no
argon2/tokio deps — that dependency weight is presumably why the WASM apps
copied types). Every message becomes an enum variant; hubs and daemons match
exhaustively on the enum so adding a message is a compile error until every
site handles it; dashboard/portal delete `types.rs` and import the crate. Add
`proto_version` to the envelope while you're in there (agents already send
`agent_version`; nothing reads it) — the first schema evolution will otherwise
be a fleet-wide flag day.

**Status (2026-07-04, commit 3422861):** done as specified. New wasm-safe
`strata-protocol` crate (envelope + every payload + four direction enums +
DashboardEvent + shared REST api types + models + profiles); all four
dispatch sites match exhaustively; sends go through
`Envelope::from_message`; `proto_version` added (default 1, hubs warn on
mismatch); dashboard deleted `types.rs` (dead placebo UI removed with it:
NAL counters card, FEC-layer/BLEST/fec_overhead_percent knobs — none ever
had a server-side producer/consumer); verified for wasm32.

### E2. ✅ DONE 2026-07-04 — Reconciliation over guessing: a real stream state machine

Stream state is mutated by **seven** independent sites, all via raw SQL string
states: REST start (`'starting'`), first-stats inference (`'starting'→'live'`,
[ws_agent.rs:284](crates/strata-control/src/ws_agent.rs#L284)), REST stop
(`'stopping'`), a 15 s force-end timer ([streams.rs:386](crates/strata-control/src/api/streams.rs#L386)),
`stream.ended`, agent-disconnect orphaning, and receiver-disconnect orphaning.
There is no transition validation, and three parallel copies of "what's live":
the DB `state` column, the `live_streams` DashSet, and the receivers'
`active_streams` counter — each updated in different files.

The deepest fallacy: **a WS drop is treated as stream death** (both hubs).
A control-plane restart or a 30 s connectivity blip marks every live stream
`'ended'` while the actual media pipelines keep running — the media path
doesn't even touch the control plane. Reality and the DB then diverge
permanently, because nothing resyncs on reconnect: the agent's heartbeat
reports only a binary `Live`/`Idle` with **no stream_id**, and neither
connect handler asks "what are you actually running?"

**Change:** one module owns stream transitions (validated state machine,
single `transition(stream_id, from, to)` function). `device.status` and the
receiver heartbeat carry the set of running stream IDs; on (re)connect the
control plane reconciles — re-adopting streams that are still running,
ending the ones that aren't — instead of orphan-marking on disconnect.
WS-drop then only means "unobserved", never "dead".

**Status (2026-07-04, commit a2b2f67):** done. `stream_state.rs` owns
every `streams.state` write (validated `transition()`, reconcile-only
`readopt()`); heartbeats carry `running_streams` and both hubs reconcile
on every heartbeat (readopt only inferred ends — user-confirmed ends are
enforced by re-sending stop); disconnect orphaning deleted; a 30 s sweeper
ends streams whose sender is unobserved > 90 s and force-ends stale
'stopping' rows. Four integration tests cover WS-drop survival,
reconcile-end, readopt-vs-enforce, and transition validation.

### E3. ✅ DONE (core fix); CORS/`/metrics` flagged, not changed — Authenticate and scope the dashboard WebSocket

`GET /ws` ([ws_dashboard.rs](crates/strata-control/src/ws_dashboard.rs)) has
**no authentication of any kind** — no extractor, no middleware, the handler
immediately ships a snapshot of every sender, every stream, then subscribes
the socket to the global broadcast. The client dutifully appends `?token=` to
the URL ([dashboard ws.rs:60](crates/strata-dashboard/src/ws.rs#L60)); the
server never reads it. Two distinct bugs:

- Anyone who can reach port 3000 gets live fleet telemetry.
- Even with auth, `DashboardEvent` carries no owner and the broadcast channel
  is global — **every operator would receive every other operator's events**,
  directly contradicting the wiki's "no user can see another user's
  resources" security model.

**Change:** verify the JWT at upgrade (first-message auth, like the agent WS —
tokens in URLs end up in proxy logs), resolve the owner, and filter events
per-connection by owner (attach `owner_id` to `DashboardEvent` or key
broadcast channels per owner). Also: `CorsLayer::permissive()` and the
unauthenticated `/metrics` on the same listener deserve a production posture
decision at the same time.

**Status (2026-07-02, done same day):** both bugs fixed. `ws_dashboard.rs`
now requires an `auth.login` first message (a JWT bearer token, mirroring
`ws_agent.rs`/`ws_receiver.rs`'s handshake exactly) before sending anything;
rejects device-role tokens (`claims.owner.is_some()`) so only real user
sessions can open the feed. `AppState::dashboard_tx` now carries
`(owner_id, DashboardEvent)` tuples — every `broadcast_dashboard` call site
(`ws_agent.rs`, `ws_receiver.rs`, `api/streams.rs`) was updated to supply the
owning user's ID, and `ws_dashboard.rs` filters its subscription to the
connected user's own `owner_id` before forwarding anything to the browser;
the initial snapshot query is scoped the same way. The dashboard client
(`strata-dashboard/src/ws.rs`) now sends the token as the first WS message
instead of a `?token=` query param. Two new integration tests
(`dashboard_ws_scopes_events_to_owner`, `dashboard_ws_rejects_invalid_token`
in `crates/strata-control/tests/api_integration.rs`, using a real
`tokio_tungstenite` client against a real `TcpListener` — WS upgrades can't
be exercised through axum's oneshot tower-service testing) prove a second
owner's event never reaches the first owner's socket, and that an invalid
token gets rejected and the connection closed. `CorsLayer::permissive()` and
the unauthenticated `/metrics` endpoint (`main.rs`) were **not** touched —
per this finding's own instruction to flag them for a deliberate posture
decision rather than silently change deployment-facing behavior.

### E4. ✅ DONE 2026-07-04 — Real device identity (the current one is a placeholder in disguise)

- Device-key auth is `TODO` on **both** sides ([ws_agent.rs:173](crates/strata-control/src/ws_agent.rs#L173),
  [sender control.rs:130](crates/strata-sender/src/control.rs#L130)); the
  wiki's "authenticates via ed25519 device keypair" and the dashboard's
  "one-time enrollment token" are both currently false — the enrollment token
  is deliberately kept valid forever as the reconnect credential
  (comment at ws_agent.rs:188).
- Authentication is an **O(n·argon2) scan**: every connect fetches *all*
  senders and runs argon2 verification against each row until one matches.
  Argon2 is designed to be slow; at fleet size this is seconds of CPU per
  reconnect, and it's an unauthenticated CPU-DoS endpoint (each garbage
  connect burns n argon2 verifications). Reconnect storms after a control
  restart multiply it.
- Agents/receivers are issued a 1-hour session JWT that **nothing ever
  verifies** — connection identity is the socket itself; the token is
  decorative.

**Change:** split the enrollment token into `id.secret` (lookup by id, one
argon2 verify), make it genuinely one-time: on first enrollment the agent
submits its ed25519 public key, and reconnects authenticate by signature
challenge (the keygen code already exists in `strata-common::auth`). Delete
the decorative session token or actually use it.

**Status (2026-07-04, commit 8b6c04a):** done as specified, for both
senders and receivers (migration 003 adds the receivers' pubkey column).
Composite tokens issued by create/unenroll; token consumed when a key is
bound (keyless legacy enrollment still works but warns loudly); daemons
persist identity (keypair 0600 + device id) before spending the token and
fail fast if the file is unwritable; the decorative session JWT is
deleted end-to-end. Integration tests prove single-use and wrong-key
rejection.

### E5. ✅ DONE — The control plane must stop overriding transport tuning

[streams.rs:226-235](crates/strata-control/src/api/streams.rs#L226) hardcodes
the `bonding_config` sent with every managed stream:

```json
{ "critical_broadcast": true, "redundancy_enabled": true,
  "capacity_floor_bps": 5000000.0, "failover_enabled": true, ... }
```

The bonding crate's defaults set `redundancy_enabled: false` and
`critical_broadcast: false` **deliberately, after field incidents** — the
config comment says they "make bursty congestion worse (doubling the offered
load right when a link is marginal)". The web plane silently re-enables both
for every platform-started stream, and pins `capacity_floor_bps` to 5 Mbps
against the tuned 1.5 Mbps default. Every field lesson encoded in
`SchedulerConfig::default()` is invisible to platform streams. (This also
explains why CLAUDE.md believes the floor is 5 Mbps.) Same class of issue one
line up: the receiver playout buffer is pinned via a URL string —
`strata://{addr}?buffer=2000` — overriding `ReceiverConfig`'s tuned 1500 ms.

**Change:** the control plane sends *named profiles* (or nothing — let
defaults rule) and the bonding crate owns what a profile means. Any explicit
override should be a versioned, reviewed artifact (DB row or checked-in file),
not a `json!` literal in a REST handler. This is the platform-side twin of
the config-centralization findings in review_findings.md Part 3.

**Status (2026-07-02):** done, the "nothing — let defaults rule" half only.
Deleted the hardcoded `bonding_config` JSON literal and the
`?buffer=2000` URL override; both now fall through to
`SchedulerConfig::default()`/`ReceiverConfig`'s tuned values. The "named
profiles" mechanism itself (a versioned, reviewed way to *explicitly*
override per-stream) was **not** built — out of scope for a fix, since
the REST API has no override mechanism to plug into today; that's a
separate feature if/when it's wanted.

### E6. ✅ DONE 2026-07-04 — Per-stream port allocation (make `max_streams` true or delete it)

`pick_receiver_links` hands **every** stream the receiver's *entire*
`link_ports` list, and `receiver.stream.start` tells the receiver to bind
those same ports. Two concurrent streams on one receiver collide on every
port. The schema, capacity query (`active_streams < max_streams`) and
heartbeats all pretend multi-stream receivers exist; the port model supports
exactly one. **Change:** the receiver owns a port pool (it already has one
for stats ports); `receiver.stream.start` becomes request/ack — control asks,
receiver replies with the allocated ports, control forwards them to the
sender. Until then, set `max_streams = 1` everywhere so the fiction is at
least consistent.

**Status (2026-07-04, commit e8eb5a9):** done — request/ack exactly as
described. `receiver.stream.start` carries {request_id, link_count}; the
receiver allocates from its own PortPool (which previously only released),
enforces max_streams, and answers `receiver.stream.started` with the
bound ports; the control plane builds the sender's destinations from the
ack.

### E7. ✅ DONE (2026-07-02 + 2026-07-04) — Make start/stop transactional sagas (and fix the stop-path orphan)

Concrete holes in the current sequences, all confirmed by reading:

- ✅ DONE (2026-07-02) — **Stop never notifies the receiver.** `receiver.stream.stop` has a fully
  implemented handler in the receiver daemon and **zero senders** in the
  control plane. Stopping a stream leaves the receiver pipeline running
  (a UDP listener doesn't EOS when the sender stops) and `active_streams`
  is never decremented on the normal path — the capacity-aware assignment
  degrades monotonically until the receiver reconnects.
- ✅ DONE 2026-07-04 — **Start is non-atomic with a partial rollback**: DB insert → receiver
  command → counter increment → agent send; if the agent send fails, the
  rollback marks the row ended but does not stop the receiver or decrement
  the counter.
- ✅ DONE 2026-07-04 (capacity now COUNT(*)-derived; column is display-only) — **`active_streams` is a hand-maintained counter** (increment in streams.rs,
  decrement in ws_receiver.rs, reset-to-0 on disconnect) — it will drift;
  it should be `COUNT(*)` over streams, or at least reconciled by E2.
  (Fixing the stop-notify bug above means the decrement handler now
  actually fires on the normal path, closing the most common drift source
  — but the counter itself is still hand-maintained, not derived, so E2's
  reconciliation is still the real fix.)
- ✅ DONE (2026-07-02) — **Likely hard bug:** the concurrent-stream guard
  ([streams.rs:72-79](crates/strata-control/src/api/streams.rs#L72)) binds
  **two** parameters to a query with **one** placeholder — with sqlx/Postgres
  that's a runtime error on every call, which would 500 every platform
  stream-start. If platform starts currently work, verify why; if they don't,
  this is the smoking gun. Either way the second `.bind` must go.
  Confirmed it was the smoking gun; regression test added.
- ✅ DONE (2026-07-02) — Minor: if a sender reports 0 connected interfaces, `link_count` becomes 0
  and the stream starts with an empty destination list — guard it.

### E8. ✅ DONE 2026-07-04 — Receiver-side telemetry is discarded — surface it

`receiver.stream.stats` arrives at the control plane and is **dropped at
trace level** ([ws_receiver.rs:275-285](crates/strata-control/src/ws_receiver.rs#L275)).
The entire transport saga on this branch (goodput vs residual, late-rate,
AQM) established that *receiver-side* measurements are the ground truth, and
the platform throws them away — the dashboard shows only sender-side stats.
Add a `DashboardEvent::ReceiverStreamStats` (trivial once E1 lands) and
render both sides; disagreements between them are exactly the diagnostic the
field runs keep needing.

**Status (2026-07-04, commit e8eb5a9):** done — owner-scoped
`DashboardEvent::ReceiverStreamStats` broadcast from the receiver hub, and
the dashboard's Stream tab renders a "Receiver-Side Links" table beside
the sender-side view.

### E9. ✅ DONE — Platform timing/constants hygiene pass

The plane has its own magic-number sprawl, in the same shapes the transport
audit flagged: heartbeat 10 s (CLI default), reconnect backoff 1→30 s
(agent + receiver, duplicated code), dashboard reconnect **fixed 3 s with no
jitter** (a control restart makes every browser and every device reconnect in
lockstep — a thundering herd against the O(n·argon2) auth of E4), mpsc
channel 64 (send-fails on a slow consumer are silently dropped commands —
`let _ = tx.send(...)` in several places), broadcast 1024, stop force-end
15 s, monitor poll 500 ms, JWT expiry 3600 s (login just dies after an hour —
no refresh; a broadcast operator mid-stream gets logged out), fallback ports
5000/5002/5004, stats ports 9200+. Collect them into one documented config
module per crate with the same rigor as `net/transport.rs`'s named-const
block, and add jitter to every reconnect loop.

**Status (2026-07-02, done same day):** every literal named at its site
(not a shared cross-crate module — "one documented config module per
crate" per this finding's own wording, so `strata-common`/`strata-control`/
`strata-sender`/`strata-receiver`/`strata-dashboard` each keep their own
consts rather than a new dependency edge between them): JWT expiry →
`strata_common::auth::SESSION_TOKEN_TTL_SECS`; reconnect backoff →
`INITIAL_BACKOFF`/`MAX_BACKOFF` in both sender/receiver `control.rs`;
channel capacities → `DASHBOARD_BROADCAST_CAPACITY`,
`AGENT_COMMAND_CHANNEL_CAPACITY`/`RECEIVER_COMMAND_CHANNEL_CAPACITY` (64)
vs `CONTROL_OUTGOING_CHANNEL_CAPACITY` (128, the 64-vs-128 mismatch
flagged, not silently unified); stop force-end → `STOP_FORCE_END_TIMEOUT`;
monitor poll → `MONITOR_POLL_INTERVAL`; fallback ports →
`FALLBACK_RECEIVER_PORTS`. The one behavior change: jitter added to every
reconnect loop (agent, receiver, dashboard) — ±20% via `rand` natively,
`js_sys::Math::random()` in the WASM dashboard client (avoids `rand`'s
getrandom/wasm32 backend complications). The 3 silently-dropped command
sends actually named in this finding (`receiver.stream.start`/
`stream.stop`/`receiver.stream.stop` in `api/streams.rs`) now log a
warning on failure; other `let _ = ...send(...)` sites (best-effort
broadcasts, auth-error responses, watch-channel signals) were left as-is —
not the "command drops" this finding names.

### E10. ✅ DONE — Decide what the portal is

`strata-portal` duplicates a third of the dashboard (system stats, interface
management, enrollment, config) against a *different* API surface
(agent-local HTTP on :3001) with its own hand-copied types. After E1, most of
its remaining body is a worse copy of dashboard components. Either commit to
it as the offline-first field tool (then it needs the device-key identity
from E4 and a defined local API contract), or fold its unique pieces
(enrollment, local diagnostics) into a served-by-the-agent page and retire
the crate. Keeping both without a decision is how the sbd/Thompson-sampling
"documented but dead" pattern starts.

**Status (2026-07-02):** decided and done — user chose outright retirement
(not the fold-into-agent-page option, which would have been a new feature).
`strata-portal` deleted along with its workspace membership, `portal-dev`
compose service, and CI step. **Real gap surfaced, not yet decided:**
`strata-sender/src/portal.rs` (the local HTTP server on `:3001`) served
this crate's built assets for on-device enrollment/diagnostics and now has
nothing to serve — it won't crash, but that UI is non-functional until a
follow-up decision is made (static page, fold into the agent, or
something else).

---

## 2. Cross-cutting observations

**Two sources of truth everywhere.** Beyond streams (E2): receiver liveness is
both a DB `online` flag and the in-memory `receivers()` map — `pick_receiver_links`
consults both and silently falls back to env vars when they disagree; sender
online-ness is the `agents()` map but `last_seen_at` is also maintained;
device status lives in a DashMap cache keyed by messages that are never
sequence-checked. Every pair needs an owner and a derivation rule.

**Error handling conventions are demo-grade.** `let _ =` on nearly every
DB write and channel send in the hubs (a failed `UPDATE streams SET state=...`
is silently swallowed — the state machine of E2 must not inherit that);
`serde_json::to_string(...).unwrap()` on every outgoing message;
`Envelope::new` panics on serialization failure while `try_new` sits unused.

**The security model doc is now substantially true — resolved 2026-07-04.**
Owner isolation is enforced in the REST layer and on the dashboard WS
(E3); device identity (E4) now implements the doc: one-time enrollment
tokens, ed25519 device keys, challenge-response reconnect auth. Remaining
gap: the deliberately-flagged CORS/`/metrics` production posture decision.

**What's fine (leave it alone):** the crate boundaries themselves; axum +
sqlx + migrations; DashMap-based hubs; UUIDv7 prefixed IDs; the enrollment
UX; Leptos CSR + Trunk for the two WASM apps; the dev-seed loop. The
foundations are right — the discipline layers (protocol, reconciliation,
identity, config ownership) were skipped, and they're each a bounded piece of
work.

---

## 3. Suggested sequencing

Status column updated 2026-07-04: **everything is done.** The remaining
items landed in dependency order on 2026-07-04 — E1 first (unblocking
E2/E8), then E2, E4, and E6+E7-rest+E8 together.

| Status | Order | Item | Why first |
|---|---|---|---|
| ✅ DONE 2026-07-02 | 1 | E7's SQL bind bug + stop-path receiver orphan | small, likely user-visible today |
| ✅ DONE 2026-07-02 | 2 | E3 dashboard WS auth + scoping | exposed surface, small fix |
| ✅ DONE 2026-07-04 | 3 | E1 protocol crate | unblocks E2/E8 cheaply, deletes 41-type copy |
| ✅ DONE 2026-07-04 | 4 | E2 state machine + reconciliation | biggest correctness win |
| ✅ DONE 2026-07-02 | 5 | E5 bonding-profile ownership | protects the transport tuning investment |
| ✅ DONE 2026-07-04 | 6 | E4 device identity | before any real fleet exists |
| ✅ DONE 2026-07-04 | 7 | E6 port allocation (+ rest of E7) | before multi-stream receivers are attempted |
| ✅ DONE (E9/E10 2026-07-02, E8 2026-07-04) | 8 | E8, E9, E10 | quality-of-life, in any order |
