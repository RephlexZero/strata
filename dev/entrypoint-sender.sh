#!/bin/sh
# ── Strata Sender Entrypoint ───────────────────────────────────────
#
# Applies tc netem impairments to each network interface before
# starting the strata-agent daemon.  This simulates realistic
# cellular uplink conditions across bonded links.
#
# Each LINKn_IMPAIRMENT env var has the format:
#   "RATE_KBIT DELAY_MS JITTER_MS LOSS_PERCENT LOSS_CORR_PERCENT"
#
# Profiles modelled from production-grade cellular measurements:
#   link0: 8000 kbit, 22ms ±8ms,  0.5% loss (25% corr) — LTE urban
#   link1: 5000 kbit, 30ms ±15ms, 2.0% loss (30% corr) — LTE poor
#   link2: 6000 kbit, 18ms ±5ms,  0.3% loss (15% corr) — LTE good
#
# The aggregate is ~19 Mbps — representative of 3 bonded LTE uplinks
# with dedicated SIM cards on commodity USB modems.
#
# Extras applied unconditionally:
#   - HTB shaper → netem child (proper token-bucket + impairment)
#   - `distribution normal` on jitter (Gaussian, not uniform)
#   - Auto-computed queue limit (~100ms of buffering at link rate)
#   - 0.05% corruption on all links
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
    local loss_corr="${6:-25}"

    if [ -z "$rate_kbit" ]; then
        echo "[netem] skipping $iface (no config)"
        return
    fi

    # Remove existing qdisc (ignore error if none)
    tc qdisc del dev "$iface" root 2>/dev/null || true

    # Compute queue limit: ~100ms of buffering at the link rate.
    # rate_kbit * 1000 / 8 = bytes/sec; * 0.1 = 100ms; / 1200 = packets.
    # Floor at 10 to allow burst absorption.
    local limit=$(( (rate_kbit * 1000 / 8 / 1200 / 10) ))
    [ "$limit" -lt 10 ] && limit=10

    echo "[netem] $iface: rate=${rate_kbit}kbit delay=${delay_ms}ms ±${jitter_ms}ms loss=${loss_pct}% corr=${loss_corr}% limit=${limit}pkts"

    # HTB root shaper — token-bucket rate limit
    tc qdisc add dev "$iface" root handle 1: htb default 10
    tc class add dev "$iface" parent 1: classid 1:10 htb \
        rate "${rate_kbit}kbit" ceil "${rate_kbit}kbit" burst 32k

    # Netem child — delay/jitter/loss/corruption
    tc qdisc add dev "$iface" parent 1:10 handle 10: netem \
        delay "${delay_ms}ms" "${jitter_ms}ms" distribution normal \
        loss "${loss_pct}%" "${loss_corr}%" \
        corrupt 0.05% \
        limit "$limit"
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
