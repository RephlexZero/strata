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