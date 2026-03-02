# Strata — New Crate Structure

## Naming principle

Every binary name answers: "what does this process **do** right now?"

```
strata-sender      daemon on a field device    — manages sending
strata-receiver    daemon on a cloud server    — manages receiving
strata-pipeline    GStreamer pipeline runner   — sender | receiver mode
strata-control     cloud control plane        — manages the fleet
strata-probe       raw transport diagnostic   — no GStreamer, dev/test only
```

No "agent", no "node", no "gateway".

---

## Binary map

| Binary | Role | Host |
|---|---|---|
| `strata-sender` | Field daemon. Connects to control plane, reports hardware, spawns `strata-pipeline sender` per stream, relays telemetry back. Hosts local portal UI for on-site enrollment. | Field device (cellular, ARM64) |
| `strata-receiver` | Cloud daemon. Connects to control plane, registers capacity/region, spawns `strata-pipeline receiver` per assigned stream, relays receiver stats back. | VPS / cloud server |
| `strata-pipeline sender` | GStreamer pipeline. Encodes video, bonds across N links via `stratasink`. Reports stats via UDP to the daemon that spawned it. | Child process of strata-sender |
| `strata-pipeline receiver` | GStreamer pipeline. Receives across N links via `stratasrc`, reassembles, outputs to RTMP/HLS/file. Reports stats via UDP to its spawning daemon. | Child process of strata-receiver |
| `strata-control` | REST API + WebSocket hub + PostgreSQL. Commands both sender and receiver daemons. | Cloud, single instance |
| `strata-probe` | Transport-layer diagnostic. Raw UDP reassembly, no GStreamer, no relay. For CI and field fault-finding. | Anywhere |

---

## Crate structure

```
crates/
  strata-transport/   Wire protocol — FEC, ARQ, Biscay CC (library, no runtime dep)
  strata-bonding/     Bonding engine — DWRR/IoDS/BLEST scheduler, modem supervisor
                        └── bin/strata-probe   (diagnostic tool)
  strata-gst/         GStreamer plugin (stratasink / stratasrc)
                        └── bin/strata-pipeline  (sender | receiver modes)
  strata-common/      Shared types, WS protocol envelopes, JWT, IDs
  strata-control/     Cloud control plane — Axum REST + WS hubs + PostgreSQL
  strata-sender/      Field sender daemon
                        └── portal/   served locally on :3001 for enrollment
  strata-receiver/    Cloud receiver daemon  ← to be created
  strata-dashboard/   Operator web UI (Leptos WASM, served by strata-control)
  strata-portal/      Field device onboarding UI (Leptos WASM, served by strata-sender)
  strata-sim/         Integration test harness — Linux netns + tc-netem
```

---

## Asymmetry between sender and receiver daemons

The two daemons are **structurally symmetric** but **different in what features they enable**. This is intentional.

### What they share

- WebSocket connection to `strata-control` (auth, heartbeat, receive commands, send stats)
- `PipelineManager` — spawns `strata-pipeline <mode>` as a child process, SIGINT/SIGKILL lifecycle, stats via local UDP
- `PipelineMonitor` — watches for unexpected child exits, reports to control plane
- Prometheus `/metrics` endpoint (optional)
- Graceful shutdown

### What only `strata-sender` has

- **Hardware scanner** — sysinfo, network interface enumeration, modem presence detection
- **Portal** — Axum HTTP server on `:3001` serving the onboarding WASM UI
- **`device.status` heartbeat** — reports interface state, signal quality, media inputs

### What only `strata-receiver` has

- **Capacity registration** — advertises `max_streams`, `region`, per-link bind ports to control plane on connect
- **Stream assignment** — receives `receiver.stream.start` with `{stream_id, relay_url, bind_ports}` from control plane
- **Port allocator** — manages a pool of UDP receive ports and assigns them per stream

### The bandwidth concern

The control plane WS connections carry **management traffic only** — JSON envelopes a few hundred bytes each, ~1/second. The actual media flows directly from `strata-pipeline sender` to `strata-pipeline receiver` over UDP without touching the control plane. So the `strata-sender` daemon's connection to `strata-control` adds negligible overhead on a cellular uplink.

The telemetry payload (sent from sender → control plane) is ~500 bytes/sec of JSON (link stats for each active link, once per second). On a constrained 5 Mbps cellular link that is 0.01% of bandwidth.

There is no case where the control plane connection saturates the sender's uplink.

### Standalone deployment (no control plane)

`strata-pipeline sender` and `strata-pipeline receiver` can be run directly without any daemon:

```bash
# Receiver side (cloud)
strata-pipeline receiver --bind 0.0.0.0:5000,0.0.0.0:5002 \
  --relay-url "rtmp://a.rtmp.youtube.com/live2/KEY"

# Sender side (field device)
strata-pipeline sender --dest receiver.example.com:5000,receiver.example.com:5002 \
  --source v4l2 --bitrate 4000
```

The receiver daemon (`strata-receiver`) is optional. You only need it when you want fleet management: automatic receiver assignment, multiple concurrent streams, capacity-aware scheduling, live stats in the dashboard. If you just want to run one stream manually, `strata-pipeline` tools are everything you need.

This means the architecture degrades gracefully:

| Deployment | Components needed |
|---|---|
| Single manual stream | `strata-pipeline sender` + `strata-pipeline receiver` |
| Managed sender, manual receiver | `strata-sender` + `strata-control` + `strata-pipeline receiver` |
| Fully managed | `strata-sender` + `strata-receiver` + `strata-control` |

---

## Control plane changes needed for strata-receiver

### New DB table

```sql
CREATE TABLE receivers (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id     UUID NOT NULL REFERENCES users(id),
    hostname     TEXT NOT NULL,
    region       TEXT,                    -- "eu-central", "us-east", etc.
    bind_host    TEXT NOT NULL,           -- e.g. "203.0.113.45"
    link_ports   INTEGER[] NOT NULL,      -- e.g. [5000, 5002, 5004]
    max_streams  INTEGER NOT NULL DEFAULT 6,
    active_streams INTEGER NOT NULL DEFAULT 0,
    online       BOOLEAN NOT NULL DEFAULT false,
    last_seen_at TIMESTAMPTZ
);
```

### New WS endpoint

```
GET /receiver/ws
```

Mirrors `/agent/ws` exactly: auth.login → register capacity → bidirectional command/stats loop.

### Stream assignment

Replace the hardcoded `RECEIVER_LINKS` env var in `streams.rs::build_receiver_links()` with:

```rust
// Pick the least-loaded online receiver in the preferred region
let receiver = db::pick_receiver(&pool, owner_id, region, link_count).await?;
let bind_ports = receiver.link_ports[..link_count].to_vec();
// Send receiver.stream.start to that receiver via its WS handle
// Send stream.start to sender with receiver.bind_host + those ports
```

---

## Docker Compose after strata-receiver exists

```yaml
services:
  strata-control:   # unchanged
  strata-sender-sim:
    entrypoint: strata-sender      # spawns strata-pipeline sender per stream
  strata-receiver:
    entrypoint: strata-receiver    # spawns strata-pipeline receiver per stream
                                   # (replaces the inline shell script that ran strata-pipeline directly)
  postgres:         # unchanged
```

The docker-compose `strata-receiver` service today runs `strata-pipeline receiver` directly via a shell script. Once the daemon exists, it just becomes `ENTRYPOINT ["strata-receiver"]` — symmetric with the sender service. The RELAY_URL env var goes away; the receiver daemon gets the relay URL from the `receiver.stream.start` command sent by the control plane.
