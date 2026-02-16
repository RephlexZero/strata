#!/usr/bin/env bash
# Set up the full 3-link network topology for the YouTube test.
# Idempotent — safe to run multiple times.
set -euo pipefail

NS="yt_snd"

echo "=== Cleanup ==="
for v in yt_a0 yt_b0 yt_c0; do
    sudo ip link del "$v" 2>/dev/null || true
done
sudo ip netns del "$NS" 2>/dev/null || true
sleep 0.5

echo "=== Create namespace ==="
sudo ip netns add "$NS"
sudo ip netns exec "$NS" ip link set lo up

echo "=== Create veth pairs ==="
for lnk in a b c; do
    case $lnk in
        a) subnet="10.99.1" ;;
        b) subnet="10.99.2" ;;
        c) subnet="10.99.3" ;;
    esac
    host="yt_${lnk}0"
    snd="yt_${lnk}1"

    sudo ip link add "$host" type veth peer name "$snd"
    sudo ip link set "$snd" netns "$NS"
    sudo ip addr add "${subnet}.1/24" dev "$host"
    sudo ip link set "$host" up
    sudo ip netns exec "$NS" ip addr add "${subnet}.2/24" dev "$snd"
    sudo ip netns exec "$NS" ip link set "$snd" up
    echo "  $host (${subnet}.1) <-> $snd (${subnet}.2)"
done

echo "=== Apply netem ==="
sudo ip netns exec "$NS" tc qdisc add dev yt_a1 root netem delay 30ms 5ms distribution normal loss random 0.5% rate 2000kbit
sudo ip netns exec "$NS" tc qdisc add dev yt_b1 root netem delay 50ms 8ms distribution normal loss random 1.0% rate 2000kbit
sudo ip netns exec "$NS" tc qdisc add dev yt_c1 root netem delay 40ms 3ms distribution normal loss random 0.2% rate 1500kbit
echo "  A: 2000kbit 30ms 0.5%  B: 2000kbit 50ms 1.0%  C: 1500kbit 40ms 0.2%"

echo "=== Verify connectivity ==="
for lnk in a b c; do
    case $lnk in
        a) subnet="10.99.1" ;;
        b) subnet="10.99.2" ;;
        c) subnet="10.99.3" ;;
    esac
    # Forward
    sudo ip netns exec "$NS" ping -c1 -W2 "${subnet}.1" > /dev/null 2>&1 && f="OK" || f="FAIL"
    # Reverse
    ping -c1 -W2 "${subnet}.2" > /dev/null 2>&1 && r="OK" || r="FAIL"
    echo "  Link $lnk: fwd=$f  rev=$r"
done

echo "=== Done ==="
