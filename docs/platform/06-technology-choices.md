# Technology Choices & Trade-off Analysis

> **Status:** Draft — captures reasoning for key decisions so they can be revisited.

---

## 1. Language

### Recommendation: Rust for backend, TypeScript/React for dashboard

**Backend (control plane, sender agent, receiver worker):**
Rust is the obvious choice — the entire transport engine is Rust, the GStreamer
bindings are Rust, and the team already has the toolchain. Using a different
language for the platform layer would create a pointless context-switch tax.

Potential framework: **axum** (tokio-based, async, tower middleware ecosystem).

**Dashboard:**
The dashboard is a standard CRUD SPA with real-time WebSocket updates. React or
Vue with TypeScript is the pragmatic choice — fastest to iterate on, largest
ecosystem for UI components, easy to hire for.

Rust-native web frameworks (Leptos, Yew) are interesting but add compilation
time and reduce the pool of potential contributors. Not worth it for a dashboard.

---

## 2. Deployment: Why Process-per-Stream over Docker/K8s

### Process-per-Stream (Chosen)

Each receiver worker is a child process managed by the control plane:

```
strata-control (pid 1000)
  └── strata-receiver --stream-id str_001 (pid 1001)
  └── strata-receiver --stream-id str_002 (pid 1002)
  └── strata-receiver --stream-id str_003 (pid 1003)
```

**Why this wins for v1:**

| Factor | Process | Docker Container | Kubernetes Pod |
|---|---|---|---|
| Startup time | <100ms | 1–5s | 5–30s |
| Memory overhead | ~0 | ~10–30 MB per container | ~50–100 MB per pod |
| UDP port binding | Direct | Requires `--network host` or port mapping | Requires hostNetwork or NodePort |
| Operational complexity | Low (systemd) | Medium (docker daemon) | High (cluster, etcd, DNS) |
| Debugging | `strace -p <pid>`, gdb | Docker exec, log drivers | kubectl exec, log aggregation |
| Maximum density | ~40 streams/host | ~30 streams/host | ~20 streams/host |
| Team knowledge needed | Linux basics | Docker + networking | k8s ecosystem |

GStreamer pipelines are inherently process-bound (GMainLoop, bus watches). Docker
adds overhead and networking complexity (especially for UDP) without meaningful
benefit for this workload. Kubernetes is even worse — it's designed for stateless
HTTP microservices, not latency-sensitive UDP media workers.

### When to Reconsider

- **Docker**: When deploying to customer-managed infrastructure where isolation
  matters (security compliance), or when the deploy target varies between
  Linux distributions.
- **Kubernetes**: When running >100 concurrent streams across >3 hosts with
  auto-scaling requirements. Even then, consider Nomad or a simpler scheduler
  before k8s.

---

## 3. Database

### Recommendation: SQLite for v1, PostgreSQL when multi-host

| Factor | SQLite | PostgreSQL |
|---|---|---|
| Deployment | Zero-config, single file | Separate service to manage |
| Performance | 50k+ reads/s, ~1k writes/s | Much higher write throughput |
| Multi-host | Single-host only | Shared across hosts |
| Backups | `cp database.db backup.db` | pg_dump, WAL archiving |
| Migrations | rusqlite + manual | sqlx + inline migrations |

For v1 (single host, <100 users, <50 senders), SQLite with WAL mode is more than
sufficient and removes an entire deployment dependency.

When the platform goes multi-host, migrate to PostgreSQL. The data model is simple
enough that migration is a few hours of work.

### Data Model (Sketch)

```sql
-- Users
CREATE TABLE users (
    id TEXT PRIMARY KEY,            -- usr_xxx
    email TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,     -- argon2id
    role TEXT DEFAULT 'operator',   -- admin, operator, viewer
    created_at TEXT NOT NULL
);

-- Senders (devices)
CREATE TABLE senders (
    id TEXT PRIMARY KEY,            -- snd_xxx
    owner_id TEXT REFERENCES users(id),
    name TEXT,
    hostname TEXT,
    enrollment_token_hash TEXT,     -- consumed after enrollment
    device_public_key TEXT,         -- ed25519
    last_seen_at TEXT,
    created_at TEXT NOT NULL
);

-- Streaming destinations (RTMP keys, SRT endpoints)
CREATE TABLE destinations (
    id TEXT PRIMARY KEY,            -- dst_xxx
    owner_id TEXT REFERENCES users(id),
    platform TEXT NOT NULL,         -- youtube, twitch, custom_rtmp, srt
    name TEXT,
    url TEXT NOT NULL,              -- encrypted at rest
    stream_key TEXT,                -- encrypted at rest
    created_at TEXT NOT NULL
);

-- Active + historical streams
CREATE TABLE streams (
    id TEXT PRIMARY KEY,            -- str_xxx
    sender_id TEXT REFERENCES senders(id),
    destination_id TEXT REFERENCES destinations(id),
    state TEXT NOT NULL,            -- starting, live, stopping, ended, failed
    started_at TEXT,
    ended_at TEXT,
    config_json TEXT,               -- snapshot of bonding config used
    total_bytes BIGINT DEFAULT 0,
    error_message TEXT
);
```

---

## 4. Authentication

### Recommendation: Roll own JWT auth for v1

External auth providers (Auth0, Keycloak) add cost and complexity that's not
justified for an initial user base of <100 accounts. A simple email + password
flow with Argon2id hashing and Ed25519-signed JWTs is straightforward to
implement in Rust.

Add OAuth2 (Google, GitHub) login as a convenience feature later.

### Why Not Keycloak/Auth0?

- Keycloak: Heavy Java service, requires PostgreSQL, absurd resource footprint
  for <100 users
- Auth0: SaaS dependency, monthly cost, overkill for devices that auth with
  key pairs (not human users)
- Supabase Auth: Reasonable, but ties you to their PostgreSQL instance

### Migration Path

If the platform grows to >1000 users or needs SSO/SAML for enterprise customers,
swap the auth module behind the same JWT interface. The rest of the system doesn't
need to change.

---

## 5. Real-Time Updates

### Recommendation: WebSocket for dashboard, WebSocket for agent

Both the dashboard and the sender agent need real-time bidirectional communication.
WebSocket is the right choice for both:

- Dashboard ↔ Control Plane: Live stats, sender status changes, alerts
- Agent ↔ Control Plane: Commands, status reports, telemetry

**Not:** SSE (server-sent events) — unidirectional, doesn't work for commands.
**Not:** gRPC streams — adds protobuf dependency, no browser support without grpc-web.
**Not:** MQTT — another broker to manage, overkill for this topology.

---

## 6. Monitoring & Observability

| Component | Tool | Notes |
|---|---|---|
| Metrics | Prometheus (pull from `/metrics` endpoint) | Per-stream bitrate, CPU, latency |
| Logging | stdout → journald → optional Loki | Structured JSON logs |
| Alerting | Prometheus Alertmanager or simple webhook | Stream down, sender offline, high error rate |
| Dashboard | Built-in (the SPA itself) | For end users |
| Ops dashboard | Grafana (optional) | For platform operators |

Expose a `/metrics` endpoint on the control plane in Prometheus format. This is
trivial with the `metrics` + `metrics-exporter-prometheus` Rust crates.

---

## 7. Summary of Choices

| Decision | Choice | Rationale |
|---|---|---|
| Backend language | Rust (axum) | Same as transport engine, team expertise |
| Frontend | React + TypeScript | Fastest iteration, largest ecosystem |
| Deployment | Process-per-stream | Simplest, best performance for UDP media |
| Database | SQLite (v1) → PostgreSQL (multi-host) | Zero-config start, easy migration |
| Auth | Custom JWT (Argon2id + Ed25519) | Minimal dependencies, device auth needs |
| Real-time | WebSocket | Bidirectional, browser-native, well-understood |
| Video encryption | RIST PSK (AES-256) | Built into librist, zero implementation cost |
| Monitoring | Prometheus + structured logs | Industry standard, low overhead |
| Repo structure | Monorepo | Transport + platform share crates |
