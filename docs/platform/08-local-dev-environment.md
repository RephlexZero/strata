# Local Development Environment

> **Status:** Active implementation plan.

---

## 1. Two Modes of Local Testing

| Mode | Purpose | How |
|---|---|---|
| **Dev mode** (daily) | Fast iteration — code, compile, test, repeat | `cargo run` binaries directly in the devcontainer |
| **Deploy mode** (pre-release) | Validate Docker packaging + compose stack | `docker compose up` inside devcontainer (DinD) |

Dev mode is what you use 95% of the time. Deploy mode is for verifying that the
Docker images build correctly and the compose stack works before pushing to a VPS.

---

## 2. Dev Mode — Process-Based

Run everything as plain processes inside the devcontainer. No Docker needed.

```
Terminal 1 (PostgreSQL):
  └── PostgreSQL via devcontainer feature (auto-started)
      └── listening on localhost:5432

Terminal 2 (Control Plane):
  └── cargo run --bin strata-control
      ├── axum server on :3000
      │     ├── Leptos dashboard (browser: http://localhost:3000)
      │     ├── REST API (/api/...)
      │     └── WSS endpoint (/agent/ws)
      └── spawns strata-receiver child processes as needed

Terminal 3 (Sender Agent):
  └── cargo run --bin strata-agent -- --simulate
      ├── Onboarding portal on :3001 (browser: http://localhost:3001)
      ├── Connects to ws://localhost:3000/agent/ws
      ├── Simulated modems (fake signal, fake carrier)
      └── videotestsrc pipeline (simulated HDMI input)
```

Both web UIs are accessible from your host browser via VS Code port forwarding.

### Why This Is Better Than Docker for Dev

- **Compile once, run immediately** — no image rebuild
- **Direct debugger attach** — `rust-analyzer` + `codelldb` just work
- **Instant logs** — stdout right in the terminal
- **Hot-path iteration** — change code → `cargo run` → test → repeat in seconds
- **Full network access** — sender and receiver talk over localhost, no NAT
- **Already have everything** — GStreamer, librist, network tools all installed

### Prerequisites Added to Devcontainer

The devcontainer needs additions for platform development:

1. **PostgreSQL** — runs as a service inside the devcontainer, auto-starts
2. **Docker-in-Docker** — for deploy mode testing (optional, only for compose)
3. **`wasm32-unknown-unknown` target** — for Leptos WASM compilation
4. **`trunk`** — Leptos WASM bundler (installed via `cargo install`)
5. **`cargo-leptos`** — compiles server + client in one step

### Database Setup (One-Time)

```bash
# After devcontainer rebuild:
sudo -u postgres createuser strata --createdb
sudo -u postgres createdb strata -O strata
export DATABASE_URL="postgres://strata@localhost/strata"

# Migrations run automatically on first start of strata-control
cargo run --bin strata-control
```

### Quick Start

```bash
# Terminal 1: Control plane
export DATABASE_URL="postgres://strata@localhost/strata"
cargo run --bin strata-control

# Terminal 2: Simulated sender
cargo run --bin strata-agent -- --simulate \
  --control-url ws://localhost:3000/agent/ws \
  --portal-port 3001

# Browser tab 1: http://localhost:3000  (dashboard)
# Browser tab 2: http://localhost:3001  (sender portal)
```

---

## 3. Deploy Mode — Docker Compose (Pre-Release)

When you want to verify the actual deployment packaging works:

```bash
# Build and run the full stack in containers
cd dev/
docker compose build
docker compose up -d
docker compose logs -f
```

This uses Docker-in-Docker (the devcontainer runs a nested Docker daemon via the
`docker-in-docker` feature). Since the devcontainer is already privileged, DinD
works without issues.

### Docker Compose Stack

```yaml
# dev/docker-compose.yml
services:
  postgres:
    image: postgres:16-alpine
    environment:
      POSTGRES_DB: strata
      POSTGRES_USER: strata
      POSTGRES_PASSWORD: dev-only-password
    volumes:
      - pgdata:/var/lib/postgresql/data
      - ./seed.sql:/docker-entrypoint-initdb.d/seed.sql
    ports:
      - "5432:5432"

  strata-cloud:
    build:
      context: ..
      dockerfile: dev/Dockerfile.control
    depends_on: [postgres]
    environment:
      DATABASE_URL: "postgres://strata:dev-only-password@postgres/strata"
      LISTEN_ADDR: "0.0.0.0:3000"
    ports:
      - "3000:3000"
      - "15000-15050:15000-15050/udp"

  strata-sender-sim:
    build:
      context: ..
      dockerfile: dev/Dockerfile.sender-sim
    depends_on: [strata-cloud]
    environment:
      CONTROL_PLANE_URL: "ws://strata-cloud:3000/agent/ws"
      PORTAL_LISTEN_ADDR: "0.0.0.0:3001"
      SIM_MODEM_COUNT: "2"
    ports:
      - "3001:3001"

volumes:
  pgdata:
```

### When to Use Deploy Mode

- Before tagging a release
- When changing Dockerfiles or compose config
- When testing that migrations work from scratch
- CI smoke tests

---

## 4. What Gets Tested Where

| Feature | Dev Mode | Deploy Mode |
|---|---|---|
| Rust compilation + type checking | ✅ `cargo build` | ✅ Docker build |
| Database migrations | ✅ localhost PG | ✅ containerised PG |
| REST API | ✅ curl / browser | ✅ curl / browser |
| WebSocket agent↔control | ✅ localhost | ✅ container network |
| RIST bonding (UDP) | ✅ localhost | ✅ container network |
| Leptos dashboard | ✅ cargo-leptos serve | ✅ served from container |
| Onboarding portal | ✅ localhost:3001 | ✅ container:3001 |
| Docker image correctness | ❌ | ✅ |
| Compose networking | ❌ | ✅ |
| Transport engine (336 tests) | ✅ `cargo test` | ❌ (not in compose) |

---

## 5. Build Order — Implementation Steps

This is the actual implementation sequence:

### Step 1: `strata-common` crate

Shared types used by both control plane and agent.

```
crates/strata-common/
├── Cargo.toml
└── src/
    ├── lib.rs          # Re-exports
    ├── protocol.rs     # WSS message types (serde structs)
    ├── auth.rs         # JWT, Argon2id, Ed25519 helpers
    ├── models.rs       # User, Sender, Destination, Stream types
    └── ids.rs          # Prefixed ID generation (usr_xxx, snd_xxx)
```

### Step 2: `strata-control` crate (binary)

Control plane + Leptos dashboard + receiver worker spawner.

```
crates/strata-control/
├── Cargo.toml
└── src/
    ├── main.rs         # axum server bootstrap
    ├── db.rs           # sqlx PostgreSQL pool + migrations
    ├── api/
    │   ├── mod.rs
    │   ├── auth.rs     # POST /api/login, /api/register
    │   ├── senders.rs  # GET/POST /api/senders
    │   ├── streams.rs  # POST /api/streams/start, /stop
    │   └── ws.rs       # GET /agent/ws (WebSocket handler)
    ├── receiver.rs     # Spawn/manage strata-receiver child processes
    └── dashboard/      # Leptos components (login, sender list, stats)
```

### Step 3: `strata-agent` crate (binary)

Sender agent daemon.

```
crates/strata-agent/
├── Cargo.toml
└── src/
    ├── main.rs         # Agent daemon entry point
    ├── control.rs      # WSS client to control plane
    ├── hardware.rs     # Real + simulated modem/camera scanner
    ├── pipeline.rs     # GStreamer sender pipeline manager
    ├── portal.rs       # Onboarding captive portal (axum)
    └── telemetry.rs    # Stats scraping + reporting
```

### Step 4: Dev environment polish

- Update devcontainer.json (add PG + DinD features)
- Write dev/docker-compose.yml + Dockerfiles
- Write dev/seed.sql

---

## 6. Docker-in-Docker: Why It Works Here

The devcontainer is already `"privileged": true` (required for `ip netns` in the
transport engine tests). This means Docker-in-Docker works without any additional
permissions. The `docker-in-docker` devcontainer feature installs a nested Docker
daemon that runs inside the container.

**Performance:** On a powerful home PC, DinD adds negligible overhead. The nested
Docker daemon uses the host kernel directly — there's no virtualisation layer.
Container builds, networking, and storage all perform at near-native speed.

**Port forwarding:** VS Code automatically forwards ports from containers within
the DinD daemon to your host browser. `localhost:3000` in a nested container
still shows up in your browser.

**Alternative considered: host Docker socket mount.** This would run containers on
the host daemon instead of a nested one. It's slightly more efficient but creates
path-mapping confusion (host paths ≠ devcontainer paths for volume mounts). DinD
is cleaner for a self-contained dev environment.

---

## 7. Relationship to Existing Tests

The dev environment is for interactive platform testing — seeing the dashboard,
clicking buttons, watching stats flow. It complements but does not replace the
existing test suite:

| Test Type | Count | Purpose |
|---|---|---|
| Transport engine unit tests | 294 | Bonding correctness |
| GStreamer plugin tests | 36 | Element integration |
| Network simulation tests | 6 | Impairment resilience |
| **Platform dev mode** | — | **End-to-end UX testing** |
| **Platform deploy mode** | — | **Docker packaging validation** |

---

*Related documents:*
- [01-architecture-overview.md](01-architecture-overview.md) — System design and deployment model
- [04-sender-agent.md](04-sender-agent.md) — Agent design, including simulation mode
- [06-technology-choices.md](06-technology-choices.md) — Why Docker Compose, PostgreSQL, Leptos
