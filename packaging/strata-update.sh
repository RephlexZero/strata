#!/usr/bin/env bash
# Strata pull-based updater — sender | receiver | control
#
# Fetches the newest GitHub release for this machine's architecture,
# verifies checksums (when the release ships SHA256SUMS), atomically swaps
# the role's binaries into place, and restarts the systemd unit.
#
# Usage:
#   sudo ./strata-update.sh <role> [--version vX.Y.Z] [--repo owner/repo] [--force]
#
#   --version   install a specific tag instead of the latest release
#   --repo      override the GitHub repo (default: RephlexZero/strata)
#   --force     update even while a stream is live (the restart WILL drop it)
#
# Unattended updates: enable the opt-in timer after installing this script —
#   sudo cp strata-update.{timer,service} /etc/systemd/system/  (from packaging/systemd/)
#   sudo systemctl enable --now strata-update.timer
# The live-stream guard below makes a timer-driven run a no-op while
# streaming; it retries at the next tick.
set -euo pipefail

REPO="RephlexZero/strata"
ROLE=""
VERSION=""
FORCE=0

while [ $# -gt 0 ]; do
    case "$1" in
        sender|receiver|control) ROLE="$1" ;;
        --version)   VERSION="${2:?--version needs a tag}"; shift ;;
        --version=*) VERSION="${1#--version=}" ;;
        --repo)      REPO="${2:?--repo needs owner/repo}"; shift ;;
        --repo=*)    REPO="${1#--repo=}" ;;
        --force) FORCE=1 ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
    shift
done

[ -n "$ROLE" ] || { echo "Usage: sudo $0 <sender|receiver|control> [--version vX.Y.Z] [--force]" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Error: must run as root (sudo)." >&2; exit 1; }

case "$(uname -m)" in
    aarch64) ARCH="aarch64" ;;
    x86_64)  ARCH="x86_64" ;;
    *) echo "Error: unsupported architecture $(uname -m)." >&2; exit 1 ;;
esac

STAMP="/var/lib/strata/strata-${ROLE}.version"
API="https://api.github.com/repos/${REPO}/releases"

# ── Resolve target version ───────────────────────────────────────────
if [ -z "$VERSION" ]; then
    VERSION=$(curl -fsSL "${API}/latest" | python3 -c 'import sys,json; print(json.load(sys.stdin)["tag_name"])') \
        || { echo "Error: could not resolve the latest release from ${REPO}." >&2; exit 1; }
fi

CURRENT="none"
[ -f "$STAMP" ] && CURRENT="$(cat "$STAMP")"
if [ "$CURRENT" = "$VERSION" ]; then
    echo "Already on ${VERSION} — nothing to do."
    exit 0
fi
echo "Updating strata-${ROLE}: ${CURRENT} → ${VERSION} (${ARCH})"

# ── Live-stream guard ────────────────────────────────────────────────
# A restart kills any pipeline this box is carrying. Unattended runs skip
# and retry later; a human can decide with --force.
if [ "$FORCE" -ne 1 ] && pgrep -x strata-pipeline >/dev/null 2>&1; then
    echo "A stream is live on this box (strata-pipeline running) — refusing to update." >&2
    echo "Re-run with --force to update anyway (this WILL drop the stream)." >&2
    exit 75  # EX_TEMPFAIL — timer-driven runs treat this as "try again later"
fi

# ── Download ─────────────────────────────────────────────────────────
TMP=$(mktemp -d /tmp/strata-update-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
BASE="https://github.com/${REPO}/releases/download/${VERSION}"

fetch() { # $1 = asset name
    echo "  fetching $1"
    curl -fsSL -o "${TMP}/$1" "${BASE}/$1" || { echo "Error: failed to download $1" >&2; exit 1; }
}

ASSETS=("strata-${ROLE}-${VERSION}-${ARCH}-linux-gnu")
if [ "$ROLE" != "control" ]; then
    ASSETS+=("strata-pipeline-${VERSION}-${ARCH}-linux-gnu" "strata-${VERSION}-${ARCH}-linux-gnu.so")
fi
for a in "${ASSETS[@]}"; do fetch "$a"; done

# Verify checksums when the release publishes them (releases before the
# SHA256SUMS step predate verification — warn, don't fail).
if curl -fsSL -o "${TMP}/SHA256SUMS" "${BASE}/SHA256SUMS" 2>/dev/null; then
    (cd "$TMP" && grep -F "$(printf '%s\n' "${ASSETS[@]}")" SHA256SUMS | sha256sum -c --quiet -) \
        || { echo "Error: checksum verification failed." >&2; exit 1; }
    echo "  checksums OK"
else
    echo "  WARNING: release has no SHA256SUMS — skipping verification."
fi

# ── Install (atomic per file: rename within /usr/local/bin) ──────────
install_bin() { # $1 = asset, $2 = destination path
    install -m 755 "${TMP}/$1" "$2.new"
    mv -f "$2.new" "$2"
    echo "  installed $2"
}

install_bin "strata-${ROLE}-${VERSION}-${ARCH}-linux-gnu" "/usr/local/bin/strata-${ROLE}"
if [ "$ROLE" != "control" ]; then
    install_bin "strata-pipeline-${VERSION}-${ARCH}-linux-gnu" "/usr/local/bin/strata-pipeline"
    # Same plugin-dir logic as install.sh: multiarch dir if present.
    triplet="$(gcc -dumpmachine 2>/dev/null || echo "${ARCH}-linux-gnu")"
    plugin_dir="/usr/lib/${triplet}/gstreamer-1.0"
    [ -d "$plugin_dir" ] || plugin_dir="/usr/local/lib/gstreamer-1.0"
    mkdir -p "$plugin_dir"
    install -m 644 "${TMP}/strata-${VERSION}-${ARCH}-linux-gnu.so" "${plugin_dir}/libgststrata.so.new"
    mv -f "${plugin_dir}/libgststrata.so.new" "${plugin_dir}/libgststrata.so"
    echo "  installed ${plugin_dir}/libgststrata.so"
fi

mkdir -p /var/lib/strata
echo "$VERSION" > "$STAMP"

if systemctl is-enabled --quiet "strata-${ROLE}" 2>/dev/null; then
    systemctl restart "strata-${ROLE}"
    echo "Restarted strata-${ROLE}."
fi
echo "strata-${ROLE} updated to ${VERSION}."
