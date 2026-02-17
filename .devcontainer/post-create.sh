#!/usr/bin/env bash
set -euo pipefail

echo "── Strata post-create setup ──────────────────────────────"

# Initialise submodules if not already present
git submodule update --init --recursive 2>/dev/null || true

# Pre-build so rust-analyzer has everything it needs
echo "Running initial cargo check…"
cargo check --workspace 2>&1 | tail -5

# Install cargo-release for automated versioning/tagging
if ! cargo release -V >/dev/null 2>&1; then
    echo "Installing cargo-release…"
    cargo install cargo-release
fi

# Install trunk for WASM dashboard builds
if ! trunk -V >/dev/null 2>&1; then
    echo "Installing trunk…"
    cargo install trunk
fi

# Add wasm32 target for dashboard
if ! rustup target list --installed | grep -q wasm32-unknown-unknown; then
    echo "Adding wasm32-unknown-unknown target…"
    rustup target add wasm32-unknown-unknown
fi

# Enable repo git hooks (pre-commit/pre-push)
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit .githooks/pre-push 2>/dev/null || true

# Set GST_PLUGIN_PATH for the current user so debug builds are auto-registered
REPO_ROOT="$(git rev-parse --show-toplevel)"
PROFILE_LINE="export GST_PLUGIN_PATH=\"${REPO_ROOT}/target/debug:${GST_PLUGIN_PATH:-}\""
if ! grep -qF 'GST_PLUGIN_PATH' "$HOME/.bashrc" 2>/dev/null; then
    echo "$PROFILE_LINE" >> "$HOME/.bashrc"
fi

echo "── Done. Run 'cargo build' to get started. ──────────────"
