# Local Development Environment — Full Stack Simulation

> **Status:** Draft plan. Describes how to run the entire Strata platform locally
> inside the existing devcontainer, simulating both the sender device and the
> cloud VPS, so the developer can view both web UIs from their home PC.

---

## 1. Goal

Run the complete platform on a single development machine:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         Developer's PC (browser)                        │
│                                                                         │
│   Tab 1: http://localhost:3000       Tab 2: http://localhost:3001        │
│   ┌─────────────────────────┐       ┌──────────────────────────────┐    │
│   │   Cloud Dashboard       │       │   Sender Onboarding Portal   │    │
│   │   (Leptos, served by    │       │   (Captive portal UI,        │    │
│   │    strata-control)      │       │    served by strata-agent)   │    │
│   │                         │       │                              │    │
│   │   - Login               │       │   - See simulated modems     │    │
│   │   - See sender online   │       │   - Enter enrollment token   │    │
│   │   - Start broadcast     │       │   - See HDMI input status    │    │
│   │   - View live stats     │       │   - Test connectivity        │    │
│   │   - Configure dest keys │       │                              │    │
│   └─────────────────────────┘       └──────────────────────────────┘    │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
          │ port 3000                          │ port 3001
          │                                    │
    ┌─────┴────────────────────────────────────┴──────────────────────┐
    │                    Devcontainer (Debian 12)                      │
    │                                                                  │
    │    docker compose -f dev/docker-compose.yml up                   │
    │                                                                  │
    │    ┌──────────────────────────────────────────────────────────┐  │
    │    │ Docker-in-Docker                                         │  │
    │    │                                                          │  │
    │    │  ┌──────────────────┐  ┌──────────────┐                  │  │
    │    │  │ strata-cloud     │  │ postgres     │                  │  │
    │    │  │ (container)      │  │ (container)  │                  │  │
    │    │  │                  │  │              │                  │  │
    │    │  │ strata-control   │  │ PostgreSQL   │                  │  │
    │    │  │   :3000 → dash   │◄─┤ :5432        │                  │  │
    │    │  │   :15000-15100   │  │              │                  │  │
    │    │  │   (RIST UDP)     │  └──────────────┘                  │  │
    │    │  │                  │                                    │  │
    │    │  │ strata-receiver  │                                    │  │
    │    │  │  (child process) │                                    │  │
    │    │  └──────────────────┘                                    │  │
    │    │           ▲                                              │  │
    │    │           │ RIST UDP + WSS                               │  │
    │    │           │                                              │  │
    │    │  ┌──────────────────┐                                    │  │
    │    │  │ strata-sender    │                                    │  │
    │    │  │ (container)      │                                    │  │
    │    │  │                  │                                    │  │
    │    │  │ strata-agent     │                                    │  │
    │    │  │   :3001 → portal │                                    │  │
    │    │  │                  │                                    │  │
    │    │  │ videotestsrc     │                                    │  │
    │    │  │   (simulated     │                                    │  │
    │    │  │    HDMI input)   │                                    │  │
    │    │  │                  │                                    │  │
    │    │  │ Simulated modems │                                    │  │
    │    │  │   (fake wwan0/1) │                                    │  │
    │    │  └──────────────────┘                                    │  │
    │    │                                                          │  │
    │    └──────────────────────────────────────────────────────────┘  │
    │                                                                  │
    └──────────────────────────────────────────────────────────────────┘
```

---

## 2. Docker-in-Docker in the Devcontainer

The workspace already runs inside a devcontainer. To run Docker Compose inside it,
we need Docker-in-Docker (DinD). Two approaches:

### Option A: Docker Socket Mount (Preferred)

The devcontainer mounts the host's Docker socket:

```jsonc
// .devcontainer/devcontainer.json
{
  "mounts": [
    "source=/var/run/docker.sock,target=/var/run/docker.sock,type=bind"
  ],
  "features": {
    "ghcr.io/devcontainers/features/docker-outside-of-docker:1": {}
  }
}
```

This lets us run `docker compose` inside the devcontainer, but the containers
actually run on the host Docker daemon. Port forwarding works naturally — VS Code
auto-forwards ports from the containers to the host.

### Option B: Full DinD (Fallback)

If the host socket isn't available:

```jsonc
{
  "features": {
    "ghcr.io/devcontainers/features/docker-in-docker:2": {}
  }
}
```

Runs a nested Docker daemon inside the devcontainer. Slightly more overhead,
but fully self-contained.

---

## 3. Docker Compose Layout

```
dev/
├── docker-compose.yml          # Defines all services
├── Dockerfile.control          # Builds strata-control binary + Leptos dashboard
├── Dockerfile.sender-sim       # Builds strata-agent with simulated hardware
├── seed.sql                    # Initial DB: test user, enrollment token
└── README.md                   # Quick-start instructions
```

### docker-compose.yml (Planned)

```yaml
version: "3.9"

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
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U strata"]
      interval: 5s
      timeout: 3s
      retries: 5

  strata-cloud:
    build:
      context: ..
      dockerfile: dev/Dockerfile.control
    depends_on:
      postgres:
        condition: service_healthy
    environment:
      DATABASE_URL: "postgres://strata:dev-only-password@postgres/strata"
      RUST_LOG: "info,strata_control=debug"
      LISTEN_ADDR: "0.0.0.0:3000"
      RIST_PORT_RANGE: "15000-15100"
    ports:
      - "3000:3000"           # Dashboard + API
      - "15000-15100:15000-15100/udp"  # RIST receiver ports
    # network_mode: host would be simpler for UDP but breaks
    # container DNS resolution. Port range mapping is fine for dev.

  strata-sender-sim:
    build:
      context: ..
      dockerfile: dev/Dockerfile.sender-sim
    depends_on:
      - strata-cloud
    environment:
      CONTROL_PLANE_URL: "ws://strata-cloud:3000/agent/ws"
      RUST_LOG: "info,strata_agent=debug"
      PORTAL_LISTEN_ADDR: "0.0.0.0:3001"
      # Simulated hardware
      SIM_MODEM_COUNT: "2"
      SIM_VIDEO_PATTERN: "smpte"    # GStreamer videotestsrc pattern
      SIM_RESOLUTION: "1280x720"
      SIM_FRAMERATE: "30"
    ports:
      - "3001:3001"           # Onboarding portal

volumes:
  pgdata:
```

### Dockerfile.control (Sketch)

```dockerfile
FROM rust:1.85-bookworm AS builder

# Install GStreamer dev libs
RUN apt-get update && apt-get install -y \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    libssl-dev pkg-config cmake meson ninja-build nasm

WORKDIR /build
COPY . .
RUN cargo build --release --bin strata-control --bin strata-receiver

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/strata-control /usr/local/bin/
COPY --from=builder /build/target/release/strata-receiver /usr/local/bin/

ENTRYPOINT ["strata-control"]
```

### Dockerfile.sender-sim (Sketch)

```dockerfile
FROM rust:1.85-bookworm AS builder

RUN apt-get update && apt-get install -y \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    libssl-dev pkg-config cmake meson ninja-build nasm

WORKDIR /build
COPY . .
RUN cargo build --release --bin strata-agent

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/strata-agent /usr/local/bin/

# The agent detects SIM_MODEM_COUNT env var and creates
# simulated modem interfaces instead of scanning for real hardware
ENTRYPOINT ["strata-agent", "--simulate"]
```

---

## 4. Simulated Hardware

The sender container doesn't have real cellular modems or HDMI input. The agent
runs in **simulation mode** (`--simulate` flag or `SIM_*` environment variables):

### Simulated Modems

```rust
// In strata-agent, when --simulate is set:
struct SimulatedModem {
    name: String,        // "wwan0", "wwan1"
    carrier: String,     // "Sim-Carrier-A", "Sim-Carrier-B"
    signal_dbm: i32,     // Random walk between -90..-50
    technology: String,  // "LTE", "5G"
    ip_address: String,  // "10.0.0.x" (Docker network)
}

impl SimulatedModem {
    fn update_signal(&mut self) {
        // Random walk ±3 dBm each tick, clamped to [-100, -40]
        // Simulates signal fluctuation for realistic dashboard display
    }
}
```

The agent reports these to the control plane exactly as it would report real
modems. The dashboard sees "2 modems connected, LTE, signal -65 dBm".

### Simulated Video Input

Instead of a real V4L2 device, the sender pipeline uses `videotestsrc`:

```
videotestsrc pattern=smpte ! video/x-raw,width=1280,height=720,framerate=30/1 \
  ! x264enc bitrate=4000 ! mpegtsmux ! rsristbondsink ...
```

The dashboard shows "HDMI input: 1280×720 @ 30fps" (simulated).

### Simulated Network Conditions (Optional)

For testing the bonding engine's behaviour, the dev compose stack can optionally
include a network impairment container:

```yaml
  # Optional: add to docker-compose.yml for impairment testing
  impairment:
    image: gaiadocker/iproute2
    cap_add: [NET_ADMIN]
    network_mode: "service:strata-sender-sim"
    entrypoint: >
      sh -c "tc qdisc add dev eth0 root netem delay 50ms 20ms loss 2%
             && sleep infinity"
```

This uses `tc netem` to add latency, jitter, and packet loss to the sender's
network — exactly what the bonding engine is designed to handle.

---

## 5. Seed Data

The `dev/seed.sql` pre-populates the database with a test user and enrollment
token so you can immediately log in and enroll the simulated sender:

```sql
-- Test user (email: admin@test.local, password: "password")
INSERT INTO users (id, email, password_hash, role, created_at)
VALUES (
  'usr_dev_001',
  'admin@test.local',
  '$argon2id$v=19$m=19456,t=2,p=1$...',  -- hash of "password"
  'admin',
  '2026-02-14T00:00:00Z'
);

-- Pre-generated enrollment token for the simulated sender
INSERT INTO senders (id, owner_id, name, enrollment_token_hash, created_at)
VALUES (
  'snd_sim_001',
  'usr_dev_001',
  'Dev Sender (Simulated)',
  '$argon2id$v=19$m=19456,t=2,p=1$...',  -- hash of "enr_dev_test_token"
  '2026-02-14T00:00:00Z'
);

-- Test destination (fake YouTube RTMP key)
INSERT INTO destinations (id, owner_id, platform, name, url, stream_key, created_at)
VALUES (
  'dst_dev_001',
  'usr_dev_001',
  'custom_rtmp',
  'Dev RTMP Sink (null)',
  'rtmp://localhost/dev',
  'test-key',
  '2026-02-14T00:00:00Z'
);
```

---

## 6. Developer Workflow

### First Time Setup

```bash
# Inside the devcontainer:
cd dev/
docker compose build          # Build both containers (~5 min first time)
docker compose up -d           # Start everything
docker compose logs -f         # Watch logs
```

### Access the UIs

| UI | URL | Purpose |
|---|---|---|
| Cloud Dashboard | `http://localhost:3000` | Log in as admin@test.local / password |
| Sender Portal | `http://localhost:3001` | Enroll the simulated sender |

Both ports are auto-forwarded by VS Code from the devcontainer to the host PC.

### Simulate a Full Broadcast

```
1. Open http://localhost:3001 (sender portal)
2. Enter enrollment token: "enr_dev_test_token"
3. See simulated modems and video input appear
4. Portal shows "Connected to cloud" ✓

5. Open http://localhost:3000 (cloud dashboard)
6. Log in as admin@test.local / password
7. See "Dev Sender (Simulated)" listed as ONLINE
8. Click "Start Broadcast"
   → Control plane allocates receiver ports
   → Sends config to sender agent via WSS
   → Sender starts videotestsrc → rsristbondsink
   → Receiver starts rsristbondsrc → (null sink in dev mode)
9. Dashboard shows live stats:
   - Per-link bitrate, RTT, packet loss
   - Simulated signal strength fluctuating
   - Stream uptime counter
10. Click "Stop Broadcast" → clean teardown
```

### Iterating on Code

For fast iteration during development, you don't always need to rebuild the Docker
images. Two approaches:

**Option A: Volume-mount the binaries (fastest)**

```yaml
# Override in docker-compose.override.yml:
services:
  strata-cloud:
    volumes:
      - ../target/release/strata-control:/usr/local/bin/strata-control
      - ../target/release/strata-receiver:/usr/local/bin/strata-receiver
```

Build locally with `cargo build --release`, then `docker compose restart strata-cloud`.

**Option B: Cargo workspace inside container (for cross-compilation testing)**

Mount the full source tree and build inside the container. Slower but tests the
exact build environment.

### Teardown

```bash
docker compose down            # Stop containers
docker compose down -v         # Stop + delete database volume
```

---

## 7. What This Tests End-to-End

| Feature | Tested? | Notes |
|---|---|---|
| User login + JWT auth | ✅ | Real auth flow with real tokens |
| Sender enrollment | ✅ | Captive portal → enrollment token → cloud registration |
| WSS agent ↔ control | ✅ | Real WebSocket connection between containers |
| Stream lifecycle | ✅ | Start → running → stop, full lifecycle |
| RIST bonding | ✅ | Real rsristbondsink/src with real UDP between containers |
| Live telemetry | ✅ | Real stats flowing from sender → control → dashboard |
| Database operations | ✅ | Real PostgreSQL queries via sqlx |
| Dashboard UI | ✅ | Real Leptos pages rendering in browser |
| Network impairment | 🔧 | Optional tc netem container for loss/jitter testing |
| TLS / HTTPS | ❌ | Dev mode uses HTTP. TLS tested separately or via Caddy sidecar |
| Multi-region | ❌ | Single-host only. Multi-region is a deployment concern |
| Real hardware (modems, HDMI) | ❌ | Simulated. Real hardware tested on actual ROCK 5B+ |

---

## 8. Relationship to Existing Tests

The Docker Compose dev environment is **not** a replacement for the existing test
suite (336 tests). It serves a different purpose:

| Test Type | Purpose | Runs In |
|---|---|---|
| Unit tests (`cargo test`) | Transport engine correctness | CI / local cargo |
| Integration tests (netsim) | Bonding under impairment | CI / local cargo |
| GStreamer tests | Plugin element tests | CI / local cargo |
| **Dev compose stack** | **Full platform UI/UX testing** | **Docker Compose** |

The compose stack is for interactive testing — seeing the dashboard, clicking
buttons, watching stats flow. It's the "does this actually feel like a product"
test, not a correctness test.

---

## 9. Future: CI Smoke Test

Once the compose stack is working, add a CI job that:

1. `docker compose up -d`
2. Waits for health checks
3. Hits the API with `curl` to verify:
   - Login works
   - Sender appears online
   - Stream start/stop works
4. `docker compose down`

This catches integration regressions that unit tests might miss.

---

*Related documents:*
- [01-architecture-overview.md](01-architecture-overview.md) — System design and deployment model
- [04-sender-agent.md](04-sender-agent.md) — Agent design, including simulation mode
- [06-technology-choices.md](06-technology-choices.md) — Why Docker Compose, PostgreSQL, Leptos
