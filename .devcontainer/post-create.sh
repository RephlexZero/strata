#!/usr/bin/env bash
set -euo pipefail

echo "── Strata workspace setup ──"

# ── Verify core toolchains ──────────────────────────────────────────
if ! cargo --version >/dev/null 2>&1; then
    echo "ERROR: Rust toolchain not found."
    exit 1
fi

gst_version="$(pkg-config --modversion gstreamer-1.0 2>/dev/null || true)"
if [ -z "$gst_version" ]; then
    echo "WARNING: GStreamer pkg-config not found."
elif ! gst-inspect-1.0 --version >/dev/null 2>&1; then
    echo "WARNING: GStreamer $gst_version headers present but runtime broken (GLIBC mismatch?)."
else
    echo "✓ GStreamer: $gst_version"
fi

# ── Cargo tools ─────────────────────────────────────────────────────
# Install cargo-binstall first, then use it to grab tools as pre-built
# binaries (~5s total vs ~5min compiling from source).
if ! command -v cargo-binstall >/dev/null 2>&1; then
    curl -fsSL "https://github.com/cargo-bins/cargo-binstall/releases/latest/download/cargo-binstall-x86_64-unknown-linux-musl.tgz" \
        | tar -xz -C "${CARGO_HOME:-$HOME/.cargo}/bin" cargo-binstall
fi
cargo binstall -y cargo-release trunk

echo "✓ Rust: $(rustc --version)"

# ── Git setup ───────────────────────────────────────────────────────
git submodule update --init --recursive 2>/dev/null || true
git config core.hooksPath .githooks
chmod +x .githooks/* 2>/dev/null || true

# ── Background cargo check (feeds rust-analyzer) ───────────────────
echo "Running cargo check (background)…"
nohup cargo check --workspace >/tmp/cargo-check.log 2>&1 &

echo "── Setup complete ──"
