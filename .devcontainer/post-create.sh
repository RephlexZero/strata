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

# ── Cargo tools (cargo-binstall is baked into the image) ────────────
cargo binstall -y --no-confirm cargo-release trunk 2>/dev/null \
    || { cargo install --locked cargo-release; cargo install --locked trunk; }

echo "✓ Rust: $(rustc --version)"

# ── Git setup ───────────────────────────────────────────────────────
git submodule update --init --recursive 2>/dev/null || true
git config core.hooksPath .githooks
chmod +x .githooks/* 2>/dev/null || true

# ── Background cargo check (feeds rust-analyzer) ───────────────────
echo "Running cargo check (background)…"
nohup cargo check --workspace >/tmp/cargo-check.log 2>&1 &

echo "── Setup complete ──"
