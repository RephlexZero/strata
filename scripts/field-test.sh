#!/usr/bin/env bash
# scripts/field-test.sh — End-to-end bonded cellular streaming test
#
# Uses environment variables for all device-specific configuration so no
# personal info (IPs, devices, credentials) is committed to the repo.
#
# Required env vars:
#   STRATA_RECEIVER_HOST   — SSH alias or IP of the receiver server
#   STRATA_RECEIVER_PORTS  — Comma-separated bind ports (e.g. "5000,5002")
#   STRATA_RELAY_URL       — YouTube HLS upload URL (or RTMP URL)
#
# Optional env vars:
#   STRATA_VIDEO_DEVICE    — V4L2 device path (default: /dev/video0)
#   STRATA_VIDEO_SOURCE    — Source mode: v4l2, test (default: test)
#   STRATA_RESOLUTION      — WxH (default: 640x360)
#   STRATA_FRAMERATE       — FPS (default: 30)
#   STRATA_CODEC           — h264 or h265 (default: h265)
#   STRATA_BITRATE         — Target kbps (default: 500)
#   STRATA_MIN_BITRATE     — Min kbps (default: 200)
#   STRATA_MAX_BITRATE     — Max kbps (default: 1500)
#   STRATA_LINK_IFACES     — Comma-separated interfaces (e.g. "enp2s0f0u4,enp2s0f0u3")
#   STRATA_MAX_LATENCY_MS  — Receiver jitter buffer ceiling (default: 3000)
#   STRATA_DURATION_SECS   — How long to stream before stopping (default: 60)
#
# Usage:
#   export STRATA_RECEIVER_HOST=MyServer
#   export STRATA_RECEIVER_PORTS="5000,5002"
#   export STRATA_RELAY_URL="https://a.upload.youtube.com/http_upload_hls?cid=...&copy=0&file="
#   export STRATA_LINK_IFACES="enp2s0f0u4,enp2s0f0u3"
#   ./scripts/field-test.sh

set -euo pipefail

# ── Load .env from project root if present ───────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$SCRIPT_DIR/../.env"
if [[ -f "$ENV_FILE" ]]; then
    set -a
    # shellcheck disable=SC1090
    source "$ENV_FILE"
    set +a
fi

# ── Colours ──────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()  { echo -e "${GREEN}[✓]${NC} $*"; }
warn()  { echo -e "${YELLOW}[!]${NC} $*"; }
fail()  { echo -e "${RED}[✗]${NC} $*"; exit 1; }

# ── Required env vars ────────────────────────────────────────────────
[[ -z "${STRATA_RECEIVER_HOST:-}" ]] && fail "STRATA_RECEIVER_HOST is not set"
[[ -z "${STRATA_RECEIVER_PORTS:-}" ]] && fail "STRATA_RECEIVER_PORTS is not set"
[[ -z "${STRATA_RELAY_URL:-}" ]]      && fail "STRATA_RELAY_URL is not set"

# ── Defaults ─────────────────────────────────────────────────────────
VIDEO_DEVICE="${STRATA_VIDEO_DEVICE:-/dev/video0}"
VIDEO_SOURCE="${STRATA_VIDEO_SOURCE:-test}"
RESOLUTION="${STRATA_RESOLUTION:-640x360}"
FRAMERATE="${STRATA_FRAMERATE:-30}"
CODEC="${STRATA_CODEC:-h265}"
BITRATE="${STRATA_BITRATE:-500}"
MIN_BITRATE="${STRATA_MIN_BITRATE:-200}"
MAX_BITRATE="${STRATA_MAX_BITRATE:-1500}"
MAX_LATENCY_MS="${STRATA_MAX_LATENCY_MS:-3000}"
DURATION="${STRATA_DURATION_SECS:-60}"
HOST="${STRATA_RECEIVER_HOST}"

# Parse ports and interfaces
IFS=',' read -ra PORTS <<< "${STRATA_RECEIVER_PORTS}"
IFS=',' read -ra IFACES <<< "${STRATA_LINK_IFACES:-}"

NUM_LINKS=${#PORTS[@]}

if [[ ${#IFACES[@]} -gt 0 && ${#IFACES[@]} -ne "$NUM_LINKS" ]]; then
    fail "STRATA_LINK_IFACES has ${#IFACES[@]} entries but STRATA_RECEIVER_PORTS has $NUM_LINKS"
fi

# ── Pre-flight checks ───────────────────────────────────────────────
echo "═══ Strata Field Test ═══"
echo ""

# 1. Binary exists
command -v strata-pipeline >/dev/null 2>&1 || fail "strata-pipeline not found in PATH"
info "strata-pipeline binary found"

# 2. CAP_NET_RAW check (if interfaces are specified)
if [[ ${#IFACES[@]} -gt 0 ]]; then
    BINARY_PATH=$(command -v strata-pipeline)
    if getcap "$BINARY_PATH" 2>/dev/null | grep -q cap_net_raw; then
        info "cap_net_raw is set on $BINARY_PATH"
    else
        warn "cap_net_raw NOT set on $BINARY_PATH — SO_BINDTODEVICE will fail"
        warn "Run: sudo setcap cap_net_raw+ep $BINARY_PATH"
    fi
fi

# 3. GStreamer plugin
GST_PLUGIN_PATH="${GST_PLUGIN_PATH:-$HOME/.local/share/gstreamer-1.0/plugins}"
export GST_PLUGIN_PATH
if [[ -f "$GST_PLUGIN_PATH/libgststrata.so" ]]; then
    info "GStreamer plugin found at $GST_PLUGIN_PATH/libgststrata.so"
else
    fail "libgststrata.so not found in $GST_PLUGIN_PATH"
fi

# 4. Check video device (if v4l2)
if [[ "$VIDEO_SOURCE" == "v4l2" ]]; then
    if [[ -e "$VIDEO_DEVICE" ]]; then
        info "Video device $VIDEO_DEVICE exists"
    else
        fail "Video device $VIDEO_DEVICE does not exist"
    fi
fi

# 5. Check no duplicate interfaces (snag #15)
if [[ ${#IFACES[@]} -gt 0 ]]; then
    SORTED_IFACES=($(printf '%s\n' "${IFACES[@]}" | sort))
    for ((i=1; i<${#SORTED_IFACES[@]}; i++)); do
        if [[ "${SORTED_IFACES[$i]}" == "${SORTED_IFACES[$((i-1))]}" ]]; then
            fail "Duplicate interface '${SORTED_IFACES[$i]}' in STRATA_LINK_IFACES — each link needs its own interface"
        fi
    done
    info "No duplicate interfaces"
fi

# 6. Check interfaces can reach the receiver (routing)
RECEIVER_IP=$(echo "${PORTS[0]}" | sed 's/:.*//')  # Won't work if just port numbers
if [[ ${#IFACES[@]} -gt 0 ]]; then
    for iface in "${IFACES[@]}"; do
        if ip link show "$iface" >/dev/null 2>&1; then
            info "Interface $iface exists"
        else
            fail "Interface $iface does not exist"
        fi
    done
fi

# 7. Check SSH connectivity to receiver
if ssh -o ConnectTimeout=5 "$HOST" "echo ok" >/dev/null 2>&1; then
    info "SSH to $HOST is reachable"
else
    fail "Cannot SSH to $HOST"
fi

# ── Generate TOML configs ───────────────────────────────────────────
echo ""
echo "── Generating configs ──"

SENDER_TOML=$(mktemp /tmp/strata-sender-XXXXXX.toml)
RECEIVER_TOML=$(mktemp /tmp/strata-receiver-XXXXXX.toml)

# Sender TOML
{
    for ((i=0; i<NUM_LINKS; i++)); do
        echo "[[links]]"
        echo "id = $i"
        # Resolve receiver IP — user may pass just port numbers
        PORT="${PORTS[$i]}"
        # If port contains ":", it already has an IP
        if [[ "$PORT" == *:* ]]; then
            echo "uri = \"$PORT\""
        else
            # Need STRATA_RECEIVER_IP for this case
            RECEIVER_IP=$(ssh -o ConnectTimeout=5 "$HOST" "hostname -I | awk '{print \$1}'" 2>/dev/null || echo "")
            [[ -z "$RECEIVER_IP" ]] && fail "Cannot resolve receiver IP — use full host:port in STRATA_RECEIVER_PORTS"
            echo "uri = \"${RECEIVER_IP}:${PORT}\""
        fi
        if [[ ${#IFACES[@]} -gt 0 ]]; then
            echo "interface = \"${IFACES[$i]}\""
        fi
        echo ""
    done

    echo "[scheduler]"
    # Disable critical_broadcast for LTE (snag #16) — IDR duplication
    # causes burst congestion on constrained links.
    echo "redundancy_enabled = false"
    echo "critical_broadcast = false"
    echo "failover_enabled = true"
    echo "failover_duration_ms = 3000"
} > "$SENDER_TOML"

# Receiver TOML
{
    echo "[receiver]"
    echo "start_latency_ms = 500"
    echo "buffer_capacity = 4096"
    echo ""
    echo "[scheduler]"
    echo "max_latency_ms = $MAX_LATENCY_MS"
    echo "jitter_latency_multiplier = 4.0"
} > "$RECEIVER_TOML"

info "Sender config: $SENDER_TOML"
info "Receiver config: $RECEIVER_TOML"
echo ""
cat "$SENDER_TOML"
echo ""

# ── Build bind string for receiver ──────────────────────────────────
BIND_STR=""
for port in "${PORTS[@]}"; do
    if [[ "$port" == *:* ]]; then
        # Already has host:port — replace host with 0.0.0.0
        PORT_NUM="${port##*:}"
        BIND_STR="${BIND_STR:+$BIND_STR,}0.0.0.0:$PORT_NUM"
    else
        BIND_STR="${BIND_STR:+$BIND_STR,}0.0.0.0:$port"
    fi
done

# ── Deploy and start receiver ───────────────────────────────────────
echo "── Starting receiver on $HOST ──"

# Kill any existing receiver
ssh "$HOST" "kill \$(pgrep strata-pipeline) 2>/dev/null; sleep 1; echo ok" 2>/dev/null || true

# Copy receiver config
scp -q "$RECEIVER_TOML" "$HOST:/tmp/strata-receiver.toml"

# Write receiver start script (avoids SSH quoting issues — snag #11)
RECEIVER_SCRIPT=$(mktemp /tmp/strata-receiver-start-XXXXXX.sh)
cat > "$RECEIVER_SCRIPT" << ENDSCRIPT
#!/bin/bash
export GST_PLUGIN_PATH=\$HOME/.local/share/gstreamer-1.0/plugins
nohup /usr/local/bin/strata-pipeline receiver \\
  --bind "$BIND_STR" \\
  --relay-url "$STRATA_RELAY_URL" \\
  --codec "$CODEC" \\
  --config /tmp/strata-receiver.toml \\
  > /tmp/strata-receiver.log 2>&1 < /dev/null &
echo "PID: \$!"
disown
ENDSCRIPT

scp -q "$RECEIVER_SCRIPT" "$HOST:/tmp/start-receiver.sh"
ssh "$HOST" "chmod +x /tmp/start-receiver.sh && bash /tmp/start-receiver.sh"
sleep 2

RECEIVER_PID=$(ssh "$HOST" "pgrep -n strata-pipeline" 2>/dev/null || echo "")
if [[ -z "$RECEIVER_PID" ]]; then
    fail "Receiver failed to start — check $HOST:/tmp/strata-receiver.log"
fi
info "Receiver started (PID $RECEIVER_PID)"

# ── Build dest string for sender ────────────────────────────────────
DEST_STR=""
for port in "${PORTS[@]}"; do
    if [[ "$port" == *:* ]]; then
        DEST_STR="${DEST_STR:+$DEST_STR,}$port"
    else
        RECEIVER_IP=$(ssh -o ConnectTimeout=5 "$HOST" "hostname -I | awk '{print \$1}'" 2>/dev/null)
        DEST_STR="${DEST_STR:+$DEST_STR,}${RECEIVER_IP}:${port}"
    fi
done

# ── Start sender ────────────────────────────────────────────────────
echo ""
echo "── Starting sender (${VIDEO_SOURCE}, ${RESOLUTION}, ${BITRATE}kbps) ──"

SENDER_ARGS=(
    sender
    --dest "$DEST_STR"
    --source "$VIDEO_SOURCE"
    --resolution "$RESOLUTION"
    --framerate "$FRAMERATE"
    --codec "$CODEC"
    --bitrate "$BITRATE"
    --min-bitrate "$MIN_BITRATE"
    --max-bitrate "$MAX_BITRATE"
    --audio
    --config "$SENDER_TOML"
)

if [[ "$VIDEO_SOURCE" == "v4l2" ]]; then
    SENDER_ARGS+=(--device "$VIDEO_DEVICE")
fi

strata-pipeline "${SENDER_ARGS[@]}" > /tmp/strata-sender.log 2>&1 &
SENDER_PID=$!
info "Sender started (PID $SENDER_PID)"

# ── Monitor ─────────────────────────────────────────────────────────
echo ""
echo "── Streaming for ${DURATION}s — monitoring every 5s ──"

HLS_DIR="/dev/shm/strata-hls-rx-${RECEIVER_PID}"
ELAPSED=0
SEGMENT_COUNT=0

cleanup() {
    echo ""
    echo "── Shutting down ──"
    kill "$SENDER_PID" 2>/dev/null; wait "$SENDER_PID" 2>/dev/null || true
    ssh "$HOST" "kill \$(pgrep strata-pipeline) 2>/dev/null; echo ok" 2>/dev/null || true
    rm -f "$SENDER_TOML" "$RECEIVER_TOML" "$RECEIVER_SCRIPT"
    echo ""

    if [[ $SEGMENT_COUNT -gt 2 ]]; then
        info "SUCCESS: $SEGMENT_COUNT HLS segments produced and uploaded"
    elif [[ $SEGMENT_COUNT -gt 0 ]]; then
        warn "PARTIAL: Only $SEGMENT_COUNT segment(s) produced"
    else
        fail "FAILED: No HLS segments produced"
    fi
}
trap cleanup EXIT INT TERM

while [[ $ELAPSED -lt $DURATION ]]; do
    sleep 5
    ELAPSED=$((ELAPSED + 5))

    # Sender status
    if ! kill -0 "$SENDER_PID" 2>/dev/null; then
        warn "Sender exited early — check /tmp/strata-sender.log"
        break
    fi

    # Receiver stats
    STATS=$(ssh "$HOST" "tail -1 /tmp/strata-receiver.log 2>/dev/null | grep -oE 'next_seq=[^,;]+|lost_packets=[^,;]+|late_packets=[^,;]+|current_latency_ms=[^,;]+'" 2>/dev/null || echo "")

    # Segment count
    SEG_INFO=$(ssh "$HOST" "ls -1 $HLS_DIR/*.ts 2>/dev/null | wc -l" 2>/dev/null || echo "0")
    SEGMENT_COUNT=$SEG_INFO

    # Sender adaptation
    SENDER_STATE=$(grep 'Bitrate:' /tmp/strata-sender.log 2>/dev/null | tail -1 || echo "(no adaptation yet)")

    echo "[${ELAPSED}s] segments=$SEGMENT_COUNT | $STATS | $SENDER_STATE"
done

echo ""
echo "── Final state ──"
ssh "$HOST" "ls -lh $HLS_DIR/ 2>/dev/null" || warn "No HLS directory found"
