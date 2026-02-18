#!/usr/bin/env bash
set -euo pipefail

echo "── Strata workspace setup (runs once per container create/rebuild) ──"

# Verify toolchain is available (should be pre-installed in image)
if ! cargo --version >/dev/null 2>&1; then
    echo "ERROR: Rust toolchain not found. Image build may have failed."
    exit 1
fi

ensure_cargo_tool() {
    local check_cmd="$1"
    local crate_name="$2"
    if eval "$check_cmd" >/dev/null 2>&1; then
        return
    fi
    echo "Installing $crate_name…"
    if command -v cargo-binstall >/dev/null 2>&1; then
        cargo binstall -y "$crate_name"
    else
        cargo install --locked "$crate_name"
    fi
}

ensure_cargo_tool "cargo release -V" "cargo-release"
ensure_cargo_tool "trunk -V" "trunk"

echo "✓ Rust toolchain: $(rustc --version)"
cargo_release_version="$(cargo release -V 2>/dev/null | awk '{print $2}' || true)"
trunk_version="$(trunk -V 2>/dev/null | awk '{print $2}' || true)"
echo "✓ Cargo tools: cargo-release=${cargo_release_version:-unknown}, trunk=${trunk_version:-unknown}"

# Initialize git submodules
echo "Initializing submodules…"
git submodule update --init --recursive 2>/dev/null || true

# Enable repository git hooks
echo "Configuring git hooks…"
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit .githooks/pre-push 2>/dev/null || true

# Set GST_PLUGIN_PATH for debug builds
REPO_ROOT="$(git rev-parse --show-toplevel)"
PROFILE_LINE="export GST_PLUGIN_PATH=\"${REPO_ROOT}/target/debug:${GST_PLUGIN_PATH:-}\""
if ! grep -qF 'GST_PLUGIN_PATH' "$HOME/.bashrc" 2>/dev/null; then
    echo "Setting GST_PLUGIN_PATH in ~/.bashrc…"
    echo "$PROFILE_LINE" >> "$HOME/.bashrc"
fi

# Pre-build for rust-analyzer (done in background to not block container startup)
echo "Running initial cargo check (background)…"
nohup bash -c "cargo check --workspace 2>&1 | tail -5" >/tmp/cargo-check.log 2>&1 &

echo "── Setup complete. Run 'cargo build' to start. ──────────"
