# Strata Cloud Platform — Architecture Spec

> **Status:** Draft — future work. The transport layer (bonded RIST) is proven and stable.
> This document architects the managed platform that wraps it into a multi-tenant
> streaming service.

---

## 1. Problem Statement

We have a working bonded RIST transport engine (`rsristbondsink` / `rsristbondsrc`).
Today, using it requires SSH access, manual pipeline construction, and coordination
between sender and receiver. The goal is a **managed platform** where:

1. Clients log in to a web dashboard
2. They see their field senders (Orange Pi devices), each showing connected network interfaces and available media inputs
3. They remotely configure, start, and stop broadcasts from those senders
4. The broadcast is received on a cloud VPS, which forwards it to a streaming platform (YouTube, Twitch, etc.)
5. The VPS handles many concurrent clients efficiently
6. Both control and video traffic are encrypted

---

## 2. High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          Client Browser                                 │
│                                                                         │
│   Dashboard: login, see senders, configure, start/stop, view stats      │
│                                                                         │
└──────────────────────────────┬──────────────────────────────────────────┘
                               │ HTTPS / WSS
                               ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                        Cloud VPS (Platform)                             │
│                                                                         │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────────────────────┐  │
│  │  API Gateway  │  │   Auth       │  │        Control Plane          │  │
│  │  (HTTPS/WSS)  │──│  (JWT/OAuth) │──│  - Sender registry            │  │
│  │               │  │              │  │  - Session management         │  │
│  └──────┬───────┘  └──────────────┘  │  - Config push to senders     │  │
│         │                             │  - Stream lifecycle           │  │
│         │                             └───────────────────────────────┘  │
│         │                                                               │
│  ┌──────┴────────────────────────────────────────────────────────────┐  │
│  │                         Data Plane                                │  │
│  │                                                                   │  │
│  │   ┌──────────────────┐  ┌──────────────────┐                      │  │
│  │   │ Receiver Worker 0 │  │ Receiver Worker 1 │  ...  (1 per stream)│  │
│  │   │                  │  │                  │                      │  │
│  │   │ rsristbondsrc    │  │ rsristbondsrc    │                      │  │
│  │   │   ↓              │  │   ↓              │                      │  │
│  │   │ GStreamer pipeline│  │ GStreamer pipeline│                      │  │
│  │   │   ↓              │  │   ↓              │                      │  │
│  │   │ rtmpsink/srtsink │  │ rtmpsink/srtsink │                      │  │
│  │   └──────────────────┘  └──────────────────┘                      │  │
│  │                                                                   │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                         │
└─────────────────┬──────────────────┬────────────────────────────────────┘
                  │ RIST (UDP)       │ RTMP/SRT (TCP)
                  │ encrypted        │
                  ▼                  ▼
        ┌──────────────┐    ┌──────────────────┐
        │ Field Senders │    │ Streaming Platform │
        │ (Orange Pi)   │    │ (YouTube, Twitch)  │
        └──────────────┘    └──────────────────┘
```

---

## 3. Component Breakdown

### 3.1 Sender Agent (runs on Orange Pi)

A lightweight daemon running on each field device. It maintains a persistent
connection to the control plane and exposes local hardware state.

**Responsibilities:**
- Connect to control plane over WSS (outbound — no inbound ports needed)
- Report available network interfaces (wwan0, wwan1, eth0, ...) with status
- Report available media inputs (V4L2 devices, saved media files)
- Accept remote configuration (TOML config, bitrate, source, destination)
- Start/stop the GStreamer sender pipeline on command
- Stream real-time telemetry (link stats, bitrate, signal strength) to control plane
- Self-update capability (pull new binary, restart)

**Key design choice:** The sender agent initiates all connections outbound.
This means field devices behind carrier-grade NAT need zero port forwarding.

### 3.2 Control Plane (runs on VPS)

Manages sender registration, authentication, configuration, and stream lifecycle.

**Responsibilities:**
- User authentication (JWT or OAuth2) — who can control which senders
- Sender registry — track online/offline senders, their capabilities
- Configuration management — build and push TOML configs to senders
- Stream lifecycle — coordinate "start broadcast" → allocate receiver ports → push dest URIs to sender → start receiver worker → verify stream health
- Telemetry aggregation — collect stats from all active senders, expose to dashboard
- Session management — track active streams, clean up on disconnect

### 3.3 Data Plane / Receiver Workers (runs on VPS)

Each active stream gets its own receiver pipeline process:

```
rsristbondsrc (bonded RIST in) → tsdemux → h264parse → flvmux → rtmpsink (to YouTube)
```

**Responsibilities:**
- Receive bonded RIST stream from sender
- Reassemble, reorder (bonding engine handles this)
- Forward to configured streaming platform destination
- Report stream health metrics back to control plane
- Clean up when stream ends

### 3.4 Web Dashboard (browser)

SPA that communicates with the API gateway.

**Features:**
- Login / user management
- List of owned senders (online status, last seen, location)
- Per-sender view: network interfaces, media inputs, current config
- Configure and start/stop broadcasts
- Live telemetry view (per-link bitrate, RTT, loss, signal)
- Stream destination management (RTMP keys, SRT endpoints)

---

## 4. Repo Structure Decision

### Recommendation: Monorepo with Workspace Crates

Keep everything in the same repo. The transport engine is a dependency of both
the sender agent and the receiver workers. Splitting repos creates versioning
pain for no benefit at this scale.

```
strata/
├── crates/
│   ├── librist-sys/              # FFI bindings (existing)
│   ├── rist-bonding-core/        # Transport engine (existing)
│   ├── gst-rist-bonding/         # GStreamer plugin (existing)
│   ├── rist-network-sim/         # Test infra (existing)
│   ├── strata-agent/             # Sender agent daemon (NEW)
│   ├── strata-control/           # Control plane + Leptos dashboard (NEW)
│   └── strata-common/            # Shared types, auth, protocol (NEW)
├── dev/                          # Docker Compose for local simulation (NEW)
├── deploy/                       # Production docker-compose.yml (NEW)
├── examples/                     # TOML configs (existing)
├── docs/
│   └── platform/                 # These architecture docs
└── wiki/                         # User-facing wiki (existing)
```

The `strata-control` crate includes the Leptos dashboard — it's compiled into
the same binary. No separate `web/` directory or JavaScript build step.

### Why Not Separate Repos?

- The agent embeds `gst-rist-bonding` and `rist-bonding-core` directly
- The receiver workers use the same crates
- Atomic commits across transport + platform changes
- Single CI pipeline
- Can always split later if the team grows; premature separation is worse

---

## 5. Deployment Model Decision

### Chosen: Docker Compose + Process-per-Stream Inside

The platform ships as a Docker Compose stack. Docker handles deployment
(reproducible, portable, one-command). Process-per-stream handles the runtime
(fast startup, zero overhead, direct UDP).

```
docker compose up
  ├── strata-control (1 container)
  │     ├── axum API server
  │     ├── Leptos dashboard (served on :443)
  │     ├── spawns child processes:
  │     │     ├── strata-receiver --stream-id str_001
  │     │     ├── strata-receiver --stream-id str_002
  │     │     └── ...
  │     └── connects to postgres
  │
  └── postgres (1 container)
        └── PostgreSQL 16 (data in named volume)
```

**Why this hybrid:**
- Docker Compose: one-command deploy on any VPS, reproducible, easy TLS via
  Caddy/Traefik sidecar, easy PostgreSQL provisioning.
- Process-per-stream: <100ms startup, zero memory overhead per worker, direct
  UDP binding, matches GStreamer's process-bound threading model.
- NOT container-per-stream: adds 10–30 MB overhead per stream, complicates UDP
  port mapping, no benefit for this workload.
- NOT Kubernetes: way too heavy for this topology. Revisit only if >100
  concurrent streams across >3 hosts.

### Regional Deployment

The same Docker Compose stack deploys identically to each regional VPS:

```
Europe VPS      →  docker compose up
US-East VPS     →  docker compose up
Asia-Pacific    →  docker compose up
```

Each region is independent (own database, own users, own streams). Regional DNS
routes users to the nearest VPS. No cross-region sync needed for v1.

---

## 6. Scaling Estimate

| Resource | Per-Stream Cost | 8-core / 32 GB VPS | 16-core / 64 GB VPS |
|---|---|---|---|
| CPU | ~0.2 cores (receive + forward) | ~40 streams | ~80 streams |
| RAM | ~80 MB (GStreamer + buffers) | ~400 streams¹ | ~800 streams¹ |
| Network (in) | 5 Mbps typical | ~1 Gbps = 200 streams | ~10 Gbps = 2000 streams |
| Network (out) | 5 Mbps to platform | Same | Same |
| UDP ports | 2–3 per stream | 65k available | 65k available |

¹ RAM is rarely the bottleneck. CPU (for potential transcoding) and network are.

**Without transcoding** (passthrough to RTMP), CPU is ~0.2 cores per stream.
**With transcoding** (re-encode for ABR), CPU is ~1–2 cores per stream — drastically
reduces density. Avoid transcoding on the receiver unless absolutely necessary.

---

## 7. What Gets Built When

| Phase | Deliverable | Effort |
|---|---|---|
| **Phase 0** (done) | Transport engine, GStreamer plugin, integration_node | ✅ Complete |
| **Phase 1** | Sender agent daemon with WSS control channel | 2–3 weeks |
| **Phase 2** | Control plane API + receiver worker spawner | 2–3 weeks |
| **Phase 3** | Web dashboard (MVP — start/stop, status, config) | 2–3 weeks |
| **Phase 4** | Auth, multi-tenancy, stream key management | 1–2 weeks |
| **Phase 5** | Production hardening, monitoring, auto-restart | 1–2 weeks |
| **Phase 6** | Multi-VPS scaling, load balancing | When needed |

---

## 8. Resolved Questions

All major architectural questions have been decided. See [06 §8](06-technology-choices.md#8-resolved-questions)
for the full resolution table.

Remaining open:
1. **TLS certificates** — Let's Encrypt via Caddy/Traefik in compose, or self-managed?
2. **Billing** — Stripe? Per-stream pricing? Monthly plans? (Not v1.)
3. **CDN for dashboard** — Serve directly from axum. CDN later if needed.

---

*Next documents in this directory:*
- [02-control-protocol.md](02-control-protocol.md) — WebSocket control protocol between sender agent and control plane
- [03-security-model.md](03-security-model.md) — Authentication, encryption, and trust model
- [04-sender-agent.md](04-sender-agent.md) — Sender agent daemon design
- [05-receiver-workers.md](05-receiver-workers.md) — Receiver worker lifecycle and forwarding
- [08-local-dev-environment.md](08-local-dev-environment.md) — Local development simulation with Docker Compose
