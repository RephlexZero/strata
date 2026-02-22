.PHONY: up down logs clean build ps web-dev web-up dev restart infra images

COMPOSE = docker compose -f ../docker-compose.yml

# ── Fast Dev Workflow ──────────────────────────────────────────────
#
# Build locally, restart containers — no image rebuild needed.
# docker-compose.override.yml bind-mounts target/debug/ binaries.

# Build all Rust binaries + plugin locally
build:
	cargo build -p strata-gst -p strata-agent -p strata-control

# Start Postgres + receiver (containers that don't change often)
infra:
	$(COMPOSE) up -d postgres strata-receiver

# Full dev stack: build, start everything (force-recreate ensures clean networking)
dev: build
	$(COMPOSE) up -d --force-recreate

# After code changes: rebuild + restart affected containers (~3s)
restart: build
	$(COMPOSE) restart -t 1 strata-sender-sim strata-receiver strata-control

# ── Docker Image Builds (only needed for CI / first setup) ────────

# Build images from scratch (slow — compiles Rust inside Docker)
images:
	$(COMPOSE) build --no-cache

# Build images and start all services
up:
	$(COMPOSE) up --build -d

# ── Web Development (hot-reload) ──────────────────────────────────

# Start web dev hot-reload servers (dashboard on :8080, portal on :8081)
web-dev:
	$(COMPOSE) --profile web-dev up --build dashboard-dev portal-dev

# Restart web dev servers without rebuilding
web-up:
	$(COMPOSE) --profile web-dev up dashboard-dev portal-dev

# ── Lifecycle ─────────────────────────────────────────────────────

# Stop services (preserves data volume)
down:
	$(COMPOSE) down

# Tail logs for all services
logs:
	$(COMPOSE) logs -f

# Stop and delete everything including data
clean:
	$(COMPOSE) down -v --remove-orphans

# Show running containers
ps:
	$(COMPOSE) ps
