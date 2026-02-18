# dev/ — Local Docker Compose Stack

Quick-start for validating the full containerised platform.

## Usage

```bash
# Build and start everything (from project root)
docker compose up --build -d

# Tail logs
docker compose logs -f

# Tear down (preserves data volume)
docker compose down

# Tear down and delete all data
docker compose down -v --remove-orphans
```

## Web Development (Hot Reload)

Iterate on the dashboard or portal with live hot-reload — no manual
builds or restarts needed.  Source files are bind-mounted and Trunk
watches for changes, automatically rebuilding WASM and refreshing the
browser.

```bash
# Start the full stack + dashboard dev server on :8080
docker compose --profile web-dev up --build dashboard-dev

# Start the full stack + portal dev server on :8081
docker compose --profile web-dev up --build portal-dev

# Start both web dev servers at once
docker compose --profile web-dev up --build dashboard-dev portal-dev
```

API and WebSocket requests are proxied to the backend services
automatically — the WASM apps work identically to production.

| Dev Server | URL | Proxies to |
|---|---|---|
| `dashboard-dev` | http://localhost:8080 | `strata-control:3000` |
| `portal-dev` | http://localhost:8081 | `strata-sender-sim:3001` |

## Rust Binary Iteration (Dev Overlay)

For iterating on the Rust services, use the dev overlay which
bind-mounts host-built debug binaries into the containers:

```bash
# Start with dev overlay
docker compose -f docker-compose.yml -f dev/docker-compose.dev.yml up -d

# Rebuild and restart a single service
cargo build -p strata-control && docker compose restart strata-control
cargo build -p strata-agent  && docker compose restart strata-sender-sim
cargo build -p strata-gst    && docker compose restart strata-sender-sim strata-receiver
```

## Services

| Service | Port | Description |
|---|---|---|
| `postgres` | 5432 | PostgreSQL 16 with seed data |
| `strata-control` | 3000 | Control plane (API + WSS + dashboard) |
| `strata-sender-sim` | 3001 | Simulated sender agent |
| `strata-receiver` | 5000-5004/udp | Bonded transport receiver |
| `dashboard-dev` | 8080 | Dashboard hot-reload (profile: web-dev) |
| `portal-dev` | 8081 | Portal hot-reload (profile: web-dev) |

## Seed Data

On startup with `DEV_SEED=1`, `strata-control` inserts:
- Admin user: `admin@strata.local` / `admin`
- Pre-registered sender: `sim-sender-01`
- Test destination: `rtmp://localhost/live/test`

## Notes

- This runs inside Docker-in-Docker within the devcontainer
- For daily Rust development, use `cargo run` directly — much faster iteration
- Web dev profiles give trunk hot-reload without leaving Docker
