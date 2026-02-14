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
│   ├── strata-control/           # Control plane API server (NEW)
│   └── strata-common/            # Shared types, auth, protocol (NEW)
├── web/                          # Dashboard SPA (NEW)
├── deploy/                       # Docker, k8s, systemd configs (NEW)
├── examples/                     # TOML configs (existing)
├── docs/
│   └── platform/                 # These architecture docs
└── wiki/                         # User-facing wiki (existing)
```

### Why Not Separate Repos?

- The agent embeds `gst-rist-bonding` and `rist-bonding-core` directly
- The receiver workers use the same crates
- Atomic commits across transport + platform changes
- Single CI pipeline
- Can always split later if the team grows; premature separation is worse

---

## 5. Deployment Model Decision

This is the hardest architectural call. Three options analysed:

### Option A: Process-per-Stream (Recommended for v1)

```
┌─────────────────────────────────────────┐
│               VPS (single host)          │
│                                          │
│  systemd service: strata-control         │
│    └─ API server (Rust, axum/actix)      │
│    └─ spawns/manages child processes     │
│                                          │
│  For each active stream:                 │
│    └─ strata-receiver (child process)    │
│       └─ rsristbondsrc → pipeline → RTMP │
│                                          │
└──────────────────────────────────────────┘
```

**Pros:**
- Simplest to implement and debug
- Process isolation — one bad stream can't crash others
- Each process: ~50–100 MB RAM for a 1080p30 stream
- A single 8-core VPS can handle 20–40 concurrent streams
- No container orchestration overhead
- Matches GStreamer's threading model perfectly

**Cons:**
- Single-host ceiling (~40 streams on a beefy VPS)
- Manual scaling (add more VPSes, load-balance at DNS/IP level)

### Option B: Containerised (Docker Compose)

```
┌─────────────────────────────────────────────┐
│  docker-compose                              │
│                                              │
│  strata-control (1 container)                │
│  strata-receiver-1 (1 container per stream)  │
│  strata-receiver-2                           │
│  ...                                         │
│  postgres (1 container)                      │
│  nginx (1 container, TLS termination)        │
└─────────────────────────────────────────────┘
```

**Pros:**
- Clean isolation, easily reproducible
- Can use Docker health checks for auto-restart
- Portable across VPS providers

**Cons:**
- Docker overhead per container adds up at scale
- NAT/port mapping complexity for UDP RIST traffic
- More moving parts for marginal benefit over process-per-stream

### Option C: Kubernetes

**Pros:**
- Auto-scaling, rolling deploys, service mesh
- Multi-node scaling built in

**Cons:**
- Enormous operational complexity for a small team
- k8s is terrible for UDP workloads without specialised CNI plugins
- Minimum 3-node cluster overhead
- Premature at this stage

### Verdict

**Start with Option A (process-per-stream).** It handles the realistic initial
scale (5–40 concurrent streams) with minimal complexity. If demand exceeds a
single host, add a lightweight load-balancer (HAProxy/nginx for control plane,
DNS-based for UDP receiver ports). Containerise later when there's operational
pain, not before.

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

## 8. Open Questions

These should be resolved before implementation begins:

1. **Auth provider** — Roll own JWT auth, or use an external provider (Auth0, Keycloak, Supabase Auth)?
2. **Database** — SQLite (simplest, single-host) vs PostgreSQL (multi-host ready)?
3. **Dashboard framework** — Rust (Leptos/Yew) for full-stack Rust, or React/Vue for faster UI dev?
4. **RIST encryption** — librist supports PSK (pre-shared key) and DTLS. Which to use? PSK is simpler; DTLS is more flexible.
5. **Sender auto-provisioning** — How does a new Orange Pi get registered? QR code? Enrollment token? Manual?
6. **Multi-region** — Do receivers need to be geographically close to senders for latency? (Probably not — RIST handles jitter, and the receiver-to-platform leg is TCP.)

---

*Next documents in this directory:*
- [02-control-protocol.md](02-control-protocol.md) — WebSocket control protocol between sender agent and control plane
- [03-security-model.md](03-security-model.md) — Authentication, encryption, and trust model
- [04-sender-agent.md](04-sender-agent.md) — Sender agent daemon design
- [05-receiver-workers.md](05-receiver-workers.md) — Receiver worker lifecycle and forwarding
- [06-api-reference.md](06-api-reference.md) — REST + WebSocket API specification
