# Technology Choices & Trade-off Analysis

> **Status:** Draft — captures reasoning for key decisions so they can be revisited.

---

## 1. Language

### Decision: Full-stack Rust — axum backend, Leptos dashboard

**Backend (control plane, sender agent, receiver worker):**
Rust is the obvious choice — the entire transport engine is Rust, the GStreamer
bindings are Rust, and the toolchain is already established. Using a different
language for the platform layer would create a pointless context-switch tax.

Framework: **axum** (tokio-based, async, tower middleware ecosystem).

**Dashboard:**
**Leptos** — a Rust WASM framework with fine-grained reactivity and SSR support.
The dashboard is a web page (not a native app — nothing for users to install),
so it must work well in a browser, but it doesn't need the full SPA ecosystem.

Why Leptos over React/Svelte:
- **Full-stack Rust**: One language, one toolchain, one CI pipeline. The control
  plane can serve the dashboard directly from the same axum binary — no separate
  Node.js build step, no npm, no JavaScript ecosystem churn.
- **AI-assisted development**: With Claude Code as the primary development tool,
  the "harder to hire for" argument is irrelevant. The AI writes Rust and Leptos
  as fluently as React.
- **SSR + hydration**: Leptos can server-render the initial page load (fast TTFB)
  and hydrate for interactivity. Good for the dashboard's real-time stats views.
- **Type safety end-to-end**: Shared types between backend API and frontend views
  with zero serialization glue.
- **Binary embedding**: The compiled WASM + assets can be embedded in the server
  binary via `include_dir!` or served from a static directory. Single binary
  deployment.

Alternatives considered:
- **React/Vue + TypeScript**: Largest ecosystem, but adds a Node.js dependency,
  a separate build pipeline, and a language context switch. Unnecessary complexity
  for a solo developer.
- **Svelte**: Clean and lightweight, but still JavaScript/TypeScript and a
  separate build step.
- **Yew**: Rust/WASM like Leptos, but uses a virtual DOM (React-like). Leptos's
  fine-grained signals are more efficient and ergonomic.

---

## 2. Deployment: Docker Compose + Process-per-Stream Inside

### Architecture

The platform ships as a **Docker Compose stack**. The control plane, database,
and receiver workers all run inside containers — but receiver workers are still
child processes, not individual containers.

```
docker compose up
  ├── strata-control (container)
  │     ├── axum API server (serves dashboard + REST + WSS)
  │     ├── spawns child processes:
  │     │     ├── strata-receiver --stream-id str_001 (pid inside container)
  │     │     ├── strata-receiver --stream-id str_002
  │     │     └── ...
  │     └── serves Leptos dashboard on :443
  │
  └── postgres (container)
        └── PostgreSQL 16 (data in named volume)
```

### Why This Hybrid Approach

Docker Compose handles the **deployment** problem (reproducible, portable, one
command to spin up the whole platform on any VPS). Process-per-stream handles
the **runtime** problem (fast startup, zero overhead, direct UDP binding).

| Factor | Process-in-Container | Container-per-Stream | Kubernetes Pod |
|---|---|---|---|
| Startup time | <100ms | 1–5s | 5–30s |
| Memory overhead | ~0 per worker | ~10–30 MB per container | ~50–100 MB per pod |
| UDP port binding | `--network host` on the control container | Port mapping per stream | hostNetwork or NodePort |
| Deploy complexity | `docker compose up` | Same but more containers | Cluster + etcd + DNS |
| Debugging | `docker exec` + `strace` | Docker exec per container | kubectl exec |
| Maximum density | ~40 streams/host | ~30 streams/host | ~20 streams/host |

GStreamer pipelines are inherently process-bound (GMainLoop, bus watches).
Wrapping each in a separate container adds overhead and networking complexity
for no benefit. But wrapping the whole platform in Docker Compose makes deployment
a one-liner and works identically on every VPS provider.

### Regional Deployment

The same Docker Compose stack deploys to each regional VPS:

```
Europe VPS      →  docker compose up  (strata-control + postgres)
US-East VPS     →  docker compose up  (strata-control + postgres)
Asia-Pacific    →  docker compose up  (strata-control + postgres)
```

Each region is **independent** — its own database, its own users, its own streams.
No cross-region coordination needed for v1. Regional DNS (Cloudflare, Route 53)
routes users to the nearest VPS.

Future: If cross-region features are needed (e.g., a user in Europe viewing a
stream received in Asia), add a federation layer. But that's far out.

### When to Reconsider

- **Container-per-stream**: If process isolation within the container becomes a
  compliance requirement (multi-tenant security audits).
- **Kubernetes**: When running >100 concurrent streams across >3 hosts with
  auto-scaling. Even then, consider Nomad first.

---

## 3. Database

### Decision: PostgreSQL from Day One

PostgreSQL runs as a container in the Docker Compose stack. No SQLite phase.

**Why skip SQLite:**
- The platform will deploy to **regional VPSes worldwide** from early on.
  PostgreSQL is the correct choice for any multi-region deployment.
- Docker Compose makes PostgreSQL zero-effort to deploy — it's just another
  service in the compose file, not a separate ops burden.
- `sqlx` with compile-time checked queries provides excellent Rust integration.
- Avoiding a SQLite→PostgreSQL migration eliminates throwaway work.
- Backups: `pg_dump` via a cron container or managed backup service.

**Rust integration:** `sqlx` with the `postgres` feature. Compile-time query
checking, async, connection pooling, inline migrations.

```toml
# Cargo.toml
[dependencies]
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres", "migrate"] }
```

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
| Backend language | Rust (axum) | Same as transport engine, single toolchain |
| Frontend | Leptos (Rust/WASM) | Full-stack Rust, SSR, embedded in server binary |
| Deployment | Docker Compose + process-per-stream inside | One-command deploy, zero per-worker overhead |
| Database | PostgreSQL | Multi-region from day one, sqlx compile-time checks |
| Auth | Custom JWT (Argon2id + Ed25519) | Minimal dependencies, device auth needs |
| Real-time | WebSocket | Bidirectional, browser-native, well-understood |
| Video encryption | RIST PSK (AES-256) | Built into librist, zero implementation cost |
| Monitoring | Prometheus + structured logs | Industry standard, low overhead |
| Repo structure | Monorepo | Transport + platform share crates |
| Client apps | None — web page only | No installs, works on any device with a browser |

---

## 8. Resolved Questions

Previously open, now decided:

| Question | Decision | Rationale |
|---|---|---|
| Auth provider | Roll own JWT (Argon2id + Ed25519) | <100 users initially, devices use key pairs not passwords |
| Database | PostgreSQL from day one | Multi-region VPS deployment, Docker Compose makes it free |
| Dashboard framework | Leptos (Rust/WASM) | Full-stack Rust, AI-assisted dev, no JS build step |
| RIST encryption | PSK (pre-shared key) | Simpler, sufficient, already works in librist [03](03-security-model.md) |
| Sender provisioning | AP Wi-Fi captive portal + enrollment token | Documented in [04 §9](04-sender-agent.md#9-ap-wi-fi-onboarding-first-time-setup) |
| Multi-region | Independent regional VPSes, no cross-region sync | Each VPS is self-contained. RIST handles jitter; receiver→platform is TCP |
| Client apps | None — web page only | Dashboard is a Leptos web page served by axum. No native apps to install |
| Deployment packaging | Docker Compose | Whole stack in one compose file. Process-per-stream for workers inside |

### Remaining Open Questions

1. **TLS certificates** — Let's Encrypt via Caddy/Traefik reverse proxy in the compose stack, or self-managed?
2. **Billing** — Stripe integration? Per-stream pricing? Monthly plans? (Not needed for v1.)
3. **CDN for dashboard** — Serve Leptos assets from a CDN, or directly from axum? (Directly is fine for v1.)

---

*Next documents in this directory:*
- [02-control-protocol.md](02-control-protocol.md) — WebSocket control protocol between sender agent and control plane
- [03-security-model.md](03-security-model.md) — Authentication, encryption, and trust model
- [04-sender-agent.md](04-sender-agent.md) — Sender agent daemon design
- [05-receiver-workers.md](05-receiver-workers.md) — Receiver worker lifecycle and forwarding
- [08-local-dev-environment.md](08-local-dev-environment.md) — Local development simulation with Docker Compose
