#!/usr/bin/env bash
# Strata role installer — sender | receiver | control
#
# Usage:
#   sudo ./install.sh <role> [--dist DIR]
#
# Installs the role's binary (plus strata-pipeline and the libgststrata.so
# GStreamer plugin for sender/receiver), the systemd unit, and an example
# env file at /etc/strata/<role>.env (never overwrites an existing one).
#
# Binaries are looked up under --dist DIR (default: the directory holding
# this script, then its parent) by their plain names:
#   strata-sender | strata-receiver | strata-control
#   strata-pipeline, libgststrata.so        (sender/receiver only)
#
# NOTE: strata-pipeline is intentionally NOT setcap'd. The sender unit grants
# CAP_NET_RAW via AmbientCapabilities (inherited by the child), and its
# NoNewPrivileges=true would block file-capability elevation anyway.
set -euo pipefail

usage() {
    echo "Usage: sudo $0 <sender|receiver|control> [--dist DIR]" >&2
}

ROLE=""
DIST_DIR=""
while [ $# -gt 0 ]; do
    case "$1" in
        sender|receiver|control) ROLE="$1" ;;
        --dist)   DIST_DIR="${2:?--dist needs a directory}"; shift ;;
        --dist=*) DIST_DIR="${1#--dist=}" ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; usage; exit 1 ;;
    esac
    shift
done

[ -n "$ROLE" ] || { usage; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Error: must run as root (sudo)." >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Search order: --dist dir (if given), script dir, repo layout next to script.
find_file() { # $1 = filename → prints absolute path, or fails
    local f="$1" d
    for d in ${DIST_DIR:+"$DIST_DIR"} "$SCRIPT_DIR" "$SCRIPT_DIR/.." \
             "$SCRIPT_DIR/systemd" "$SCRIPT_DIR/env"; do
        if [ -f "$d/$f" ]; then
            echo "$d/$f"
            return 0
        fi
    done
    echo "Error: '$f' not found (searched: ${DIST_DIR:-<no --dist>}, $SCRIPT_DIR)." >&2
    return 1
}

# GStreamer plugin directory: multiarch path if it exists, else /usr/local.
detect_plugin_dir() {
    local triplet=""
    if command -v gcc >/dev/null 2>&1; then
        triplet="$(gcc -dumpmachine 2>/dev/null || true)"
    fi
    if [ -z "$triplet" ]; then
        case "$(uname -m)" in
            aarch64) triplet="aarch64-linux-gnu" ;;
            x86_64)  triplet="x86_64-linux-gnu" ;;
        esac
    fi
    if [ -n "$triplet" ] && [ -d "/usr/lib/$triplet" ]; then
        echo "/usr/lib/$triplet/gstreamer-1.0"
    else
        echo "/usr/local/lib/gstreamer-1.0"
    fi
}

# ── System user ─────────────────────────────────────────────────
if ! id -u strata >/dev/null 2>&1; then
    useradd --system --user-group --home-dir /var/lib/strata \
            --create-home --shell /usr/sbin/nologin strata
    echo "Created system user 'strata'."
else
    echo "System user 'strata' already exists."
fi

# ── Binaries ────────────────────────────────────────────────────
BIN="strata-$ROLE"
install -m 755 "$(find_file "$BIN")" "/usr/local/bin/$BIN"
echo "Installed /usr/local/bin/$BIN"

if [ "$ROLE" = "sender" ] || [ "$ROLE" = "receiver" ]; then
    install -m 755 "$(find_file strata-pipeline)" /usr/local/bin/strata-pipeline
    echo "Installed /usr/local/bin/strata-pipeline (no setcap — unit grants ambient CAP_NET_RAW)"

    PLUGIN_DIR="$(detect_plugin_dir)"
    install -d "$PLUGIN_DIR"
    install -m 644 "$(find_file libgststrata.so)" "$PLUGIN_DIR/libgststrata.so"
    echo "Installed $PLUGIN_DIR/libgststrata.so"
    if [ "$PLUGIN_DIR" = "/usr/local/lib/gstreamer-1.0" ]; then
        echo "  NOTE: non-standard plugin path — set GST_PLUGIN_PATH=$PLUGIN_DIR in /etc/strata/$ROLE.env"
    fi
fi

# ── Systemd unit + env file ─────────────────────────────────────
install -m 644 "$(find_file "strata-$ROLE.service")" "/etc/systemd/system/strata-$ROLE.service"
echo "Installed /etc/systemd/system/strata-$ROLE.service"

install -d -m 755 /etc/strata
if [ -f "/etc/strata/$ROLE.env" ]; then
    echo "Kept existing /etc/strata/$ROLE.env (not overwritten)."
else
    install -m 600 "$(find_file "$ROLE.env.example")" "/etc/strata/$ROLE.env"
    echo "Installed /etc/strata/$ROLE.env from example — EDIT IT before starting."
fi

systemctl daemon-reload

cat <<EOF

Done. To finish:

  1. sudoedit /etc/strata/$ROLE.env
  2. sudo systemctl enable --now strata-$ROLE
  3. journalctl -u strata-$ROLE -f
EOF
