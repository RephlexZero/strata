#!/usr/bin/env bash
# scripts/orangepi_ethernet_field_test.sh — Orange Pi 5 sender → cloud receiver → YouTube
#
# Topology (differs from field-test.sh, which runs the sender on this machine):
#
#     ┌─────────────────────────┐         bonded cellular          ┌────────────────────┐
#     │  Orange Pi 5 (RK3588)    │   modems on the Orange Pi        │  Cloud receiver    │
#     │  • USB camera (v4l2)     │ ───────────────────────────────▶│  • reassemble      │
#     │  • rkmpp HW H.265 encode │   (links bound to its modem ifs) │  • HLS → YouTube   │
#     │  • strata sender         │                                  │                    │
#     └─────────────────────────┘                                  └────────────────────┘
#            ▲  SSH/deploy over ethernet (management)  ▲
#            └──────── this dev machine orchestrates ──┘
#
# Both the Orange Pi and the cloud receiver are aarch64, so we cross-compile the
# binary + plugin ONCE and deploy to both. The Orange Pi auto-selects its
# Rockchip MPP hardware encoder (rkmpph265enc / mpph265enc) — see codec.rs.
#
# Required env vars (put in .env at the repo root):
#   STRATA_SENDER_HOST     — SSH alias/user@host of the Orange Pi (over ethernet)
#   STRATA_SENDER_IFACES   — comma-separated modem interfaces ON THE ORANGE PI,
#                            one per link (e.g. "eth1,usb0" or "enxAA..,enxBB..")
#   STRATA_RECEIVER_HOST   — SSH alias/user@host of the cloud receiver
#   STRATA_RECEIVER_PORTS  — comma-separated host:port or port (e.g. "1.2.3.4:5000,1.2.3.4:5002")
#   STRATA_RELAY_URL       — YouTube HLS upload URL (or RTMP URL)
#
# Optional env vars:
#   STRATA_SENDER_VIDEO_DEVICE — camera on the Orange Pi (default: /dev/video0)
#   STRATA_RESOLUTION      — WxH (default: 1920x1080)
#   STRATA_FRAMERATE       — FPS (default: 30)
#   STRATA_CODEC           — h264 | h265 (default: h265)
#   STRATA_PROFILE         — broadcast | low-latency | realtime (default: broadcast)
#   STRATA_BITRATE/_MIN/_MAX — target/min/max kbps (default: 4000 / 800 / 8000)
#   STRATA_STARTUP_RAMP_MS — gently ramp the encoder from a low floor up to
#                            STRATA_BITRATE over this window so a cold link
#                            isn't blasted at startup (default: 4000; 0=off)
#   STRATA_STARTUP_FLOOR_KBPS — bitrate the startup ramp begins at
#                            (clamped to >= STRATA_MIN_BITRATE; 0=adapter default)
#   STRATA_AUDIO_ENABLED   — 1/0 (default: 1; silent AAC track for YouTube)
#   STRATA_DURATION_SECS   — stream duration before stopping (default: 120)
#   STRATA_MONITOR_INTERVAL_S — monitor cadence (default: 5)
#   STRATA_LOCAL_HLS_PORT  — also serve the receiver's HLS dir at
#                            http://localhost:<port>/playlist.m3u8 via an SSH
#                            tunnel, for watching in VLC/mpv without YouTube
#                            (default: 8088; set 0 to disable)
#   STRATA_EGRESS_WATCHDOG_SEC — receiver self-heal: rebuild its pipeline after
#                            this many seconds without a new HLS segment
#                            (default: 15; 0 disables — e.g. for a GST_DEBUG
#                            run that should observe a wedge, not heal it)
#   STRATA_MAX_LATENCY_MS  — receiver playout ceiling override (default: profile)
#   STRATA_RECEIVER_BUFFER_CAPACITY — receiver reorder slots (default: 4096)
#   STRATA_NO_BUILD=1      — skip the aarch64 cross-compile
#   STRATA_NO_DEPLOY=1     — skip deploying to both hosts (use already-installed)
#   STRATA_SENDER_DEPLOY_IFACE / STRATA_DEPLOY_IFACE — local iface for SSH (optional)
#   STRATA_LOG_LEVEL       — Rust log level (default: info)
#   YOUTUBE_API_KEY / YOUTUBE_STREAM_ID — optional live-health polling
#
# Usage:
#   export STRATA_SENDER_HOST=orangepi          # or root@192.168.1.50
#   export STRATA_SENDER_IFACES="eth1,usb0"
#   ./scripts/orangepi_ethernet_field_test.sh

set -euo pipefail

# ── Load .env from project root if present ───────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$REPO_ROOT/.env"
if [[ -f "$ENV_FILE" ]]; then
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ "$line" =~ ^[[:space:]]*# ]] && continue
        [[ "$line" =~ ^[[:space:]]*$ ]] && continue
        if [[ "$line" =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
            key="${BASH_REMATCH[1]}"; value="${BASH_REMATCH[2]}"
            if [[ -z "${!key+x}" ]]; then
                if   [[ "$value" =~ ^\"(.*)\"$ ]]; then value="${BASH_REMATCH[1]}"
                elif [[ "$value" =~ ^\'(.*)\'$ ]]; then value="${BASH_REMATCH[1]}"; fi
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
[[ -z "${STRATA_SENDER_HOST:-}"    ]] && fail "STRATA_SENDER_HOST is not set (Orange Pi SSH host)"
[[ -z "${STRATA_SENDER_IFACES:-}"  ]] && fail "STRATA_SENDER_IFACES is not set (modem interfaces on the Orange Pi)"
[[ -z "${STRATA_RECEIVER_HOST:-}"  ]] && fail "STRATA_RECEIVER_HOST is not set"
[[ -z "${STRATA_RECEIVER_PORTS:-}" ]] && fail "STRATA_RECEIVER_PORTS is not set"
[[ -z "${STRATA_RELAY_URL:-}"      ]] && fail "STRATA_RELAY_URL is not set"

SENDER_HOST="$STRATA_SENDER_HOST"
RECEIVER_HOST="$STRATA_RECEIVER_HOST"

# ── Defaults ─────────────────────────────────────────────────────────
VIDEO_DEVICE="${STRATA_SENDER_VIDEO_DEVICE:-/dev/video0}"
RESOLUTION="${STRATA_RESOLUTION:-1920x1080}"
FRAMERATE="${STRATA_FRAMERATE:-30}"
CODEC="${STRATA_CODEC:-h265}"
PROFILE="${STRATA_PROFILE:-broadcast}"
BITRATE="${STRATA_BITRATE:-4000}"
MIN_BITRATE="${STRATA_MIN_BITRATE:-800}"
MAX_BITRATE="${STRATA_MAX_BITRATE:-8000}"
# Gentle startup ramp (ms): encoder climbs from a low floor to BITRATE over
# this window so the cold link isn't blasted with full rate at stream start
# (the dominant source of the ~14% startup loss burst that decodes as grey).
STARTUP_RAMP_MS="${STRATA_STARTUP_RAMP_MS:-4000}"
# Bitrate the ramp begins at (clamped to >= MIN_BITRATE inside the adapter).
# 0 = use the adapter default (500).
STARTUP_FLOOR_KBPS="${STRATA_STARTUP_FLOOR_KBPS:-0}"
AUDIO_ENABLED="${STRATA_AUDIO_ENABLED:-1}"
DURATION="${STRATA_DURATION_SECS:-120}"
MONITOR_INTERVAL="${STRATA_MONITOR_INTERVAL_S:-5}"
LOCAL_HLS_PORT="${STRATA_LOCAL_HLS_PORT:-8088}"
LOG_LEVEL="${STRATA_LOG_LEVEL:-info}"
RECEIVER_BUFFER_CAPACITY="${STRATA_RECEIVER_BUFFER_CAPACITY:-4096}"
MAX_LATENCY_MS="${STRATA_MAX_LATENCY_MS:-}"
# Pin the receiver playout window (broadcast profile uses fixed_playout, so this
# fixes the buffer depth). Raising it lengthens the aggregator's gap-skip wait
# (skip_after == latency), giving late/reordered packets more time to arrive
# before a hole is declared — important on a single link, which has no
# cross-link delay-spread to push the adaptive window up on its own.
START_LATENCY_MS="${STRATA_START_LATENCY_MS:-}"

# ── Parse links ──────────────────────────────────────────────────────
IFS=',' read -ra PORTS  <<< "${STRATA_RECEIVER_PORTS}"
IFS=',' read -ra IFACES <<< "${STRATA_SENDER_IFACES}"
NUM_LINKS=${#PORTS[@]}
[[ ${#IFACES[@]} -ne "$NUM_LINKS" ]] && \
    fail "STRATA_SENDER_IFACES has ${#IFACES[@]} entries but STRATA_RECEIVER_PORTS has $NUM_LINKS"

# ── SSH helpers (separate option arrays per host so a deploy iface can
#    pin the management path without touching the bonded modems) ───────
ssh_opts() {  # $1 = optional local deploy iface
    # ForwardAgent/ForwardX11 off: this script never needs either, but a
    # user's local ~/.ssh/config default of "yes" makes every session print
    # "channel N: open failed: connect failed: Connection refused" (the
    # forwarded channel can't complete) — harmless noise, silenced at the
    # source instead of relying on every operator's dotfiles.
    local arr=(-o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new
               -o ForwardAgent=no -o ForwardX11=no)
    if [[ -n "${1:-}" ]]; then
        local addr
        addr="$(ip -o -4 addr show dev "$1" 2>/dev/null | awk '{print $4}' | head -n1 | cut -d/ -f1 || true)"
        arr+=(-o "BindInterface=$1")
        [[ -n "$addr" ]] && arr+=(-o "BindAddress=$addr")
    fi
    printf '%s\n' "${arr[@]}"
}
mapfile -t SENDER_SSH   < <(ssh_opts "${STRATA_SENDER_DEPLOY_IFACE:-${STRATA_DEPLOY_IFACE:-}}")
mapfile -t RECEIVER_SSH < <(ssh_opts "${STRATA_DEPLOY_IFACE:-}")

echo "═══ Strata Orange Pi Field Test ═══"
echo "  sender   : $SENDER_HOST  (ifaces: ${STRATA_SENDER_IFACES}, camera: $VIDEO_DEVICE)"
echo "  receiver : $RECEIVER_HOST  (ports: ${STRATA_RECEIVER_PORTS})"
echo "  codec=$CODEC profile=$PROFILE ${RESOLUTION}@${FRAMERATE} ${BITRATE}kbps"
echo ""

# ── SSH reachability ─────────────────────────────────────────────────
ssh "${SENDER_SSH[@]}"   "$SENDER_HOST"   "echo ok" >/dev/null 2>&1 || fail "Cannot SSH to Orange Pi ($SENDER_HOST)"
info "SSH to Orange Pi ($SENDER_HOST) OK"
ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "echo ok" >/dev/null 2>&1 || fail "Cannot SSH to receiver ($RECEIVER_HOST)"
info "SSH to receiver ($RECEIVER_HOST) OK"

# ── Camera present on the Orange Pi ──────────────────────────────────
if ssh "${SENDER_SSH[@]}" "$SENDER_HOST" "test -e '$VIDEO_DEVICE'" 2>/dev/null; then
    info "Camera $VIDEO_DEVICE present on Orange Pi"
else
    fail "Camera $VIDEO_DEVICE not found on Orange Pi ($SENDER_HOST)"
fi

# ── Modem interfaces present on the Orange Pi ────────────────────────
for ifc in "${IFACES[@]}"; do
    if ssh "${SENDER_SSH[@]}" "$SENDER_HOST" "ip link show '$ifc'" >/dev/null 2>&1; then
        info "Orange Pi interface $ifc exists"
    else
        fail "Orange Pi interface '$ifc' not found — fix STRATA_SENDER_IFACES"
    fi
done

# ── Build aarch64 once, deploy to BOTH hosts ─────────────────────────
ART="target/aarch64-unknown-linux-gnu/release"
if [[ "${STRATA_NO_BUILD:-0}" == "1" ]]; then
    warn "Skipping aarch64 build (STRATA_NO_BUILD=1)"
else
    echo "── Cross-compiling aarch64 binary + plugin ──"
    make -C "$REPO_ROOT" cross-aarch64 || fail "cross-aarch64 build failed"
fi
[[ -f "$REPO_ROOT/$ART/strata-pipeline" ]] || fail "aarch64 binary missing — run without STRATA_NO_BUILD"

# deploy_to <ssh-opts-arrayname> <host>
deploy_to() {
    local -n _ssh="$1"; local host="$2"
    echo "── Deploying to $host ──"
    rsync -z --progress -e "ssh ${_ssh[*]}" "$REPO_ROOT/$ART/strata-pipeline" "$host:/tmp/strata-pipeline-new"
    ssh "${_ssh[@]}" "$host" "mkdir -p ~/.local/share/gstreamer-1.0/plugins"
    rsync -z --progress -e "ssh ${_ssh[*]}" "$REPO_ROOT/$ART/libgststrata.so" "$host:~/.local/share/gstreamer-1.0/plugins/libgststrata.so"
    # Install with cap_net_raw (needed for SO_BINDTODEVICE on the sender's modems).
    ssh "${_ssh[@]}" "$host" \
        "pkill strata-pipeline 2>/dev/null; sleep 1; \
         sudo mv /tmp/strata-pipeline-new /usr/local/bin/strata-pipeline && \
         sudo chmod 755 /usr/local/bin/strata-pipeline && \
         sudo setcap cap_net_raw+ep /usr/local/bin/strata-pipeline" \
        || fail "Install on $host failed (need passwordless sudo there)"
    info "Deployed to $host"
}
if [[ "${STRATA_NO_DEPLOY:-0}" == "1" ]]; then
    warn "Skipping deploy (STRATA_NO_DEPLOY=1)"
else
    deploy_to RECEIVER_SSH "$RECEIVER_HOST"
    deploy_to SENDER_SSH   "$SENDER_HOST"
fi
echo ""

# ── Generate TOML configs ────────────────────────────────────────────
SENDER_TOML=$(mktemp /tmp/opi-sender-XXXXXX.toml)
RECEIVER_TOML=$(mktemp /tmp/opi-receiver-XXXXXX.toml)

# Resolve receiver IP (for sender --dest when ports are bare numbers).
RECEIVER_IP=""
if [[ "${PORTS[0]}" != *:* ]]; then
    RECEIVER_IP=$(ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "hostname -I | awk '{print \$1}'" 2>/dev/null)
    [[ -z "$RECEIVER_IP" ]] && fail "Could not resolve receiver IP; use host:port in STRATA_RECEIVER_PORTS"
fi

# Sender config: links bound to the Orange Pi's modem interfaces.
{
    echo "profile = \"$PROFILE\""
    echo ""
    for ((i=0; i<NUM_LINKS; i++)); do
        port="${PORTS[$i]}"
        uri="$port"; [[ "$port" != *:* ]] && uri="${RECEIVER_IP}:${port}"
        echo "[[links]]"
        echo "id = $i"
        echo "uri = \"$uri\""
        echo "interface = \"${IFACES[$i]}\""
        echo ""
    done
    # Optional [scheduler] redundancy/broadcast — for masking a bursty link by
    # duplicating critical (keyframe) packets across links. Off unless set, so
    # default runs are unchanged. critical_broadcast sends every keyframe to
    # ALL alive links (directly protects reference frames against per-link
    # burst loss); redundancy_enabled duplicates other important packets when
    # spare capacity allows.
    if [[ -n "${STRATA_REDUNDANCY_ENABLED:-}" || -n "${STRATA_CRITICAL_BROADCAST:-}" ]]; then
        echo "[scheduler]"
        [[ -n "${STRATA_REDUNDANCY_ENABLED:-}" ]] && echo "redundancy_enabled = ${STRATA_REDUNDANCY_ENABLED}"
        [[ -n "${STRATA_CRITICAL_BROADCAST:-}" ]] && echo "critical_broadcast = ${STRATA_CRITICAL_BROADCAST}"
        echo ""
    fi
} > "$SENDER_TOML"

# Receiver config: profile drives playout; ceiling only if explicitly set.
{
    echo "profile = \"$PROFILE\""
    echo ""
    echo "[receiver]"
    echo "buffer_capacity = $RECEIVER_BUFFER_CAPACITY"
    [[ -n "$START_LATENCY_MS" ]] && echo "start_latency_ms = $START_LATENCY_MS"
    if [[ -n "$MAX_LATENCY_MS" ]]; then
        echo ""
        echo "[scheduler]"
        echo "max_latency_ms = $MAX_LATENCY_MS"
    fi
} > "$RECEIVER_TOML"

info "Sender config:"; sed 's/^/    /' "$SENDER_TOML"

# ── Build bind (receiver) + dest (sender) strings ────────────────────
BIND_STR=""; DEST_STR=""
for port in "${PORTS[@]}"; do
    pnum="${port##*:}"
    BIND_STR="${BIND_STR:+$BIND_STR,}0.0.0.0:$pnum"
    if [[ "$port" == *:* ]]; then DEST_STR="${DEST_STR:+$DEST_STR,}$port"
    else DEST_STR="${DEST_STR:+$DEST_STR,}${RECEIVER_IP}:${port}"; fi
done

# ── Start receiver on the cloud host ─────────────────────────────────
# Optional GStreamer debug on the receiver (e.g. STRATA_GST_DEBUG="tsdemux:5,hlssink:5,mpegtsmux:5,h265parse:4")
# — output lands in /tmp/strata-receiver.log; keep runs short, it is verbose.
GST_DEBUG_ENV=""
[[ -n "${STRATA_GST_DEBUG:-}" ]] && GST_DEBUG_ENV="GST_DEBUG=${STRATA_GST_DEBUG} "
WATCHDOG_ENV=""
[[ -n "${STRATA_EGRESS_WATCHDOG_SEC:-}" ]] && WATCHDOG_ENV="STRATA_EGRESS_WATCHDOG_SEC=${STRATA_EGRESS_WATCHDOG_SEC} "
echo ""
echo "── Starting receiver on $RECEIVER_HOST ──"
ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "pkill -INT strata-pipeline 2>/dev/null || true; sleep 2; pkill -KILL strata-pipeline 2>/dev/null || true; echo ok" >/dev/null 2>&1 || true
scp "${RECEIVER_SSH[@]}" -q "$RECEIVER_TOML" "$RECEIVER_HOST:/tmp/strata-receiver.toml"
RX_START=$(mktemp /tmp/opi-rx-start-XXXXXX.sh)
cat > "$RX_START" <<ENDSCRIPT
#!/bin/bash
export GST_PLUGIN_PATH=\$HOME/.local/share/gstreamer-1.0/plugins
# setsid (not nohup+disown): on the Orange Pi, a nohup'd background job
# still dies when the launching SSH session closes; setsid detaches it into
# its own session so it survives. (Verified: nohup → sender gone in <5s;
# setsid → alive for the full run.)
setsid env RUST_LOG="$LOG_LEVEL" ${GST_DEBUG_ENV}${WATCHDOG_ENV}/usr/local/bin/strata-pipeline receiver \\
  --bind "$BIND_STR" \\
  --relay-url "$STRATA_RELAY_URL" \\
  --codec "$CODEC" \\
  --config /tmp/strata-receiver.toml \\
  > /tmp/strata-receiver.log 2>&1 < /dev/null &
echo "PID: \$!"
ENDSCRIPT
scp "${RECEIVER_SSH[@]}" -q "$RX_START" "$RECEIVER_HOST:/tmp/start-receiver.sh"
ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "chmod +x /tmp/start-receiver.sh && bash /tmp/start-receiver.sh" >/dev/null
sleep 2
RECEIVER_PID=$(ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "pgrep -n strata-pipeline" 2>/dev/null || echo "")
[[ -z "$RECEIVER_PID" ]] && fail "Receiver failed to start — check $RECEIVER_HOST:/tmp/strata-receiver.log"
info "Receiver started (PID $RECEIVER_PID)"
HLS_DIR=$(ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "grep -m1 'HLS temp dir:' /tmp/strata-receiver.log 2>/dev/null | sed -E 's/^.*HLS temp dir: ([^ ]+).*$/\\1/'" 2>/dev/null || echo "")
[[ -z "$HLS_DIR" ]] && HLS_DIR="/dev/shm/strata-hls-rx-${RECEIVER_PID}"
info "Receiver HLS dir: $HLS_DIR"

# ── Local HLS preview (dev): watch the stream without YouTube ────────
# A python http.server on the receiver serves the segment dir bound to
# 127.0.0.1 only (nothing exposed publicly); an SSH tunnel brings it to
# localhost here. Latency in the player is the full glass-to-glass chain
# minus YouTube's CDN: playout window (≤3 s) + 1 s segmentation + player
# buffer — use mpv's low-latency profile to keep the player's share small.
HLS_TUNNEL_PID=""
if [[ -n "$LOCAL_HLS_PORT" && "$LOCAL_HLS_PORT" != "0" ]]; then
    if ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "command -v python3" >/dev/null 2>&1; then
        # [h]ttp bracket trick: the wrapper shell's own cmdline contains this
        # whole string, so an unbracketed pattern makes pkill kill the shell
        # before the server ever starts (run orangepi-123888: preview dead,
        # "connection refused" through the tunnel).
        ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" \
            "pkill -f '[h]ttp\.server $LOCAL_HLS_PORT' 2>/dev/null; \
             setsid python3 -m http.server $LOCAL_HLS_PORT --bind 127.0.0.1 --directory '$HLS_DIR' \
               >/dev/null 2>&1 < /dev/null &" >/dev/null 2>&1 || true
        # Kill any stale tunnel holding the local port, whatever its target
        # form ("-L 8088:127.0.0.1:8088" vs "-L 8088:localhost:8088", …).
        pkill -f -- "-L ${LOCAL_HLS_PORT}:" 2>/dev/null || true
        ssh "${RECEIVER_SSH[@]}" -N -o ExitOnForwardFailure=yes \
            -L "${LOCAL_HLS_PORT}:127.0.0.1:${LOCAL_HLS_PORT}" "$RECEIVER_HOST" &
        HLS_TUNNEL_PID=$!
        sleep 1
        if kill -0 "$HLS_TUNNEL_PID" 2>/dev/null; then
            info "Local HLS preview: http://localhost:${LOCAL_HLS_PORT}/playlist.m3u8"
            echo "      mpv --profile=low-latency --cache=no http://localhost:${LOCAL_HLS_PORT}/playlist.m3u8"
            echo "      vlc --network-caching=1000 http://localhost:${LOCAL_HLS_PORT}/playlist.m3u8"
        else
            HLS_TUNNEL_PID=""
            warn "local HLS preview tunnel failed — port ${LOCAL_HLS_PORT} still busy? (STRATA_LOCAL_HLS_PORT to change)"
        fi
    else
        warn "python3 not found on receiver — local HLS preview disabled"
    fi
fi

# ── Start sender on the Orange Pi (camera + rkmpp HW encoder) ────────
echo ""
echo "── Starting sender on Orange Pi ($SENDER_HOST) ──"
ssh "${SENDER_SSH[@]}" "$SENDER_HOST" "pkill -INT strata-pipeline 2>/dev/null || true; sleep 2; pkill -KILL strata-pipeline 2>/dev/null || true; echo ok" >/dev/null 2>&1 || true
scp "${SENDER_SSH[@]}" -q "$SENDER_TOML" "$SENDER_HOST:/tmp/strata-sender.toml"

AUDIO_FLAG=""
case "${AUDIO_ENABLED,,}" in 1|true|yes|on) AUDIO_FLAG="--audio";; esac

TX_START=$(mktemp /tmp/opi-tx-start-XXXXXX.sh)
cat > "$TX_START" <<ENDSCRIPT
#!/bin/bash
export GST_PLUGIN_PATH=\$HOME/.local/share/gstreamer-1.0/plugins
# setsid (not nohup+disown): see the receiver launch above — nohup'd jobs
# die when the SSH session closes on this Pi; setsid keeps the sender alive.
setsid env RUST_LOG="$LOG_LEVEL" /usr/local/bin/strata-pipeline sender \\
  --dest "$DEST_STR" \\
  --source v4l2 --device "$VIDEO_DEVICE" \\
  --resolution "$RESOLUTION" --framerate "$FRAMERATE" \\
  --codec "$CODEC" \\
  --bitrate "$BITRATE" --min-bitrate "$MIN_BITRATE" --max-bitrate "$MAX_BITRATE" \\
  --startup-ramp-ms "$STARTUP_RAMP_MS" --startup-floor-kbps "$STARTUP_FLOOR_KBPS" \\
  $AUDIO_FLAG \\
  --config /tmp/strata-sender.toml \\
  > /tmp/strata-sender.log 2>&1 < /dev/null &
echo "PID: \$!"
ENDSCRIPT
scp "${SENDER_SSH[@]}" -q "$TX_START" "$SENDER_HOST:/tmp/start-sender.sh"
ssh "${SENDER_SSH[@]}" "$SENDER_HOST" "chmod +x /tmp/start-sender.sh && bash /tmp/start-sender.sh" >/dev/null
sleep 3
SENDER_PID=$(ssh "${SENDER_SSH[@]}" "$SENDER_HOST" "pgrep -n strata-pipeline" 2>/dev/null || echo "")
[[ -z "$SENDER_PID" ]] && fail "Sender failed to start — check $SENDER_HOST:/tmp/strata-sender.log"
info "Sender started (PID $SENDER_PID)"
# Report the encoder the Orange Pi actually selected.
ENC_LINE=$(ssh "${SENDER_SSH[@]}" "$SENDER_HOST" "grep -m1 '^Encoder:' /tmp/strata-sender.log 2>/dev/null" 2>/dev/null || echo "")
[[ -n "$ENC_LINE" ]] && info "Orange Pi $ENC_LINE"
echo "$ENC_LINE" | grep -qi 'software' && warn "Sender fell back to SOFTWARE encode — rkmpp not found on the Orange Pi (install gstreamer1.0-rockchip or gst-plugins-bad ≥1.24 with rkmpp)"

# ── Monitor ──────────────────────────────────────────────────────────
echo ""
echo "── Streaming for ${DURATION}s — monitoring every ${MONITOR_INTERVAL}s ──"
ARTIFACT_DIR="$REPO_ROOT/runs/orangepi-${SENDER_PID}"
mkdir -p "$ARTIFACT_DIR"
MAX_SEGS=0
CLEANED=0
RECEIVER_DIED=0
# Egress-stall tracking. File COUNT is not a progress signal (max-files
# rotation holds it constant), so progress = cumulative 'segment added'
# events in the receiver log. 2026-07-04 run 2: segment production froze at
# t≈25s (corrupt-PES timeline latch stalled the muxer) while every
# transport metric stayed green and the file count sat at 8 — invisible.
PREV_PRODUCED=0
STALL_TICKS=0
MAX_STALL_TICKS=0
RESTARTS=0

cleanup() {
    [[ $CLEANED -eq 1 ]] && return; CLEANED=1
    echo ""; echo "── Shutting down ──"
    [[ -n "$HLS_TUNNEL_PID" ]] && kill "$HLS_TUNNEL_PID" 2>/dev/null || true
    [[ -n "$LOCAL_HLS_PORT" && "$LOCAL_HLS_PORT" != "0" ]] && \
        ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "pkill -f 'http\.server $LOCAL_HLS_PORT' 2>/dev/null || true" >/dev/null 2>&1 || true
    ssh "${SENDER_SSH[@]}"   "$SENDER_HOST"   "pkill -INT strata-pipeline 2>/dev/null || true" >/dev/null 2>&1 || true
    ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "pkill -INT strata-pipeline 2>/dev/null || true" >/dev/null 2>&1 || true
    sleep 2
    scp "${RECEIVER_SSH[@]}" -q "$RECEIVER_HOST:/tmp/strata-receiver.log" "$ARTIFACT_DIR/receiver.log" 2>/dev/null || true
    scp "${SENDER_SSH[@]}"   -q "$SENDER_HOST:/tmp/strata-sender.log"     "$ARTIFACT_DIR/sender.log"   2>/dev/null || true
    info "Logs saved under $ARTIFACT_DIR"
}
trap cleanup EXIT INT TERM

num() { local v="${1//[^0-9]/}"; echo "${v:-0}"; }

ELAPSED=0
while [[ $ELAPSED -lt $DURATION ]]; do
    sleep "$MONITOR_INTERVAL"; ELAPSED=$((ELAPSED + MONITOR_INTERVAL))

    # Sender still alive?
    if ! ssh "${SENDER_SSH[@]}" "$SENDER_HOST" "kill -0 $SENDER_PID 2>/dev/null"; then
        warn "Sender process exited early — see $SENDER_HOST:/tmp/strata-sender.log"; break
    fi

    # Receiver still alive? A fatal GStreamer error (e.g. "Timestamping error
    # on input streams") kills it while the sender streams into the void — and
    # grepping the dead process's log tail would keep reporting the last-known
    # stats as if healthy (2026-07-04: 99 s of dead air passed as OK).
    if ! ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "kill -0 $RECEIVER_PID 2>/dev/null"; then
        warn "Receiver process exited early — see $RECEIVER_HOST:/tmp/strata-receiver.log"
        RECEIVER_DIED=1; break
    fi

    SEGS=$(ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "ls '$HLS_DIR'/*.ts 2>/dev/null | wc -l" 2>/dev/null || echo 0)
    SEGS=$(num "$SEGS"); [[ $SEGS -gt $MAX_SEGS ]] && MAX_SEGS=$SEGS
    PLAYLIST=$(ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "test -f '$HLS_DIR/playlist.m3u8' && echo yes || echo no" 2>/dev/null || echo no)

    # Cumulative segments ever produced — the real egress heartbeat.
    PRODUCED=$(num "$(ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "grep -c 'segment added' /tmp/strata-receiver.log 2>/dev/null" 2>/dev/null || echo 0)")
    if [[ $PRODUCED -gt $PREV_PRODUCED ]]; then
        STALL_TICKS=0
    elif [[ $PRODUCED -gt 0 ]]; then
        STALL_TICKS=$((STALL_TICKS + 1))
        [[ $STALL_TICKS -gt $MAX_STALL_TICKS ]] && MAX_STALL_TICKS=$STALL_TICKS
        if [[ $STALL_TICKS -eq 4 ]]; then
            warn "HLS egress STALLED: no new segment for $((STALL_TICKS * MONITOR_INTERVAL))s while both processes are alive (transport metrics can stay green through this — check the receiver gate logs)"
        fi
    fi
    PREV_PRODUCED=$PRODUCED

    # Egress-watchdog self-heals: the receiver rebuilds its own pipeline when
    # segment production wedges (run 4 would have been ~90 s of dead air).
    RESTARTS=$(num "$(ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "grep -c 'egress-watchdog: no HLS segment' /tmp/strata-receiver.log 2>/dev/null" 2>/dev/null || echo 0)")

    # Latest receiver stats line.
    STATS=$(ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" "grep 'strata-stats' /tmp/strata-receiver.log 2>/dev/null | tail -1" 2>/dev/null || echo "")
    DELIVERED=$(num "$(grep -oP 'packets_delivered=\(guint64\)\K[0-9]+' <<<"$STATS" | tail -1)")
    DISCONT=$(num "$(grep -oP 'discontinuities=\(guint64\)\K[0-9]+' <<<"$STATS" | tail -1)")
    LOST=$(num "$(grep -oP 'lost_packets=\(guint64\)\K[0-9]+' <<<"$STATS" | tail -1)")
    LATE=$(num "$(grep -oP 'late_packets=\(guint64\)\K[0-9]+' <<<"$STATS" | tail -1)")
    LAT=$(num "$(grep -oP 'current_latency_ms=\(guint64\)\K[0-9]+' <<<"$STATS" | tail -1)")

    YT=""
    if [[ -n "${YOUTUBE_API_KEY:-}" && -n "${YOUTUBE_STREAM_ID:-}" ]]; then
        s=$(curl -s --max-time 2 "https://www.googleapis.com/youtube/v3/liveStreams?part=status&id=${YOUTUBE_STREAM_ID}&key=${YOUTUBE_API_KEY}" | grep -oP '"healthStatus":\s*\{\s*"status":\s*"\K[^"]+' | head -1 || echo "")
        [[ -n "$s" ]] && YT=" yt_health=$s"
    fi

    STALL_STR=""; [[ $STALL_TICKS -gt 0 ]] && STALL_STR=" STALLED=$((STALL_TICKS * MONITOR_INTERVAL))s"
    RESTART_STR=""; [[ $RESTARTS -gt 0 ]] && RESTART_STR=" wd_restarts=$RESTARTS"
    echo "╌╌╌ [${ELAPSED}s] produced=$PRODUCED segs (dir=$SEGS) playlist=$PLAYLIST$STALL_STR$RESTART_STR$YT ╌╌╌"
    echo "  RX: delivered=$DELIVERED lost=$LOST late=$LATE discont=$DISCONT playout=${LAT}ms"
done

# ── Verdict ──────────────────────────────────────────────────────────
echo ""
RX_FATAL=$(ssh "${RECEIVER_SSH[@]}" "$RECEIVER_HOST" \
    "grep -m1 -E 'Timestamping error on input streams|^Error:' /tmp/strata-receiver.log 2>/dev/null" \
    2>/dev/null || echo "")
RESTART_STR=""; [[ $RESTARTS -gt 0 ]] && RESTART_STR=" — $RESTARTS egress-watchdog restart(s)"
if [[ $RECEIVER_DIED -eq 1 || -n "$RX_FATAL" ]]; then
    VERDICT="FAILED: receiver died mid-run${RX_FATAL:+ — fatal: $RX_FATAL} (produced $PREV_PRODUCED segment(s) before death)"; warn "$VERDICT"
elif [[ $STALL_TICKS -ge 4 ]]; then
    VERDICT="FAILED: HLS egress stalled and never recovered ($((STALL_TICKS * MONITOR_INTERVAL))s at run end; produced $PREV_PRODUCED segments total$RESTART_STR) — YouTube went dark even though both processes stayed up"; warn "$VERDICT"
elif [[ $MAX_STALL_TICKS -ge 4 ]]; then
    VERDICT="RECOVERED: egress stalled mid-run (max $((MAX_STALL_TICKS * MONITOR_INTERVAL))s) but resumed$RESTART_STR — $PREV_PRODUCED segments total; check receiver.log watchdog/gate lines"; warn "$VERDICT"
elif [[ $MAX_SEGS -ge 2 && "$PLAYLIST" == "yes" ]]; then
    VERDICT="OK: $PREV_PRODUCED segments produced + playlist (lost=$LOST late=$LATE discont=$DISCONT)$RESTART_STR"; info "$VERDICT"
elif [[ $MAX_SEGS -ge 1 ]]; then
    VERDICT="PARTIAL: only $MAX_SEGS segment(s) — check receiver.log for timestamping errors / single-segment stall"; warn "$VERDICT"
else
    VERDICT="NO SEGMENTS — YouTube saw nothing; inspect $ARTIFACT_DIR/{sender,receiver}.log"; warn "$VERDICT"
fi
# The verdict travels with the logs — forensics on a run dir shouldn't have
# to reconstruct what the live monitor already concluded (2026-07-04 run 4).
echo "$VERDICT" > "$ARTIFACT_DIR/verdict.txt"
# cleanup() runs on EXIT
