#!/usr/bin/env bash
# Quick 3-link test: no netem, no RTMP, just check if RIST sender queue works
set -euo pipefail
BIN=/workspaces/rist-bonding/target/debug/integration_node
NS="yt_snd"

# Clean residual netem
sudo ip netns exec "$NS" tc qdisc del dev yt_a1 root 2>/dev/null || true
sudo ip netns exec "$NS" tc qdisc del dev yt_b1 root 2>/dev/null || true
sudo ip netns exec "$NS" tc qdisc del dev yt_c1 root 2>/dev/null || true

pkill -f "integration_node.*7000" 2>/dev/null || true
pkill -f "integration_node.*7002" 2>/dev/null || true
sleep 1

echo "=== Starting receiver (3 links, no RTMP) ==="
GST_DEBUG="2,rsristbondsrc:4" RUST_LOG="info,rist_bonding_core=debug" \
$BIN receiver --bind "rist://@10.99.1.1:7000,rist://@10.99.2.1:7002,rist://@10.99.3.1:7004" \
    > /tmp/rcv_3link.log 2>&1 &
RCV=$!
echo "  PID=$RCV"
sleep 2

echo "=== Starting sender (3 links, 500kbps, 15fps) ==="
sudo ip netns exec "$NS" env \
    GST_DEBUG="2,rsristbondsink:5" \
    RUST_LOG="info,rist_bonding_core=debug" \
    PATH="$PATH" HOME="$HOME" \
    $BIN sender --source test --bitrate 500 --framerate 15 \
    --dest "rist://10.99.1.1:7000,rist://10.99.2.1:7002,rist://10.99.3.1:7004" \
    > /tmp/snd_3link.log 2>&1 &
SND=$!
echo "  PID=$SND"

echo "=== Running 15s ==="
sleep 15

echo "=== Stopping ==="
kill -INT $SND 2>/dev/null || true; sleep 2
kill -INT $RCV 2>/dev/null || true; sleep 1
kill -9 $SND $RCV 2>/dev/null || true

echo ""
echo "=== Sender (first 20 lines) ==="
head -20 /tmp/snd_3link.log
echo ""
echo "=== Sender send-failed count ==="
grep -c "send failed" /tmp/snd_3link.log || echo "0"
echo ""
echo "=== Sender total lines ==="
wc -l /tmp/snd_3link.log
echo ""
echo "=== Receiver (last 10) ==="
tail -10 /tmp/rcv_3link.log
echo ""
echo "=== Receiver total lines ==="
wc -l /tmp/rcv_3link.log
