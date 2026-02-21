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
fi