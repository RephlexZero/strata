#!/bin/bash
# ── Fast Reload — build on host, restart containers ────────────────
#
# DEPRECATED: Prefer the built-in Docker workflows instead:
#
#   Web development (hot-reload, no builds needed):
#     docker compose --profile web-dev up dashboard-dev   # :8080
#     docker compose --profile web-dev up portal-dev      # :8081
#
#   Rust binary iteration (with dev overlay):
#     cargo build -p strata-control && docker compose restart strata-control
#     cargo build -p strata-agent  && docker compose restart strata-sender-sim
#     cargo build -p strata-gst    && docker compose restart strata-sender-sim strata-receiver
#
# This script is kept for backwards compatibility only.
#
# Prerequisites:
#   - Containers running with dev overlay:
#     docker compose -f docker-compose.yml -f dev/docker-compose.dev.yml up -d

set -e
cd "$(dirname "$0")/.."

COMPOSE="docker compose -f docker-compose.yml -f dev/docker-compose.dev.yml"

TARGET=${1:-all}

case "$TARGET" in
    agent)
        cargo build -p strata-agent
        $COMPOSE restart strata-sender-sim
        ;;
    control)
        cargo build -p strata-control
        $COMPOSE restart strata-control
        ;;
    pipeline)
        cargo build -p strata-gst
        $COMPOSE restart strata-sender-sim strata-receiver
        ;;
    all|"")
        cargo build -p strata-agent -p strata-control -p strata-gst
        $COMPOSE restart strata-sender-sim strata-receiver strata-control
        ;;
    *)
        echo "Usage: $0 [agent|control|pipeline|all]"
        exit 1
        ;;
esac

echo "Done. Use 'docker compose logs -f <service>' to tail logs."
