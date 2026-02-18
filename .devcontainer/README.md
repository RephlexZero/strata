# Dev Container Configuration

This directory contains the VS Code dev container configuration for Strata development.

## Architecture

The setup uses a **multi-stage Dockerfile** for optimal performance:

### Build Stages

1. **base** - System dependencies (GStreamer, build tools, network utilities)
2. **rust-installer** - Rust toolchain installation via rustup
3. **cargo-tools** - Development tools (cargo-binstall, cargo-release, trunk, wasm32 target)
4. **final** - Clean runtime with all tools pre-installed

### Benefits

- **Fast startup**: All tools pre-installed in image (~2-3 minutes faster than post-create installs)
- **Reproducible**: Everyone gets identical tool versions
- **Offline capable**: No network dependencies during container creation
- **Efficient caching**: Docker layer caching makes rebuilds near-instant
- **Version controlled**: Tool versions pinned in Dockerfile, not dynamically fetched

## Files

- `Dockerfile` - Multi-stage container image definition
- `devcontainer.json` - VS Code configuration, extensions, and capabilities
- `post-create.sh` - Workspace-specific setup (git hooks, submodules, GST_PLUGIN_PATH)

## Post-Create Script

The post-create script runs **after** container creation and handles workspace-specific tasks:

- Git submodule initialization
- Git hooks configuration
- User environment setup (GST_PLUGIN_PATH)
- Background cargo check for rust-analyzer

It does **not** install any software - that's all baked into the image.

## Rebuilding

When you update the Dockerfile:

```bash
# VS Code Command Palette (Ctrl+Shift+P)
> Dev Containers: Rebuild Container
```

Or rebuild without cache to pull latest base images:

```bash
# VS Code Command Palette
> Dev Containers: Rebuild Container Without Cache
```

## Pre-installed Tools

- Rust stable (rustc, cargo, clippy, rustfmt, rust-analyzer)
- cargo-binstall - Fast binary installations
- cargo-release - Version management and release automation
- trunk - WASM web bundler for dashboard builds
- wasm32-unknown-unknown target - WebAssembly compilation

## Customization

To add more cargo tools to the image, edit the `cargo binstall` command in the **Stage 3** of the Dockerfile.

To add system packages, edit the `apt-get install` command in the **Stage 1** of the Dockerfile.
