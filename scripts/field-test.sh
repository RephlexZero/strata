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
#   STRATA_REDUNDANCY_ENABLED   — Sender scheduler redundancy flag (default: false)
#   STRATA_CRITICAL_BROADCAST   — Broadcast critical stream headers (default: false)
#   STRATA_FAILOVER_ENABLED     — Sender scheduler failover flag (default: true)
#   STRATA_FAILOVER_DURATION_MS — Sender failover hold (default: 800)
#   STRATA_MAX_LATENCY_MS  — Receiver jitter buffer ceiling (default: 1000)
#   STRATA_DURATION_SECS   — How long to stream before stopping (default: 60)
#   STRATA_NO_BUILD=1      — Skip building and installing the sender binary
#   STRATA_NO_DEPLOY=1     — Skip cross-compiling and deploying receiver binary
#   STRATA_DEPLOY_IFACE    — Network interface for SSH/SCP deploy (e.g. "wlan0" to avoid cellular)
#   STRATA_LOG_LEVEL       — Rust log level (default: debug)
#   YOUTUBE_API_KEY        — API key for fetching stream health
#   YOUTUBE_STREAM_ID      — ID of the YouTube live stream to monitor
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
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ "$line" =~ ^[[:space:]]*# ]] && continue
        [[ "$line" =~ ^[[:space:]]*$ ]] && continue
        if [[ "$line" =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
            key="${BASH_REMATCH[1]}"
            value="${BASH_REMATCH[2]}"
            if [[ -z "${!key+x}" ]]; then
                if [[ "$value" =~ ^\"(.*)\"$ ]]; then
                    value="${BASH_REMATCH[1]}"
                elif [[ "$value" =~ ^\'(.*)\'$ ]]; then
                    value="${BASH_REMATCH[1]}"
                fi
                export "$key=$value"
            fi
        fi
    done < "$ENV_FILE"
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
REDUNDANCY_ENABLED="${STRATA_REDUNDANCY_ENABLED:-false}"
CRITICAL_BROADCAST="${STRATA_CRITICAL_BROADCAST:-false}"
FAILOVER_ENABLED="${STRATA_FAILOVER_ENABLED:-true}"
FAILOVER_DURATION_MS="${STRATA_FAILOVER_DURATION_MS:-800}"
MAX_LATENCY_MS="${STRATA_MAX_LATENCY_MS:-1000}"
DURATION="${STRATA_DURATION_SECS:-60}"
LOG_LEVEL="${STRATA_LOG_LEVEL:-debug,strata_bonding=debug,strata_transport=debug,strata::adapt=debug}"
HOST="${STRATA_RECEIVER_HOST}"

# SSH/SCP options — bind to a specific interface (e.g. WiFi) so deploys
# don't go through the cellular links you're about to bond.
SSH_OPTS=(-o ConnectTimeout=10)
DEPLOY_BIND_ADDR=""
if [[ -n "${STRATA_DEPLOY_IFACE:-}" ]]; then
    DEPLOY_BIND_ADDR="$(ip -o -4 addr show dev "${STRATA_DEPLOY_IFACE}" 2>/dev/null | awk '{print $4}' | head -n1 | cut -d/ -f1 || true)"
    SSH_OPTS+=(-o "BindInterface=${STRATA_DEPLOY_IFACE}")
    if [[ -n "$DEPLOY_BIND_ADDR" ]]; then
        SSH_OPTS+=(-o "BindAddress=${DEPLOY_BIND_ADDR}")
        info "Deploy will use interface ${STRATA_DEPLOY_IFACE} (source ${DEPLOY_BIND_ADDR}) for SSH/SCP"
    else
        warn "Could not resolve IPv4 for ${STRATA_DEPLOY_IFACE}; using BindInterface only"
        info "Deploy will use interface ${STRATA_DEPLOY_IFACE} for SSH/SCP"
    fi
fi

# Parse ports and interfaces
IFS=',' read -ra PORTS <<< "${STRATA_RECEIVER_PORTS}"
IFS=',' read -ra IFACES <<< "${STRATA_LINK_IFACES:-}"

NUM_LINKS=${#PORTS[@]}

if [[ ${#IFACES[@]} -gt 0 && ${#IFACES[@]} -ne "$NUM_LINKS" ]]; then
    fail "STRATA_LINK_IFACES has ${#IFACES[@]} entries but STRATA_RECEIVER_PORTS has $NUM_LINKS"
fi

# ── Build and install sender binary ─────────────────────────────────
echo "═══ Strata Field Test ═══"
echo ""

REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
if [[ "${STRATA_NO_BUILD:-0}" == "1" ]]; then
    warn "Skipping build/install (STRATA_NO_BUILD=1)"
else
    echo "── Building and installing strata-pipeline ──"
    make -C "$REPO_ROOT" install || fail "make install failed"
    info "strata-pipeline built and installed"
fi
echo ""

# ── Pre-flight checks ───────────────────────────────────────────────
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
        if command -v lsof >/dev/null 2>&1 && lsof "$VIDEO_DEVICE" >/dev/null 2>&1; then
            warn "Video device $VIDEO_DEVICE appears busy"
            lsof "$VIDEO_DEVICE" || true
            fail "Video device $VIDEO_DEVICE is already in use"
        fi
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
if ssh "${SSH_OPTS[@]}" "$HOST" "echo ok" >/dev/null 2>&1; then
    info "SSH to $HOST is reachable"
else
    fail "Cannot SSH to $HOST"
fi

# 8. Cross-compile and deploy receiver binary to remote
if [[ "${STRATA_NO_DEPLOY:-0}" == "1" ]]; then
    warn "Skipping receiver deploy (STRATA_NO_DEPLOY=1)"
else
    echo ""
    echo "── Deploying receiver to $HOST (aarch64 cross-compile) ──"
    REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
    STRATA_DEPLOY_HOST="$HOST" \
    STRATA_DEPLOY_IFACE="${STRATA_DEPLOY_IFACE:-}" \
    STRATA_DEPLOY_BIND_ADDR="${DEPLOY_BIND_ADDR:-}" \
    make -C "$REPO_ROOT" deploy-aarch64
    info "Receiver binary deployed"
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
            RECEIVER_IP=$(ssh "${SSH_OPTS[@]}" "$HOST" "hostname -I | awk '{print \$1}'" 2>/dev/null || echo "")
            [[ -z "$RECEIVER_IP" ]] && fail "Cannot resolve receiver IP — use full host:port in STRATA_RECEIVER_PORTS"
            echo "uri = \"${RECEIVER_IP}:${PORT}\""
        fi
        if [[ ${#IFACES[@]} -gt 0 ]]; then
            echo "interface = \"${IFACES[$i]}\""
        fi
        echo ""
    done

    echo "[scheduler]"
    echo "redundancy_enabled = $REDUNDANCY_ENABLED"
    echo "critical_broadcast = $CRITICAL_BROADCAST"
    echo "failover_enabled = $FAILOVER_ENABLED"
    echo "failover_duration_ms = $FAILOVER_DURATION_MS"
} > "$SENDER_TOML"

# Receiver TOML
{
    echo "[receiver]"
    echo "buffer_capacity = 4096"
    echo ""
    echo "[scheduler]"
    # Only the hard ceiling is user-set (from STRATA_MAX_LATENCY_MS).  The
    # buffer self-tunes within this ceiling via closed-loop late-arrival
    # feedback — no jitter/start-latency knobs to misconfigure.
    echo "max_latency_ms = $MAX_LATENCY_MS"
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
ssh "${SSH_OPTS[@]}" "$HOST" "pkill -INT strata-pipeline 2>/dev/null || true; sleep 2; pkill -TERM strata-pipeline 2>/dev/null || true; echo ok" 2>/dev/null || true

# Copy receiver config
scp "${SSH_OPTS[@]}" -q "$RECEIVER_TOML" "$HOST:/tmp/strata-receiver.toml"

# Write receiver start script (avoids SSH quoting issues — snag #11)
RECEIVER_SCRIPT=$(mktemp /tmp/strata-receiver-start-XXXXXX.sh)
cat > "$RECEIVER_SCRIPT" << ENDSCRIPT
#!/bin/bash
export GST_PLUGIN_PATH=\$HOME/.local/share/gstreamer-1.0/plugins
nohup env RUST_LOG="$LOG_LEVEL" GST_DEBUG="tsdemux:4,strata*:4" /usr/local/bin/strata-pipeline receiver \\
  --bind "$BIND_STR" \\
  --relay-url "$STRATA_RELAY_URL" \\
  --codec "$CODEC" \\
  --config /tmp/strata-receiver.toml \\
  > /tmp/strata-receiver.log 2>&1 < /dev/null &
echo "PID: \$!"
disown
ENDSCRIPT

scp "${SSH_OPTS[@]}" -q "$RECEIVER_SCRIPT" "$HOST:/tmp/start-receiver.sh"
ssh "${SSH_OPTS[@]}" "$HOST" "chmod +x /tmp/start-receiver.sh && bash /tmp/start-receiver.sh"
sleep 2

RECEIVER_PID=$(ssh "${SSH_OPTS[@]}" "$HOST" "pgrep -n strata-pipeline" 2>/dev/null || echo "")
if [[ -z "$RECEIVER_PID" ]]; then
    fail "Receiver failed to start — check $HOST:/tmp/strata-receiver.log"
fi
info "Receiver started (PID $RECEIVER_PID)"

# Discover the actual HLS temp directory from receiver logs. This avoids
# assuming /dev/shm in environments where the receiver falls back to /tmp.
HLS_DIR=$(ssh "${SSH_OPTS[@]}" "$HOST" "grep -m1 'HLS temp dir:' /tmp/strata-receiver.log 2>/dev/null | sed -E 's/^.*HLS temp dir: ([^ ]+).*$/\\1/'" 2>/dev/null || echo "")
if [[ -z "$HLS_DIR" ]]; then
    HLS_DIR="/dev/shm/strata-hls-rx-${RECEIVER_PID}"
fi
info "Receiver HLS dir: $HLS_DIR"

# ── Build dest string for sender ────────────────────────────────────
DEST_STR=""
for port in "${PORTS[@]}"; do
    if [[ "$port" == *:* ]]; then
        DEST_STR="${DEST_STR:+$DEST_STR,}$port"
    else
        RECEIVER_IP=$(ssh "${SSH_OPTS[@]}" "$HOST" "hostname -I | awk '{print \$1}'" 2>/dev/null)
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

# Enable adaptation debug logging unless user already set RUST_LOG
export RUST_LOG="$LOG_LEVEL"

strata-pipeline "${SENDER_ARGS[@]}" > /tmp/strata-sender.log 2>&1 &
SENDER_PID=$!
info "Sender started (PID $SENDER_PID)"

# ── Monitor ─────────────────────────────────────────────────────────
echo ""
echo "── Streaming for ${DURATION}s — monitoring every 5s ──"

ELAPSED=0
SEGMENT_COUNT=0
MAX_SEGMENT_COUNT=0
PLAYLIST_SEEN=0
WORST_FB_LOSS_FEC="0.000"
MAX_WINDOW_LOSS_BP=0
MAX_DELTA_LATE=0
UNHEALTHY_WINDOWS=0
CLEANUP_DONE=0

cleanup() {
    if [[ $CLEANUP_DONE -eq 1 ]]; then
        return
    fi
    CLEANUP_DONE=1

    echo ""
    echo "── Shutting down ──"
    kill "$SENDER_PID" 2>/dev/null || true
    wait "$SENDER_PID" 2>/dev/null || true
    ssh "${SSH_OPTS[@]}" "$HOST" "pkill -INT strata-pipeline 2>/dev/null || true; sleep 2; pkill -TERM strata-pipeline 2>/dev/null || true; echo ok" 2>/dev/null || true

    # Final snapshot before verdict: if the sender exited before the first
    # 5s monitor tick, MAX_SEGMENT_COUNT may still be 0 despite valid output.
    FINAL_SEGMENT_COUNT=$(ssh "${SSH_OPTS[@]}" "$HOST" "find '$HLS_DIR' -maxdepth 1 -type f -name '*.ts' 2>/dev/null | wc -l" 2>/dev/null || echo "0")
    if [[ $FINAL_SEGMENT_COUNT -gt $MAX_SEGMENT_COUNT ]]; then
        MAX_SEGMENT_COUNT=$FINAL_SEGMENT_COUNT
    fi
    FINAL_PLAYLIST_COUNT=$(ssh "${SSH_OPTS[@]}" "$HOST" "find '$HLS_DIR' -maxdepth 1 -type f -name '*.m3u8' 2>/dev/null | wc -l" 2>/dev/null || echo "0")
    if [[ $FINAL_PLAYLIST_COUNT -gt 0 ]]; then
        PLAYLIST_SEEN=1
    fi

    echo "── Fetching full logs for analysis (via ${STRATA_DEPLOY_IFACE:-SSH}) ──"
    # This safely traverses wlan0 (or whatever STRATA_DEPLOY_IFACE is set to) so it doesn't affect active tests
    scp "${SSH_OPTS[@]}" -q "$HOST:/tmp/strata-receiver.log" "./strata-receiver-${SENDER_PID}.log" || warn "Failed to fetch receiver log"
    cp /tmp/strata-sender.log "./strata-sender-${SENDER_PID}.log" || warn "Failed to copy sender log"
    info "Saved full logs to ./strata-sender-${SENDER_PID}.log and ./strata-receiver-${SENDER_PID}.log"

    rm -f "$SENDER_TOML" "$RECEIVER_TOML" "$RECEIVER_SCRIPT" "/tmp/start-receiver.sh"
    echo ""

    MAX_WINDOW_LOSS_PCT=$(awk "BEGIN { printf \"%.1f\", $MAX_WINDOW_LOSS_BP / 100.0 }")
    HEALTH_SUMMARY="worst_loss_fec=${WORST_FB_LOSS_FEC} max_window_loss=${MAX_WINDOW_LOSS_PCT}% max_delta_late=${MAX_DELTA_LATE} unhealthy_windows=${UNHEALTHY_WINDOWS}"

    severe_health_failure=0
    degraded_health=0
    if awk "BEGIN { exit !($WORST_FB_LOSS_FEC >= 0.55) }"; then
        severe_health_failure=1
    fi
    if [[ $MAX_WINDOW_LOSS_BP -ge 2000 || $MAX_DELTA_LATE -ge 250 || $UNHEALTHY_WINDOWS -ge 3 ]]; then
        severe_health_failure=1
    fi
    if awk "BEGIN { exit !($WORST_FB_LOSS_FEC >= 0.30) }"; then
        degraded_health=1
    fi
    if [[ $MAX_WINDOW_LOSS_BP -ge 1000 || $MAX_DELTA_LATE -ge 120 || $UNHEALTHY_WINDOWS -ge 1 ]]; then
        degraded_health=1
    fi

    if [[ $MAX_SEGMENT_COUNT -gt 2 ]]; then
        if [[ $severe_health_failure -eq 1 ]]; then
            fail "FAILED: Segments produced but stream health collapsed ($HEALTH_SUMMARY)"
        elif [[ $degraded_health -eq 1 ]]; then
            warn "PARTIAL: Segments produced but quality degraded ($HEALTH_SUMMARY)"
        else
            info "SUCCESS: $MAX_SEGMENT_COUNT HLS segments produced and uploaded ($HEALTH_SUMMARY)"
        fi
    elif [[ $MAX_SEGMENT_COUNT -gt 0 ]]; then
        warn "PARTIAL: Only $MAX_SEGMENT_COUNT segment(s) produced ($HEALTH_SUMMARY)"
    elif [[ $PLAYLIST_SEEN -gt 0 ]]; then
        warn "PARTIAL: Playlist observed but no retained TS segments found ($HEALTH_SUMMARY)"
    else
        fail "FAILED: No HLS segments produced"
    fi
}
trap cleanup EXIT INT TERM

PREV_LOST=0
PREV_LATE=0
PREV_DELIVERED=0

# Coerce a value to a non-negative integer (defaults to 0). Prevents
# arithmetic / comparison failures under `set -e` when ssh/grep return
# empty or non-numeric payloads during transient network blips.
num() {
    local v="${1:-0}"
    [[ "$v" =~ ^[0-9]+$ ]] || v=0
    echo "$v"
}

# The monitor loop intentionally relaxes errexit: a single ssh timeout or
# grep-with-no-match must not abort the run before we reach the final
# state dump. Re-enable it below the loop so cleanup still fails loudly.
set +e

while [[ $ELAPSED -lt $DURATION ]]; do
    sleep 5
    ELAPSED=$((ELAPSED + 5))

    # Sender status
    if ! kill -0 "$SENDER_PID" 2>/dev/null; then
        warn "Sender exited early — check /tmp/strata-sender.log"
        break
    fi

    # Receiver stats (last lines — include enough history for per-link fields)
    RX_RAW=$(ssh "${SSH_OPTS[@]}" "$HOST" "tail -30 /tmp/strata-receiver.log 2>/dev/null" 2>/dev/null || echo "")
    RX_STATS_LINE=$(echo "$RX_RAW" | grep 'strata-stats' | tail -1 || echo "")
    STATS=$(echo "$RX_STATS_LINE" | grep -oE 'next_seq=[^,;]+|lost_packets=[^,;]+|late_packets=[^,;]+|current_latency_ms=[^,;]+|target_latency_ms=[^,;]+|jitter_estimate_ms=[^,;]+|loss_rate=[^,;]+|packets_delivered=[^,;]+|queue_depth=[^,;]+' | head -9 | tr '\n' ' ')
    RX_LINK_STATS=$(echo "$RX_STATS_LINE" | grep -oE 'packets_received_link_[0-9]+=[^,;]+|packets_delivered_link_[0-9]+=[^,;]+|loss_link_[0-9]+=[^,;]+' | tr '\n' ' ')

    # Extract numbers for delta calculation (handle GStreamer type annotations like =(guint64)123)
    CUR_LOST=$(num "$(echo "$STATS" | grep -oP 'lost_packets=\([^)]*\)\K[0-9]+' | head -1)")
    CUR_LATE=$(num "$(echo "$STATS" | grep -oP 'late_packets=\([^)]*\)\K[0-9]+' | head -1)")
    CUR_DELIVERED=$(num "$(echo "$STATS" | grep -oP 'packets_delivered=\([^)]*\)\K[0-9]+' | head -1)")
    # A counter reset (receiver restart) would produce a negative delta.
    # Clamp to 0 so the health math remains sane.
    DELTA_LOST=$(( CUR_LOST >= PREV_LOST ? CUR_LOST - PREV_LOST : 0 ))
    DELTA_LATE=$(( CUR_LATE >= PREV_LATE ? CUR_LATE - PREV_LATE : 0 ))
    DELTA_DELIVERED=$(( CUR_DELIVERED >= PREV_DELIVERED ? CUR_DELIVERED - PREV_DELIVERED : 0 ))
    PREV_LOST=$CUR_LOST; PREV_LATE=$CUR_LATE; PREV_DELIVERED=$CUR_DELIVERED

    WINDOW_TOTAL=$((DELTA_DELIVERED + DELTA_LOST))
    WINDOW_LOSS_BP=0
    if [[ $WINDOW_TOTAL -gt 0 ]]; then
        WINDOW_LOSS_BP=$((DELTA_LOST * 10000 / WINDOW_TOTAL))
    fi
    if [[ $WINDOW_LOSS_BP -gt $MAX_WINDOW_LOSS_BP ]]; then
        MAX_WINDOW_LOSS_BP=$WINDOW_LOSS_BP
    fi
    if [[ $DELTA_LATE -gt $MAX_DELTA_LATE ]]; then
        MAX_DELTA_LATE=$DELTA_LATE
    fi
    if [[ $WINDOW_LOSS_BP -ge 1200 || $DELTA_LATE -ge 150 ]]; then
        UNHEALTHY_WINDOWS=$((UNHEALTHY_WINDOWS + 1))
    fi

    # Segment count
    SEG_INFO=$(ssh "${SSH_OPTS[@]}" "$HOST" "find '$HLS_DIR' -maxdepth 1 -type f -name '*.ts' 2>/dev/null | wc -l" 2>/dev/null || echo "0")
    SEGMENT_COUNT=$(num "$SEG_INFO")
    if [[ $SEGMENT_COUNT -gt $MAX_SEGMENT_COUNT ]]; then
        MAX_SEGMENT_COUNT=$SEGMENT_COUNT
    fi
    PLAYLIST_COUNT=$(num "$(ssh "${SSH_OPTS[@]}" "$HOST" "find '$HLS_DIR' -maxdepth 1 -type f -name '*.m3u8' 2>/dev/null | wc -l" 2>/dev/null || echo "0")")
    if [[ $PLAYLIST_COUNT -gt 0 ]]; then
        PLAYLIST_SEEN=1
    fi

    # Sender: last adaptation + feedback lines
    ADAPT_LINE=$(grep '\[adapt\] agg=' /tmp/strata-sender.log 2>/dev/null | tail -1 | sed 's/.*\[adapt\]/[adapt]/' || echo "")
    FB_LINE=$(grep '\[adapt\] fb:' /tmp/strata-sender.log 2>/dev/null | tail -1 | sed 's/.*\[adapt\]/[adapt]/' || echo "")
    CMD_LINE=$(grep '\[adapt\] CMD' /tmp/strata-sender.log 2>/dev/null | tail -1 | sed 's/.*\[adapt\]/[adapt]/' || echo "")
    FEC_LINE=$(grep '\[fec\]' /tmp/strata-sender.log 2>/dev/null | tail -1 | sed 's/.*\[fec\]/[fec]/' || echo "")
    LINK_LINES=$(grep '\[link\]' /tmp/strata-sender.log 2>/dev/null | tail -"$NUM_LINKS" | sed 's/.*\[link\]/  [link]/' || echo "")

    CUR_FB_LOSS=$(echo "$FB_LINE" | grep -oP 'loss_fec=\K[0-9]+(\.[0-9]+)?' | head -1 || true)
    if [[ -n "$CUR_FB_LOSS" ]]; then
        if awk "BEGIN { exit !($CUR_FB_LOSS > $WORST_FB_LOSS_FEC) }"; then
            WORST_FB_LOSS_FEC="$CUR_FB_LOSS"
        fi
        if awk "BEGIN { exit !($CUR_FB_LOSS >= 0.30) }"; then
            UNHEALTHY_WINDOWS=$((UNHEALTHY_WINDOWS + 1))
        fi
    fi

    WINDOW_LOSS_PCT=$(awk "BEGIN { printf \"%.1f\", $WINDOW_LOSS_BP / 100.0 }")

    # YouTube Health Polling
    YT_HEALTH_STR=""
    if [[ -n "${YOUTUBE_API_KEY:-}" && -n "${YOUTUBE_STREAM_ID:-}" ]]; then
        # Run a quick curl with a timeout so it doesn't stall the monitor loop
        YT_STATUS=$(curl -s --max-time 2 "https://www.googleapis.com/youtube/v3/liveStreams?part=status&id=${YOUTUBE_STREAM_ID}&key=${YOUTUBE_API_KEY}" | grep -oP '"healthStatus":\s*\{\s*"status":\s*"\K[^"]+' | head -1 || echo "unknown")
        [[ -n "$YT_STATUS" ]] && YT_HEALTH_STR=" YT_Health=$YT_STATUS"
    fi

    echo ""
    echo "╌╌╌ [${ELAPSED}s] segments=$SEGMENT_COUNT (max=$MAX_SEGMENT_COUNT)$YT_HEALTH_STR ╌╌╌"
    echo "  RX: $STATS"
    [[ -n "$RX_LINK_STATS" ]] && echo "  RX links: $RX_LINK_STATS"
    echo "  Δ5s: delivered=$DELTA_DELIVERED lost=$DELTA_LOST late=$DELTA_LATE win_loss=${WINDOW_LOSS_PCT}%"
    [[ -n "$ADAPT_LINE" ]] && echo "  $ADAPT_LINE"
    [[ -n "$FB_LINE" ]]    && echo "  $FB_LINE"
    [[ -n "$CMD_LINE" ]]   && echo "  $CMD_LINE"
    [[ -n "$FEC_LINE" ]]   && echo "  $FEC_LINE"
    [[ -n "$LINK_LINES" ]] && echo "$LINK_LINES"
done

set -e

echo ""
echo "── Final state ──"
echo "Receiver log (last 20 lines):"
ssh "${SSH_OPTS[@]}" "$HOST" "tail -20 /tmp/strata-receiver.log 2>/dev/null" || warn "No receiver log"
echo ""
echo "Sender log (last 20 lines):"
tail -20 /tmp/strata-sender.log 2>/dev/null || warn "No sender log"
echo ""
echo "HLS directory:"
ssh "${SSH_OPTS[@]}" "$HOST" "ls -lh $HLS_DIR/ 2>/dev/null" || warn "No HLS directory found"
