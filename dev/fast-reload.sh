#!/bin/bash
# ── Fast Reload — build on host, restart containers ────────────────
#
# Usage:
#   ./dev/fast-reload.sh              # rebuild everything
#   ./dev/fast-reload.sh agent        # rebuild agent only, restart sender
#   ./dev/fast-reload.sh control      # rebuild control plane only
#   ./dev/fast-reload.sh pipeline     # rebuild integration_node + bonding plugin
#   ./dev/fast-reload.sh all          # rebuild all, restart all
#
# Prerequisites:
#   - Containers already running (docker compose up -d)
#   - Host has matching Debian 12 bookworm + GStreamer 1.22 (devcontainer)
#
# This skips the Docker multi-stage build entirely. Instead it:
#   1. Builds the Rust crate(s) on the host (incremental, ~2-8 seconds)
#   2. Restarts the container — which picks up the newly-mounted binary
#
# First run: you need to start containers with the dev overlay:
#   docker compose -f docker-compose.yml -f dev/docker-compose.dev.yml up -d

set -e
cd "$(dirname "$0")/.."

COMPOSE="docker compose -f docker-compose.yml -f dev/docker-compose.dev.yml"

TARGET=${1:-all}

build_agent() {
    echo ">>> Building strata-agent..."
    cargo build -p strata-agent
}

build_control() {
    echo ">>> Building strata-control..."
    cargo build -p strata-control
}

build_pipeline() {
    echo ">>> Building gst-rist-bonding (integration_node + plugin)..."
    cargo build -p gst-rist-bonding
}

restart_sender() {
    echo ">>> Restarting strata-sender-sim..."
    $COMPOSE restart strata-sender-sim
}

restart_receiver() {
    echo ">>> Restarting strata-receiver..."
    $COMPOSE restart strata-receiver
}

restart_control() {
    echo ">>> Restarting strata-control..."
    $COMPOSE restart strata-control
}

case "$TARGET" in
    agent)
        build_agent
        restart_sender
        ;;
    control)
        build_control
        restart_control
        ;;
    pipeline)
        build_pipeline
        restart_sender
        restart_receiver
        ;;
    all|"")
        build_agent
        build_control
        build_pipeline
        restart_sender
        restart_receiver
        restart_control
        ;;
    *)
        echo "Usage: $0 [agent|control|pipeline|all]"
        exit 1
        ;;
esac

echo ""
echo "Done. Containers restarted with host-built binaries."
echo "Use 'docker compose logs -f <service>' to tail logs."
