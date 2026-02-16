#!/usr/bin/env bash
# Run the RIST bonding → YouTube pipeline on the pre-configured network.
# Run setup_network.sh first.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN=/workspaces/rist-bonding/target/debug/integration_node
LOG_DIR="$SCRIPT_DIR/logs"
STREAM_KEY="1j25-q9ky-5ej8-kemh-96ah"
RTMP_URL="rtmp://a.rtmp.youtube.com/live2/${STREAM_KEY}"
NS="yt_snd"
DURATION=${1:-60}

mkdir -p "$LOG_DIR"
rm -f "$LOG_DIR"/*.log "$LOG_DIR"/*.jsonl

# GStreamer debug levels — crank bonding to 5 (DEBUG)
GST_DBG="2,rsristbondsink:5,rsristbondsrc:5,mpegtsmux:3,x264enc:3,rtmpsink:4,flvmux:4"

echo "================================================================="
echo "  RIST Bonding → YouTube Live Test"
echo "  RTMP: $RTMP_URL"
echo "  Duration: ${DURATION}s"
echo "  Encoder: 1000 kbps video + 128 kbps audio"
echo "  Links: A=2000k/30ms B=2000k/50ms C=1500k/40ms"
echo "================================================================="

# ── Receiver (host namespace — has internet for RTMP) ────────────────
echo "Starting receiver..."
GST_DEBUG="$GST_DBG" \
RUST_LOG="info,rist_bonding_core=debug" \
$BIN receiver \
    --bind "rist://@10.99.1.1:7000,rist://@10.99.2.1:7002,rist://@10.99.3.1:7004" \
    --relay-url "$RTMP_URL" \
    --config "$SCRIPT_DIR/receiver.toml" \
    > "$LOG_DIR/receiver.log" 2>&1 &
RCV_PID=$!
echo "  Receiver PID=$RCV_PID"
sleep 3

if ! kill -0 "$RCV_PID" 2>/dev/null; then
    echo "ERROR: Receiver died. Log tail:"
    tail -20 "$LOG_DIR/receiver.log"
    exit 1
fi

# ── Sender (yt_snd namespace) ────────────────────────────────────────
echo "Starting sender..."
sudo ip netns exec "$NS" env \
    GST_DEBUG="$GST_DBG" \
    RUST_LOG="info,rist_bonding_core=debug" \
    PATH="$PATH" HOME="$HOME" \
    $BIN sender \
    --source test \
    --audio \
    --bitrate 1000 \
    --framerate 30 \
    --dest "rist://10.99.1.1:7000,rist://10.99.2.1:7002,rist://10.99.3.1:7004" \
    --config "$SCRIPT_DIR/sender.toml" \
    --stats-dest "10.99.1.1:9200" \
    > "$LOG_DIR/sender.log" 2>&1 &
SND_PID=$!
echo "  Sender PID=$SND_PID"

# ── Stats collector ──────────────────────────────────────────────────
(socat -u UDP-RECV:9200,reuseaddr STDOUT 2>/dev/null | while IFS= read -r line; do
    echo "[$(date +%H:%M:%S)] $line"
done > "$LOG_DIR/stats.jsonl") &
STATS_PID=$!

# ── Monitor ──────────────────────────────────────────────────────────
echo "Streaming for ${DURATION}s..."
echo ""
for ((t=0; t<DURATION; t+=10)); do
    sleep 10 || break
    elapsed=$((t + 10))
    snd_ok="yes"; kill -0 "$SND_PID" 2>/dev/null || snd_ok="NO"
    rcv_ok="yes"; kill -0 "$RCV_PID" 2>/dev/null || rcv_ok="NO"

    # Show sender log progress
    snd_lines=$(wc -l < "$LOG_DIR/sender.log" 2>/dev/null || echo 0)
    rcv_lines=$(wc -l < "$LOG_DIR/receiver.log" 2>/dev/null || echo 0)
    stats_lines=$(wc -l < "$LOG_DIR/stats.jsonl" 2>/dev/null || echo 0)

    echo "[${elapsed}s] snd=${snd_ok}(${snd_lines}L) rcv=${rcv_ok}(${rcv_lines}L) stats=${stats_lines}L"

    if [[ "$snd_ok" == "NO" && "$rcv_ok" == "NO" ]]; then
        echo "Both exited —stopping early."
        break
    fi
done

# ── Shutdown ─────────────────────────────────────────────────────────
echo ""
echo "=== Shutting down ==="
kill -INT "$SND_PID" 2>/dev/null || true
sleep 3
kill -INT "$RCV_PID" 2>/dev/null || true
sleep 2
kill "$STATS_PID" 2>/dev/null || true

# Ensure everything is dead
kill -9 "$SND_PID" "$RCV_PID" "$STATS_PID" 2>/dev/null || true
wait 2>/dev/null

echo ""
echo "=== LOG SUMMARY ==="
echo "  Sender:   $(wc -l < "$LOG_DIR/sender.log") lines"
echo "  Receiver: $(wc -l < "$LOG_DIR/receiver.log") lines"
echo "  Stats:    $(wc -l < "$LOG_DIR/stats.jsonl" 2>/dev/null || echo 0) lines"

echo ""
echo "=== SENDER LOG (last 40) ==="
tail -40 "$LOG_DIR/sender.log"

echo ""
echo "=== RECEIVER LOG (last 40) ==="
tail -40 "$LOG_DIR/receiver.log"

echo ""
echo "=== STATS (last 5) ==="
tail -5 "$LOG_DIR/stats.jsonl" 2>/dev/null || echo "(none)"
