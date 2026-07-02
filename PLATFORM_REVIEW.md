# Strata Platform — Top-Down Architecture Review

**Date:** 2026-07-01 (Fable 5)
**Scope:** The management plane and web: `strata-common`, `strata-control`,
`strata-sender`, `strata-receiver` (daemon), `strata-dashboard`,
`strata-portal`. The transport stack is covered separately in
[review_findings.md](review_findings.md); this document is about the plane
that starts, stops, observes and configures it.

> **Implementation status (2026-07-02):** E5, E7, and E10 are **done and
> merged to `main`**. E3 (dashboard WS auth), E9 (timing/jitter hygiene),
> E1 (protocol crate), E2 (state machine), E4 (device identity), E6 (port
> allocation), E8 (receiver telemetry) are **not started**. See
> `.claude/plans/rosy-squishing-treasure.md` for the intended order and
> notes on why E1/E2/E4 in particular were held back rather than rushed.

---

## 0. Verdict

The platform is a competent **demo-grade** control plane: clean crate split,
sensible axum/sqlx/Leptos choices, readable code, good docs. What makes it
lackluster is not any one file — it's that four architectural properties were
never established, and every gap below is a symptom of one of them:

1. **No single source of truth for the protocol.** The message schema exists
   three times (typed enums in `strata-common`, stringly-typed dispatch in
   every hub, and 41 hand-copied types in the dashboard), so the compiler
   can't defend the wire format.
2. **No reconciliation — events only.** Every state change is an edge-triggered
   event with no snapshot/resync path. Any missed event (a WS blip, a control
   restart) permanently desyncs the DB from reality, and the code "handles"
   this by guessing (orphan-marking streams on disconnect).
3. **Security is declared, not enforced.** The wiki's security model
   (per-owner isolation, ed25519 device auth, one-time tokens) is
   substantially unimplemented: the dashboard WS is unauthenticated and
   unscoped, device-key auth is a TODO, enrollment tokens are permanent
   reusable passwords.
4. ✅ FIXED 2026-07-02 (E5) — **The plane reaches into the transport's tuning.** The control plane
   hardcodes a bonding config that silently reverses field-validated
   transport defaults — the exact cross-layer override failure class the
   transport audit keeps finding inside the bonding crate.

None of this needs a rewrite. It needs ~10 deliberate, ordered changes.
Properties 1-3 (protocol, reconciliation, security) are still unaddressed
as of 2026-07-02 — see the status markers on E1/E2/E3/E4 below.

---

## 1. Executive change list (ranked)

### E1. ⬜ NOT STARTED — One protocol crate, one dispatch path

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

### E2. ⬜ NOT STARTED — Reconciliation over guessing: a real stream state machine

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

### E3. ⬜ NOT STARTED — Authenticate and scope the dashboard WebSocket

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

**Status (2026-07-02):** not started — the agent assigned to this made no
progress before an account-level usage limit cut it off; still fully
unauthenticated and unscoped on `main` as described above.

### E4. ⬜ NOT STARTED — Real device identity (the current one is a placeholder in disguise)

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

### E6. ⬜ NOT STARTED — Per-stream port allocation (make `max_streams` true or delete it)

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

### E7. 🟡 PARTIALLY DONE — Make start/stop transactional sagas (and fix the stop-path orphan)

Concrete holes in the current sequences, all confirmed by reading:

- ✅ DONE (2026-07-02) — **Stop never notifies the receiver.** `receiver.stream.stop` has a fully
  implemented handler in the receiver daemon and **zero senders** in the
  control plane. Stopping a stream leaves the receiver pipeline running
  (a UDP listener doesn't EOS when the sender stops) and `active_streams`
  is never decremented on the normal path — the capacity-aware assignment
  degrades monotonically until the receiver reconnects.
- ⬜ NOT STARTED — **Start is non-atomic with a partial rollback**: DB insert → receiver
  command → counter increment → agent send; if the agent send fails, the
  rollback marks the row ended but does not stop the receiver or decrement
  the counter.
- 🟡 MITIGATED, NOT FIXED — **`active_streams` is a hand-maintained counter** (increment in streams.rs,
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

### E8. ⬜ NOT STARTED — Receiver-side telemetry is discarded — surface it

`receiver.stream.stats` arrives at the control plane and is **dropped at
trace level** ([ws_receiver.rs:275-285](crates/strata-control/src/ws_receiver.rs#L275)).
The entire transport saga on this branch (goodput vs residual, late-rate,
AQM) established that *receiver-side* measurements are the ground truth, and
the platform throws them away — the dashboard shows only sender-side stats.
Add a `DashboardEvent::ReceiverStreamStats` (trivial once E1 lands) and
render both sides; disagreements between them are exactly the diagnostic the
field runs keep needing.

### E9. ⬜ NOT STARTED — Platform timing/constants hygiene pass

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

**Status (2026-07-02):** not started — the agent assigned to this made no
progress before an account-level usage limit cut it off.

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

**The security model doc should be re-titled "target state".** Owner
isolation is genuinely enforced in the REST layer (consistent
`owner_id`-scoped queries — good), but the WS surfaces (E3) and device
identity (E4) don't implement the doc. Until they do, the wiki overstating
the live system is the same credibility problem the 2026-05-29 transport
review called out.

**What's fine (leave it alone):** the crate boundaries themselves; axum +
sqlx + migrations; DashMap-based hubs; UUIDv7 prefixed IDs; the enrollment
UX; Leptos CSR + Trunk for the two WASM apps; the dev-seed loop. The
foundations are right — the discipline layers (protocol, reconciliation,
identity, config ownership) were skipped, and they're each a bounded piece of
work.

---

## 3. Suggested sequencing

Status column added 2026-07-02 — the actual order landed 1 (partially,
just the SQL bug + orphan, not the rest of E7's sagas), 5, and 8's E10
only; everything else is unstarted, including E3 despite being ranked #2.

| Status | Order | Item | Why first |
|---|---|---|---|
| ✅ DONE (SQL bug + orphan only; rest of E7 open) | 1 | E7's SQL bind bug + stop-path receiver orphan | small, likely user-visible today |
| ⬜ NOT STARTED | 2 | E3 dashboard WS auth + scoping | exposed surface, small fix |
| ⬜ NOT STARTED | 3 | E1 protocol crate | unblocks E2/E8 cheaply, deletes 41-type copy |
| ⬜ NOT STARTED | 4 | E2 state machine + reconciliation | biggest correctness win |
| ✅ DONE | 5 | E5 bonding-profile ownership | protects the transport tuning investment |
| ⬜ NOT STARTED | 6 | E4 device identity | before any real fleet exists |
| ⬜ NOT STARTED | 7 | E6 port allocation | before multi-stream receivers are attempted |
| 🟡 E10 done, E8/E9 not started | 8 | E8, E9, E10 | quality-of-life, in any order |
