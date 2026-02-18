#!/usr/bin/env bash
set -euo pipefail

if ! command -v docker >/dev/null 2>&1; then
    exit 0
fi

if docker info >/dev/null 2>&1; then
    exit 0
fi

if command -v modprobe >/dev/null 2>&1; then
    sudo modprobe iptable_nat || true
    sudo modprobe nf_nat || true
    sudo modprobe br_netfilter || true
    sudo modprobe overlay || true
fi

if [ -x /usr/local/share/docker-init.sh ]; then
    sudo /usr/local/share/docker-init.sh >/tmp/docker-init.log 2>&1 || true
fi