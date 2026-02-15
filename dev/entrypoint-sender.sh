#!/bin/sh
# ── Strata Sender Entrypoint ───────────────────────────────────────
#
# Applies tc netem impairments to each network interface before
# starting the strata-agent daemon.  This simulates realistic
# cellular uplink conditions across bonded links.
#
# Each LINKn_IMPAIRMENT env var has the format:
#   "RATE_KBIT DELAY_MS JITTER_MS LOSS_PERCENT"
#
# Defaults model typical LTE/5G cellular uplinks:
#   link0: 1500 kbit, 40ms ±10ms, 1% loss   (decent LTE)
#   link1:  800 kbit, 60ms ±15ms, 2% loss   (poor LTE)
#   link2: 1200 kbit, 35ms  ±8ms, 0.5% loss (good LTE)
#
# The aggregate is ~3500 kbps — within the 2000–7000 kbps range
# that 3-4 bonded cellular links deliver in the real world.
#
# Requires NET_ADMIN capability and iproute2 installed.

set -e

# ── Apply impairments ──────────────────────────────────────────────

apply_impairment() {
    local iface="$1"
    local rate_kbit="$2"
    local delay_ms="$3"
    local jitter_ms="$4"
    local loss_pct="$5"

    if [ -z "$rate_kbit" ]; then
        echo "[netem] skipping $iface (no config)"
        return
    fi

    # Remove existing qdisc (ignore error if none)
    tc qdisc del dev "$iface" root 2>/dev/null || true

    echo "[netem] $iface: rate=${rate_kbit}kbit delay=${delay_ms}ms ±${jitter_ms}ms loss=${loss_pct}%"
    tc qdisc add dev "$iface" root netem \
        rate "${rate_kbit}kbit" \
        delay "${delay_ms}ms" "${jitter_ms}ms" \
        loss "${loss_pct}%" \
        limit 10000
}

# Match impairments to interfaces by subnet rather than by interface
# index.  Docker may connect networks in an unpredictable order, so
# we look at the IP address on each interface and map 172.30.X.0/24
# to LINK{X}_IMPAIRMENT.  Non-matching interfaces (management network)
# are left untouched.
for iface in $(ls /sys/class/net/ | sort); do
    [ "$iface" = "lo" ] && continue

    # Get the first IPv4 address on this interface
    IP=$(ip -4 -o addr show dev "$iface" 2>/dev/null | awk '{print $4}' | head -1 | cut -d/ -f1)

    if [ -z "$IP" ]; then
        echo "[netem] $iface: no IPv4 address, skipping"
        continue
    fi

    # Extract the third octet to determine which link network
    # Format: 172.30.X.Y  →  X is the link index
    OCTET3=$(echo "$IP" | cut -d. -f3)
    OCTET1=$(echo "$IP" | cut -d. -f1)
    OCTET2=$(echo "$IP" | cut -d. -f2)

    if [ "$OCTET1" = "172" ] && [ "$OCTET2" = "30" ]; then
        # This is a link interface (172.30.X.0/24)
        IMPAIRMENT_VAR="LINK${OCTET3}_IMPAIRMENT"
        eval IMPAIRMENT="\$$IMPAIRMENT_VAR"

        if [ -n "$IMPAIRMENT" ]; then
            apply_impairment "$iface" $IMPAIRMENT
        else
            echo "[netem] $iface ($IP): link${OCTET3}, no $IMPAIRMENT_VAR set"
        fi
    else
        echo "[netem] $iface ($IP): management network, no impairment"
    fi
done

echo "[netem] impairment setup complete"
echo ""

# ── Start the agent ────────────────────────────────────────────────
exec strata-agent "$@"
