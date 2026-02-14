# dev/ — Local Docker Compose Stack

Quick-start for validating the full containerised platform.

## Usage

```bash
# Build and start everything
make up

# Tail logs
make logs

# Tear down (preserves data volume)
make down

# Tear down and delete all data
make clean
```

## Services

| Service | Port | Description |
|---|---|---|
| `postgres` | 5432 | PostgreSQL 16 with seed data |
| `strata-control` | 3000 | Control plane (API + WSS + dashboard) |
| `strata-sender-sim` | 3001 | Simulated sender agent |

## Seed Data

On startup with `DEV_SEED=1`, `strata-control` inserts:
- Admin user: `admin@strata.local` / `admin`
- Pre-registered sender: `sim-sender-01`
- Test destination: `rtmp://localhost/live/test`

## Notes

- This runs inside Docker-in-Docker within the devcontainer
- For daily development, use `cargo run` directly — much faster iteration
- See [docs/platform/08-local-dev-environment.md](../docs/platform/08-local-dev-environment.md)
