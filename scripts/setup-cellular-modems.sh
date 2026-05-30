#!/usr/bin/env bash
# scripts/setup-cellular-modems.sh — Initialize USB cellular modems for field testing

set -euo pipefail

echo "═══ Cellular Modem Setup ═══"
echo ""

# Load modem drivers
echo "Loading modem drivers..."
sudo modprobe huawei_cdc_ncm 2>/dev/null || true
sudo modprobe cdc_ncm 2>/dev/null || true
sudo modprobe cdc_ether 2>/dev/null || true

echo ""
echo "── Discovered Modems ──"
echo ""

# Check Huawei (12d1:14db @ 3-1.3 → enp11s0f3u1u3)
HUAWEI_IFACE="enp11s0f3u1u3"
HUAWEI_IP=$(ip addr show dev "$HUAWEI_IFACE" 2>/dev/null | grep -oP 'inet \K[0-9.]+' | head -1 || echo "unconfigured")
HUAWEI_CARRIER=$(cat /sys/class/net/"$HUAWEI_IFACE"/carrier 2>/dev/null || echo "unknown")
echo "Huawei (12d1:14db):"
echo "  Interface: $HUAWEI_IFACE"
echo "  IPv4: $HUAWEI_IP"
echo "  Carrier: $HUAWEI_CARRIER"
echo ""

# Check ZTE (19d2:1405 @ 1-3 → enp2s0f0u3)
ZTE_IFACE="enp2s0f0u3"
ZTE_IP=$(ip addr show dev "$ZTE_IFACE" 2>/dev/null | grep -oP 'inet \K[0-9.]+' | head -1 || echo "unconfigured")
ZTE_CARRIER=$(cat /sys/class/net/"$ZTE_IFACE"/carrier 2>/dev/null || echo "unknown")
echo "ZTE (19d2:1405):"
echo "  Interface: $ZTE_IFACE"
echo "  IPv4: $ZTE_IP"
echo "  Carrier: $ZTE_CARRIER"
echo ""

# Ensure interfaces are up
echo "Bringing interfaces up..."
sudo ip link set "$HUAWEI_IFACE" up 2>/dev/null || true
sudo ip link set "$ZTE_IFACE" up 2>/dev/null || true

# Ensure ZTE has an IP (may be disconnected but network-stack-ready)
if [[ "$ZTE_IP" == "unconfigured" ]]; then
    echo "Assigning static IP to ZTE (no carrier expected)..."
    sudo ip addr add 192.168.1.100/24 dev "$ZTE_IFACE" 2>/dev/null || echo "  (already assigned or failed)"
fi

echo ""
echo "── Field Test Configuration ──"
echo ""
echo "Two modems are now bonded-ready:"
echo "  export STRATA_LINK_IFACES=\"$HUAWEI_IFACE,$ZTE_IFACE\""
echo ""
echo "Full setup (with required fields):"
echo ""
echo "  export STRATA_RECEIVER_HOST=<receiver-ip-or-hostname>"
echo "  export STRATA_RECEIVER_PORTS=\"5000,5002\""
echo "  export STRATA_RELAY_URL=\"https://...\""
echo "  export STRATA_LINK_IFACES=\"$HUAWEI_IFACE,$ZTE_IFACE\""
echo "  ./scripts/field-test.sh"
echo ""
echo "Or use the .env file:"
echo "  source .env.field-test-modems"
echo "  # (edit required fields: STRATA_RECEIVER_HOST, STRATA_RELAY_URL)"
echo "  ./scripts/field-test.sh"
echo ""
