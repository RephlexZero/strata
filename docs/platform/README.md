# Strata Cloud Platform — Design Documents

> **Status:** Architecture spec — **Phase 1 implementation in progress**. The transport layer is complete and proven.
> These documents spec the managed platform that will wrap it into a multi-tenant streaming service.

---

## Documents

| # | Document | Summary |
|---|---|---|
| 01 | [Architecture Overview](01-architecture-overview.md) | High-level system design: sender agents, control plane, receiver workers, web dashboard. Deployment model comparison (process vs Docker vs k8s). Scaling estimates and phased build plan. |
| 02 | [Control Protocol](02-control-protocol.md) | WebSocket protocol between sender agent and control plane. REST API for the dashboard. Message types, authentication flow, port allocation, error handling. |
| 03 | [Security Model](03-security-model.md) | Threat model, video encryption (RIST PSK / DTLS), control channel security (TLS + JWT), device identity, secrets management, firewall rules. |
| 04 | [Sender Agent](04-sender-agent.md) | Design of the `strata-agent` daemon: hardware discovery, pipeline management, systemd integration, state machine, self-update. |
| 05 | [Receiver Workers](05-receiver-workers.md) | Receiver worker lifecycle, forwarding pipeline variants (RTMP, SRT, HLS, record), resource management, health checks, multi-host scaling. |
| 06 | [Technology Choices](06-technology-choices.md) | Trade-off analysis: language, deployment model, database, auth, real-time updates, monitoring. Rationale for each decision. |
| 07 | [Hardware Evaluation](07-hardware-evaluation.md) | SBC comparison (ROCK 5B+, Orange Pi 5 Plus, ROCK 5 ITX, RPi 5). HDMI input approaches (native vs USB capture). Bill of materials. Thermal and VPU considerations. |
| 08 | [Local Dev Environment](08-local-dev-environment.md) | Dev mode (process-based, `cargo run`) for daily iteration. Deploy mode (Docker Compose via DinD) for pre-release validation. Build order and implementation steps. |

---

## Key Architectural Decisions

| Decision | Choice | Doc |
|---|---|---|
| Deployment model | Docker Compose + process-per-stream inside | [01](01-architecture-overview.md#5-deployment-model-decision), [06](06-technology-choices.md#2-deployment-docker-compose--process-per-stream-inside) |
| Repo structure | Monorepo with new workspace crates | [01](01-architecture-overview.md#4-repo-structure-decision) |
| Video encryption | RIST PSK (AES-256) via librist | [03](03-security-model.md#2-video-encryption) |
| Backend framework | Rust + axum | [06](06-technology-choices.md#1-language) |
| Frontend framework | Leptos (Rust/WASM) — web page, no native apps | [06](06-technology-choices.md#1-language) |
| Database | PostgreSQL from day one | [06](06-technology-choices.md#3-database) |
| Agent ↔ Cloud | WebSocket (WSS), outbound from agent | [02](02-control-protocol.md#1-transport) |
| Sender hardware | Radxa ROCK 5B+ (primary), OPi5+ (alt) | [07](07-hardware-evaluation.md#4-recommendation-matrix) |
| HDMI input | USB capture card (v1), native HDMI RX (future) | [07](07-hardware-evaluation.md#2-hdmi-input-native-vs-usb-capture) |
| First-time setup | AP Wi-Fi captive portal on device | [04 §9](04-sender-agent.md#9-ap-wi-fi-onboarding-first-time-setup) |
| Local dev testing | Process-based (`cargo run`) + DinD for deploy testing | [08](08-local-dev-environment.md) |

---

## Open Questions

See [01 §8](01-architecture-overview.md#8-open-questions) for the full list.

---

## Build Phases

| Phase | Scope | Status |
|---|---|---|
| 0 | Transport engine + GStreamer plugin | ✅ Done (336 tests) |
| 1a | `strata-common` — shared types, auth, protocol, DB models | 🔄 In progress |
| 1b | `strata-control` — axum API, sqlx, WSS, receiver spawner | Next |
| 1c | `strata-agent` — daemon, WSS client, hardware sim, pipeline | Next |
| 2 | Leptos dashboard — login, sender list, live stats | After 1b |
| 3 | Docker Compose dev stack + deploy-mode validation | After 1c |
| 4 | Production hardening, monitoring, TLS | After 3 |
| 5 | Multi-region VPS deployment | When ready to sell |
