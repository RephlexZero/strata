# dev/ — Development Workflows

All commands run from the **project root** (`/workspaces/strata`).

## Native Dev (fastest — auto-restart on save)

Run Rust services directly with `cargo watch`. Only Postgres and the
receiver stay in Docker. Edit → save → auto-rebuild+restart (~5s).

```bash
# 1. Start infrastructure (once):
docker compose up -d postgres strata-receiver

# 2. Terminal 1 — control plane:
cargo watch -x 'run -p strata-control'

# 3. Terminal 2 — sender agent:
cargo watch -x 'run -p strata-agent -- --control-url ws://localhost:3000/agent/ws --enrollment-token DEV1-TEST --hostname sim-sender-01'
```

Environment variables are preset in `.cargo/config.toml` — no env vars
needed on the command line.

Dashboard: http://localhost:3000 — Login: `dev@strata.local` / `development`

## Docker Dev (full stack in containers)

Host-compiled binaries are bind-mounted into containers via
`docker-compose.override.yml` (auto-loaded by Docker Compose).

```bash
# First time (force-recreate ensures clean container networking):
cargo build -p strata-gst -p strata-agent -p strata-control
docker compose up -d --force-recreate

# After code changes (~5s build + ~2s restart):
cargo build -p strata-gst -p strata-agent -p strata-control
docker compose restart -t 1 strata-sender-sim strata-receiver strata-control
```

## Web Development (Hot Reload)

Live-reloading Trunk dev servers — edit `.rs` / `.css` files and the
browser refreshes automatically.

```bash
docker compose --profile web-dev up --build dashboard-dev   # :8080
docker compose --profile web-dev up --build portal-dev      # :8081
```

| Dev Server | URL | Proxies to |
|---|---|---|
| `dashboard-dev` | http://localhost:8080 | `strata-control:3000` |
| `portal-dev` | http://localhost:8081 | `strata-sender-sim:3001` |

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
- `docker-compose.override.yml` is auto-loaded by `docker compose` — no `-f` flags needed
- Host binaries are ABI-compatible with containers (both Debian, GStreamer 1.28)
- Web dev profiles give trunk hot-reload without leaving Docker
