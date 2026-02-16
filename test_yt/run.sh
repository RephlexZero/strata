#!/usr/bin/env bash
#
# YouTube Live test via bonded RIST + RTMP relay
#
# Architecture:
#   [sender in yt_snd netns] ──3 veth links──> [receiver in host netns] ──RTMP──> YouTube
#
# Sender runs in an isolated network namespace with tc netem impairment on each veth.
# Receiver runs in the host namespace (has internet access for RTMP relay to YouTube).
#
# Aggregate link capacity: ~5500 kbit/s raw  →  ~5000 kbit/s usable after overhead
#   Link A: 2000 kbit/s, 30 ms delay, 0.5% loss
#   Link B: 2000 kbit/s, 50 ms delay, 1.0% loss
#   Link C: 1500 kbit/s, 40 ms delay, 0.2% loss
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="$WORKSPACE/target/debug/integration_node"
LOG_DIR="$SCRIPT_DIR/logs"
STREAM_KEY="1j25-q9ky-5ej8-kemh-96ah"
RTMP_URL="rtmp://a.rtmp.youtube.com/live2/${STREAM_KEY}"

NS="yt_snd"
DURATION=${1:-60}   # seconds to stream (default: 60)

mkdir -p "$LOG_DIR"

# ── Cleanup ──────────────────────────────────────────────────────────
cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    # Kill background jobs (receiver)
    jobs -p 2>/dev/null | xargs -r kill 2>/dev/null || true
    # Kill sender in namespace (if lingering)
    sudo ip netns pids "$NS" 2>/dev/null | xargs -r sudo kill 2>/dev/null || true
    sleep 1
    # Remove veths (host side — peer side auto-deleted)
    for v in yt_a0 yt_b0 yt_c0; do
        sudo ip link del "$v" 2>/dev/null || true
    done
    # Remove namespace
    sudo ip netns del "$NS" 2>/dev/null || true
    echo "Cleanup done."
}
trap cleanup EXIT

# ── Build binary if stale ────────────────────────────────────────────
echo "=== Building integration_node ==="
(cd "$WORKSPACE" && cargo build --bin integration_node 2>&1 | tail -3)
if [[ ! -x "$BIN" ]]; then
    echo "ERROR: $BIN not found after build"
    exit 1
fi
echo "Binary: $BIN"

# ── Tear down stale resources ───────────────────────────────────────
echo "=== Tearing down stale netns/veths ==="
for v in yt_a0 yt_b0 yt_c0; do
    sudo ip link del "$v" 2>/dev/null || true
done
sudo ip netns del "$NS" 2>/dev/null || true

# ── Create namespace ────────────────────────────────────────────────
echo "=== Creating namespace: $NS ==="
sudo ip netns add "$NS"
sudo ip netns exec "$NS" ip link set lo up

# ── Create veth pairs ───────────────────────────────────────────────
# Each pair: host-side (yt_Xo) <──veth──> sender-side (yt_X1) in $NS
#
#   Link A: host 10.99.1.1/24 ↔ sender 10.99.1.2/24
#   Link B: host 10.99.2.1/24 ↔ sender 10.99.2.2/24
#   Link C: host 10.99.3.1/24 ↔ sender 10.99.3.2/24

declare -A LINKS
LINKS[a]="10.99.1"
LINKS[b]="10.99.2"
LINKS[c]="10.99.3"

for lnk in a b c; do
    subnet="${LINKS[$lnk]}"
    host_veth="yt_${lnk}0"
    snd_veth="yt_${lnk}1"

    sudo ip link add "$host_veth" type veth peer name "$snd_veth"
    sudo ip link set "$snd_veth" netns "$NS"

    sudo ip addr add "${subnet}.1/24" dev "$host_veth"
    sudo ip link set "$host_veth" up

    sudo ip netns exec "$NS" ip addr add "${subnet}.2/24" dev "$snd_veth"
    sudo ip netns exec "$NS" ip link set "$snd_veth" up

    echo "  veth $host_veth (${subnet}.1) <-> $snd_veth (${subnet}.2)"
    sleep 0.5   # let the kernel settle before creating next pair
done

# ── Apply tc netem impairment (sender-side veths inside netns) ──────
echo "=== Applying netem impairment ==="
#   Link A: 2000 kbit, 30 ms, 0.5% loss
sudo ip netns exec "$NS" tc qdisc add dev yt_a1 root netem \
    delay 30ms 5ms distribution normal loss random 0.5% rate 2000kbit
echo "  Link A (yt_a1): 2000 kbit, 30ms ±5ms, 0.5% loss"

#   Link B: 2000 kbit, 50 ms, 1.0% loss
sudo ip netns exec "$NS" tc qdisc add dev yt_b1 root netem \
    delay 50ms 8ms distribution normal loss random 1.0% rate 2000kbit
echo "  Link B (yt_b1): 2000 kbit, 50ms ±8ms, 1.0% loss"

#   Link C: 1500 kbit, 40 ms, 0.2% loss
sudo ip netns exec "$NS" tc qdisc add dev yt_c1 root netem \
    delay 40ms 3ms distribution normal loss random 0.2% rate 1500kbit
echo "  Link C (yt_c1): 1500 kbit, 40ms ±3ms, 0.2% loss"

# ── Verify connectivity ─────────────────────────────────────────────
echo "=== Verifying connectivity ==="
for lnk in a b c; do
    subnet="${LINKS[$lnk]}"
    sudo ip netns exec "$NS" ping -c 1 -W 2 "${subnet}.1" >/dev/null 2>&1 \
        && echo "  Link $lnk: OK" \
        || echo "  Link $lnk: FAIL"
done

# ── GStreamer debug levels ───────────────────────────────────────────
# Level reference:  1=ERROR  2=WARN  3=FIXME  4=INFO  5=DEBUG  6=LOG  7=TRACE
#
# We crank the custom RIST bonding elements to 5 (DEBUG) and leave
# everything else at 2 (WARN) to keep noise down. Bump to 6/7 if
# you need packet-level tracing.
GST_DBG="2,rsristbondsink:5,rsristbondsrc:5,ristbonding*:5,mpegtsmux:3,x264enc:3,rtmpsink:4,flvmux:4"

echo ""
echo "================================================================="
echo "  RIST Bonding → YouTube Live Test"
echo "  RTMP: $RTMP_URL"
echo "  Duration: ${DURATION}s"
echo "  Links: 3 (A=2000k/30ms, B=2000k/50ms, C=1500k/40ms)"
echo "  Aggregate: ~5500 kbit raw, ~5000 kbit usable"
echo "  Encoder bitrate: 1000 kbps (must fit single link for broadcast startup)"
echo "  Logs: $LOG_DIR/"
echo "================================================================="
echo ""

# ── Start receiver (host namespace — has internet for RTMP) ─────────
echo "=== Starting receiver (host namespace) ==="
GST_DEBUG="$GST_DBG" \
RUST_LOG="info,rist_bonding_core=debug" \
"$BIN" receiver \
    --bind "rist://@10.99.1.1:7000,rist://@10.99.2.1:7002,rist://@10.99.3.1:7004" \
    --relay-url "$RTMP_URL" \
    --config "$SCRIPT_DIR/receiver.toml" \
    2>&1 | tee "$LOG_DIR/receiver.log" &

RCV_PID=$!
echo "  Receiver PID: $RCV_PID"

# Give receiver time to bind all ports
sleep 3

if ! kill -0 "$RCV_PID" 2>/dev/null; then
    echo "ERROR: Receiver exited early. Check $LOG_DIR/receiver.log"
    cat "$LOG_DIR/receiver.log"
    exit 1
fi

# ── Start sender (inside yt_snd namespace) ───────────────────────────
echo "=== Starting sender (namespace: $NS) ==="
sudo ip netns exec "$NS" env \
    GST_DEBUG="$GST_DBG" \
    RUST_LOG="info,rist_bonding_core=debug" \
    GST_PLUGIN_PATH="${GST_PLUGIN_PATH:-}" \
    PATH="$PATH" \
    HOME="$HOME" \
    "$BIN" sender \
    --source test \
    --audio \
    --bitrate 1000 \
    --framerate 30 \
    --dest "rist://10.99.1.1:7000,rist://10.99.2.1:7002,rist://10.99.3.1:7004"
    --config "$SCRIPT_DIR/sender.toml" \
    --stats-dest "10.99.1.1:9200" \
    2>&1 | tee "$LOG_DIR/sender.log" &

SND_PID=$!
echo "  Sender PID: $SND_PID"

# ── Stats collector (optional — runs in host namespace) ──────────────
# Collect UDP stats JSON from the sender on port 9100 in background
echo "=== Starting stats collector ==="
(
    exec 2>/dev/null
    socat -u UDP-RECV:9200,reuseaddr STDOUT 2>/dev/null | \
        while IFS= read -r line; do
            echo "[$(date +%H:%M:%S)] $line"
        done > "$LOG_DIR/stats.jsonl"
) &
STATS_PID=$!
echo "  Stats collector PID: $STATS_PID (→ $LOG_DIR/stats.jsonl)"

# ── Run for DURATION seconds, printing periodic status ──────────────
echo ""
echo "=== Streaming for ${DURATION}s … ==="
echo "    Tail logs:  tail -f $LOG_DIR/sender.log"
echo "                tail -f $LOG_DIR/receiver.log"
echo "    Stats:      tail -f $LOG_DIR/stats.jsonl"
echo ""

for ((t=0; t<DURATION; t+=10)); do
    sleep 10 || break
    elapsed=$((t + 10))

    # Quick health check
    snd_alive="yes"; kill -0 "$SND_PID" 2>/dev/null || snd_alive="NO"
    rcv_alive="yes"; kill -0 "$RCV_PID" 2>/dev/null || rcv_alive="NO"

    echo "[${elapsed}s/${DURATION}s] sender=$snd_alive  receiver=$rcv_alive"

    if [[ "$snd_alive" == "NO" && "$rcv_alive" == "NO" ]]; then
        echo "Both processes exited — stopping early."
        break
    fi
done

# ── Graceful shutdown ────────────────────────────────────────────────
echo ""
echo "=== Shutting down ==="
# Send SIGINT to sender (triggers EOS)
kill -INT "$SND_PID" 2>/dev/null || true
sleep 3
# Send SIGINT to receiver
kill -INT "$RCV_PID" 2>/dev/null || true
sleep 2
# Kill stats collector
kill "$STATS_PID" 2>/dev/null || true

echo ""
echo "=== Logs ==="
echo "  Sender:   $LOG_DIR/sender.log   ($(wc -l < "$LOG_DIR/sender.log") lines)"
echo "  Receiver: $LOG_DIR/receiver.log ($(wc -l < "$LOG_DIR/receiver.log") lines)"
echo "  Stats:    $LOG_DIR/stats.jsonl  ($(wc -l < "$LOG_DIR/stats.jsonl" 2>/dev/null || echo 0) lines)"

echo ""
echo "=== Last 30 lines of sender log ==="
tail -30 "$LOG_DIR/sender.log"
echo ""
echo "=== Last 30 lines of receiver log ==="
tail -30 "$LOG_DIR/receiver.log"
