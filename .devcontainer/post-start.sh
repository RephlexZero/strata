#!/usr/bin/env bash
set -euo pipefail

# Load kernel modules needed for network-namespace simulation tests.
# The docker-in-docker feature handles its own startup; we only need the
# extra netfilter / overlay modules for strata-sim's tc/netem tests.
if command -v modprobe >/dev/null 2>&1; then
    sudo modprobe iptable_nat  2>/dev/null || true
    sudo modprobe nf_nat       2>/dev/null || true
    sudo modprobe br_netfilter 2>/dev/null || true
    sudo modprobe overlay      2>/dev/null || true
    sudo modprobe sch_netem    2>/dev/null || true
    sudo modprobe sch_tbf      2>/dev/null || true
fi

# ── Device group membership ─────────────────────────────────────────
# The /dev bind mount exposes host device nodes; ensure the container user
# is in the groups that own them so reads/writes don't require sudo.
#
# video (typically GID 44)  — /dev/video* V4L2 capture devices
# dialout (typically GID 20) — /dev/ttyUSB* USB serial / AT-command modems
# plugdev (typically GID 46) — udev-managed USB devices (LTE dongles)
#
# Group IDs on the host may differ; we add by name and suppress errors for
# groups that don't exist inside the container image.
GROUPS_WANTED=(video dialout plugdev)
for g in "${GROUPS_WANTED[@]}"; do
    if getent group "$g" >/dev/null 2>&1; then
        sudo usermod -aG "$g" "$(whoami)" 2>/dev/null || true
    fi
done

# ── De-prioritize cellular/USB links for general traffic ─────────────
# With --network=host the container shares the host routing table.
# DHCP on LTE dongles installs low-metric default routes that can steal
# traffic from apt-get, cargo builds, Docker pulls, etc.
# We raise those metrics so high (20000) that the primary interface
# (wlan0 / eno1) always wins, while SO_BINDTODEVICE in strata-pipeline
# still routes each link's packets correctly.
#
# Interface detection order:
#   1. STRATA_LINK_IFACES from .env (explicit list, most reliable)
#   2. Auto-detect USB ethernet by predictable name suffix 'u' (enp*u*)
CELLULAR_METRIC=20000
mapfile -t _CELLULAR_IFACES < <(
    {
        if [[ -f /workspaces/strata/.env ]]; then
            grep -oP '(?<=STRATA_LINK_IFACES=)[^\s#"]+' /workspaces/strata/.env \
                | tr ',' '\n'
        fi
        find /sys/class/net -maxdepth 1 -name 'enp*u*' -printf '%f\n' 2>/dev/null
    } | sort -u | grep -v '^$'
)
for _iface in "${_CELLULAR_IFACES[@]}"; do
    _current=$(ip route show default dev "$_iface" 2>/dev/null \
                | grep -oP 'metric \K[0-9]+' | head -1)
    [[ -z "$_current" || "$_current" -ge "$CELLULAR_METRIC" ]] && continue
    _via=$(ip route show default dev "$_iface" 2>/dev/null \
            | grep -oP 'via \K\S+' | head -1)
    [[ -z "$_via" ]] && continue
    sudo ip route del default via "$_via" dev "$_iface" metric "$_current" 2>/dev/null || true
    sudo ip route add default via "$_via" dev "$_iface" \
        metric "$CELLULAR_METRIC" 2>/dev/null || true
    echo "[devcontainer] Raised default route metric: $_iface via $_via  $_current → $CELLULAR_METRIC"
done
unset _CELLULAR_IFACES _iface _current _via CELLULAR_METRIC