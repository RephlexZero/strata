#!/bin/sh
# ── Strata Sender Entrypoint ───────────────────────────────────────
#
# Applies tc netem impairments to each network interface before
# starting the strata-agent daemon.  This simulates realistic
# cellular uplink conditions across bonded links.
#
# Each LINKn_IMPAIRMENT env var has the format:
#   "RATE_KBIT DELAY_MS JITTER_MS LOSS_PERCENT LOSS_CORR_PERCENT"
#   (optional: SLOT_MIN_US SLOT_MAX_US MODEM_BUF_KB)
#
# Profiles modelled from production-grade cellular measurements:
#   link0: 8000 kbit, 22ms ±8ms,  0.5% loss (25% corr) — LTE urban
#   link1: 5000 kbit, 30ms ±15ms, 2.0% loss (30% corr) — LTE poor
#   link2: 6000 kbit, 18ms ±5ms,  0.3% loss (15% corr) — LTE good
#
# The aggregate is ~19 Mbps — representative of 3 bonded LTE uplinks
# with dedicated SIM cards on commodity USB modems.
#
# TC stack applied per interface:
#   tbf root (handle 1:) — token-bucket rate + modem firmware buffer (~64 KiB)
#     └─ netem child (parent 1:1 handle 10:) — delay/jitter/loss/slot
#
# The `slot` directive models the LTE TTI (1 ms radio scheduling window),
# reproducing the burst-then-silence packet rhythm of real cellular modems.
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
    # LTE TTI slot boundary range in microseconds (override via $7/$8)
    local slot_min_us="${7:-1000}"
    local slot_max_us="${8:-2000}"
    # Modem firmware buffer size in KiB (tbf burst, override via $9)
    local modem_buf_kb="${9:-64}"

    if [ -z "$rate_kbit" ]; then
        echo "[netem] skipping $iface (no config)"
        return
    fi

    # Remove existing qdisc (ignore error if none)
    tc qdisc del dev "$iface" root 2>/dev/null || true

    # Compute netem queue limit: target 500 ms of buffering at the link rate.
    # rate_kbit * 1000 / 8 * 0.5 / 1200 (bytes/pkt) = packets.
    local limit=$(( rate_kbit * 500 / 8 / 1200 ))
    [ "$limit" -lt 10 ] && limit=10

    local burst_bytes=$(( modem_buf_kb * 1024 ))

    echo "[netem] $iface: tbf rate=${rate_kbit}kbit burst=${modem_buf_kb}KiB | netem delay=${delay_ms}ms±${jitter_ms}ms loss=${loss_pct}%(${loss_corr}%) slot=${slot_min_us}us-${slot_max_us}us limit=${limit}pkts"

    # Layer 1: tbf root — token-bucket rate shaper + modem firmware buffer model
    tc qdisc add dev "$iface" root handle 1: tbf \
        rate "${rate_kbit}kbit" burst "$burst_bytes" latency 300ms

    # Layer 2: netem child of tbf — delay/jitter/loss/slot scheduling
    # No `rate` here: tbf already owns rate limiting above.
    # `slot` models LTE TTI burst-then-silence radio scheduling.
    tc qdisc add dev "$iface" parent 1:1 handle 10: netem \
        delay "${delay_ms}ms" "${jitter_ms}ms" distribution normal \
        loss "${loss_pct}%" "${loss_corr}%" \
        corrupt 0.05% \
        slot "${slot_min_us}us" "${slot_max_us}us" packets 12 bytes 14400 \
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
