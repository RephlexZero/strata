# Strata — Project Guidelines

Bonded video transport and live-streaming management platform in Rust. Aggregates bandwidth across multiple network interfaces (cellular, WiFi, Ethernet, satellite) into a single resilient video stream via GStreamer plugin + full management platform.

## Architecture

```
strata-gst ──▶ strata-bonding ──▶ strata-transport
strata-control ──▶ strata-common ◀── strata-agent
strata-dashboard (WASM, talks to control via HTTP/WS)
strata-portal (WASM, talks to agent via HTTP)
strata-sim (test infra — Linux network namespaces)
```

| Crate | Role | Runtime |
|---|---|---|
| `strata-transport` | Custom wire protocol (12B header, VarInt seq), FEC+ARQ, congestion control | `monoio` (io_uring) |
| `strata-bonding` | Multi-link scheduler (DWRR), receiver aggregator, encoder adaptation | `monoio` (io_uring) |
| `strata-gst` | GStreamer plugin (`stratasink`/`stratasrc` elements) + `strata-node` CLI binary | GStreamer threads |
| `strata-sim` | Linux netns + tc-netem impairment for tests | sync |
| `strata-common` | Shared types, protocol messages, auth (JWT+Ed25519), prefixed UUIDv7 IDs | — |
| `strata-control` | Axum REST API, WebSocket hubs, PostgreSQL, serves dashboard SPA | `tokio` |
| `strata-agent` | Field device daemon — hardware scan, interface mgmt, stream lifecycle | `tokio` |
| `strata-dashboard` | Operator web UI — Leptos 0.7 CSR + Tailwind/DaisyUI | WASM |
| `strata-portal` | Field device web UI — enrollment, config, connectivity test | WASM |

### Key design decisions

- **Two async runtimes**: `monoio` (io_uring) for the hot-path transport/bonding, `tokio` for the control plane. Do not mix them.
- **Lock-free hot path**: Use `rtrb` (SPSC ring buffers), `arc-swap`, slab allocation—no `Mutex` on packet paths.
- **Zero-copy**: `bytes::Bytes` throughout the transport pipeline.
- **GStreamer bridge**: `crossbeam-channel` between GStreamer streaming threads and Rust async context. Callbacks must be non-blocking (channel send only).
- **ID convention**: UUIDv7 with type prefix (`usr_`, `snd_`, `str_`, `dst_`). Generated in `strata-common::ids`.
- **Linux-only**: Network namespaces, io_uring, `SO_BINDTODEVICE`—no Windows support.

## Build and Test

```bash
cargo build                                        # Debug build
cargo build --release                              # LTO + single codegen-unit + panic=abort
cargo build --release -p strata-gst                # Plugin only → target/release/libgststrata.so

# Linting (CI enforces these)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Tests
cargo test --workspace --lib                       # Unit tests (no privileges)
cargo test -p strata-common                        # Shared model + protocol tests
cargo test -p strata-control                       # API integration (needs PostgreSQL via TEST_DATABASE_URL)
sudo cargo test -p strata-gst --test end_to_end    # Transport E2E (needs NET_ADMIN for netns)

# WASM crates (separate target)
cargo check -p strata-dashboard --target wasm32-unknown-unknown
trunk build --release                              # Build dashboard/portal WASM bundles

# Cross-compile for aarch64
docker build -f docker/Dockerfile.cross-aarch64 -o dist/ .

# Dev stack
cd dev && make up                                  # PostgreSQL + control + simulated sender
# Dashboard: http://localhost:3000  (dev@strata.local / development)
```

### Test gotchas

- GStreamer E2E tests require root/sudo (network namespace creation). CI uses `continue-on-error: true`.
- Control plane tests need PostgreSQL. Set `TEST_DATABASE_URL` or tests gracefully skip.
- WASM crates won't compile with the default native target—use `--target wasm32-unknown-unknown`.
- Dev seeding (`DEV_SEED=1`) inserts test user `dev@strata.local` / `development`.

## Conventions

### Error handling

- **Application level** (transport, bonding, agent/control `main`): `anyhow::Result`
- **Library APIs** (control, agent, common): `thiserror` enums

### Async patterns

- `tokio` for control plane and agent. `monoio` for transport/bonding hot path.
- GStreamer ↔ Rust bridge: `crossbeam-channel` or tokio `mpsc`. Never block GStreamer streaming threads.
- State sharing (control): `AppState` wrapping `Arc<Inner>` with `DashMap`.
- State sharing (agent): `Arc<AgentState>` with `tokio::sync::Mutex`.

### Logging

- `tracing` + `tracing-subscriber` with env-filter everywhere.
- Bonding crate: `init()` installs subscriber guarded by `std::sync::Once`.

### Database (strata-control)

- PostgreSQL 16 via `sqlx` 0.8 with embedded migrations (`sqlx::migrate!("./migrations")`).
- Auth: Argon2id password hashing, Ed25519 JWT signing.
- Multi-tenancy: `owner_id` FK on all resources, enforced in queries.

### Frontend (dashboard/portal)

- Leptos 0.7 CSR, built with Trunk, Tailwind CSS + DaisyUI.
- HTTP via `gloo-net`, WebSocket via `web-sys`, JWT stored in `gloo-storage::LocalStorage`.
- Dashboard is served by `strata-control` via `tower_http::services::ServeDir` (SPA fallback).

### GStreamer plugin

- Elements: `stratasink` (sender), `stratasrc` (receiver) with request pads (`link_%u`).
- `strata-node` binary: `strata-node sender|receiver` subcommands.
- Plugin env: `GST_PLUGIN_PATH="$PWD/target/release:$GST_PLUGIN_PATH"`.
- Stats exported as GStreamer bus messages (`strata-stats`) and UDP JSON telemetry.

### Formatting and linting

- Default `rustfmt` and `clippy` settings (no config files). CI runs both with `-D warnings`.

## Documentation

Full docs live in the [Wiki](https://github.com/RephlexZero/strata/wiki). Key pages:
- [Architecture](wiki/Architecture.md) — system design and component overview
- [Configuration Reference](wiki/Configuration-Reference.md) — TOML config for links, scheduler, receiver
- [GStreamer Elements](wiki/GStreamer-Elements.md) — `stratasink`/`stratasrc` property reference
- [Testing](wiki/Testing.md) — full test matrix and privilege requirements
- [Deployment](wiki/Deployment.md) — privileges, performance budgets, operational guidance

Detailed transport/bonding design: see [SPECIFICATION.md](SPECIFICATION.md) and [docs/MASTER_PLAN.md](docs/MASTER_PLAN.md).
