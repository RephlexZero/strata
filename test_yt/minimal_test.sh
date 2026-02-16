#!/usr/bin/env bash
# Minimal 1-link RIST test through veth (no netem, no RTMP)
set -euo pipefail

BIN=/workspaces/rist-bonding/target/debug/integration_node

# Ensure namespace exists
sudo ip netns list | grep -q yt_snd || { echo "yt_snd ns not found"; exit 1; }

# Kill any lingering test processes (not Docker ones)
pgrep -f "integration_node receiver.*7000" | xargs -r kill 2>/dev/null || true
sleep 0.5

echo "=== Starting receiver ==="
GST_DEBUG="2,rsristbondsrc:5" \
RUST_LOG="info,rist_bonding_core=debug" \
$BIN receiver --bind "rist://@10.99.1.1:7000" > /tmp/rcv_minimal.log 2>&1 &
RCV=$!
echo "  PID=$RCV"
sleep 2

echo "=== Starting sender (in yt_snd ns) ==="
sudo ip netns exec yt_snd env \
    GST_DEBUG="2,rsristbondsink:5" \
    RUST_LOG="info,rist_bonding_core=debug" \
    PATH="$PATH" HOME="$HOME" \
    $BIN sender --source test --bitrate 500 --framerate 15 \
    --dest "rist://10.99.1.1:7000" > /tmp/snd_minimal.log 2>&1 &
SND=$!
echo "  PID=$SND"

echo "=== Running for 10s ==="
sleep 10

echo "=== Stopping ==="
kill -INT $SND 2>/dev/null || true
sleep 2
kill -INT $RCV 2>/dev/null || true
sleep 1
kill -9 $SND $RCV 2>/dev/null || true

echo ""
echo "========== RECEIVER LOG (last 30 lines) =========="
tail -30 /tmp/rcv_minimal.log
echo ""
echo "========== SENDER LOG (last 30 lines) =========="
tail -30 /tmp/snd_minimal.log
echo ""
echo "========== Sender line count =========="
wc -l /tmp/snd_minimal.log
echo "========== Receiver line count =========="
wc -l /tmp/rcv_minimal.log
