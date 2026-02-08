# Strata

**Bonded video transport over RIST for GStreamer, written in Rust.**

[![CI](https://github.com/RephlexZero/strata/actions/workflows/ci.yml/badge.svg)](https://github.com/RephlexZero/strata/actions/workflows/ci.yml)

Strata aggregates bandwidth across multiple unreliable network interfaces — cellular modems, WiFi, Ethernet, satellite — into a single resilient video stream. It ships as a pair of GStreamer elements (`rsristbondsink` / `rsristbondsrc`) backed by an intelligent scheduling core that handles load balancing, adaptive redundancy, congestion control, and seamless failover.

Designed for field deployment on constrained hardware (e.g. Orange Pi 5 Plus with USB cellular modems) where link conditions are unpredictable and every bit of available bandwidth matters.

---

## Install a Release

Pre-built plugin binaries are published for **x86_64** and **aarch64** Linux on the [Releases](https://github.com/RephlexZero/strata/releases) page.

```bash
# Download the latest release for your architecture (example: v0.1.2)
VERSION="v0.1.2"
ARCH="$(uname -m)"
curl -LO "https://github.com/RephlexZero/strata/releases/download/${VERSION}/strata-${VERSION}-${ARCH}-linux-gnu.so"

# Install the plugin
sudo cp strata-*-linux-gnu.so /usr/lib/${ARCH}-linux-gnu/gstreamer-1.0/libgstristbonding.so

# Verify
gst-inspect-1.0 rsristbondsink
```

### Runtime Dependencies (Target Device)

The plugin requires GStreamer 1.x at runtime. On Debian/Ubuntu (including Armbian on Orange Pi):

```bash
sudo apt-get install -y \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly \
  gstreamer1.0-libav \
  gstreamer1.0-tools
```

No other dependencies are needed — librist is statically linked into the plugin.

---

## Quick Start

**Sender** — stream over two bonded links:

```bash
gst-launch-1.0 \
  videotestsrc is-live=true ! \
  video/x-raw,width=1920,height=1080,framerate=30/1 ! \
  x264enc tune=zerolatency bitrate=3000 ! \
  mpegtsmux ! \
  rsristbondsink name=sink \
    sink.link_0::uri="rist://192.168.1.100:5000" \
    sink.link_1::uri="rist://10.0.0.100:5000"
```

**Receiver** — listen and display:

```bash
gst-launch-1.0 \
  rsristbondsrc links="rist://@0.0.0.0:5000" latency=100 ! \
  tsdemux ! h264parse ! avdec_h264 ! autovideosink
```

See the [Getting Started](https://github.com/RephlexZero/strata/wiki/Getting-Started) guide for TOML config examples, production deployments, and livestreaming relay pipelines.

---

## Development

### Recommended: Dev Container (zero local setup)

The fastest way to get a working build environment is to open this repo in a [Dev Container](https://containers.dev/). Everything — Rust, GStreamer, Meson, librist, clang — is pre-installed and ready to go.

**VS Code / GitHub Codespaces:**
1. Install the [Dev Containers](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-containers) extension (or open in Codespaces)
2. Open this repo → VS Code will prompt "Reopen in Container" → click it
3. Wait for the container to build (~2 min first time)
4. `cargo build` — you're done

> The dev container runs in **privileged mode** with `NET_ADMIN` capabilities so that integration tests can create network namespaces and apply `tc-netem` impairments. This is safe inside a container and required for the full test suite.

### Manual Setup

If you prefer not to use the dev container, see the [Getting Started](https://github.com/RephlexZero/strata/wiki/Getting-Started) wiki page for the full list of system dependencies.

### Building

```bash
cargo build                                # Debug build
cargo build --release                      # Release build (LTO + single codegen unit)
cargo build --release -p gst-rist-bonding  # Plugin only
```

The GStreamer plugin is produced at `target/release/libgstristbonding.so`.

```bash
export GST_PLUGIN_PATH="$PWD/target/release:$GST_PLUGIN_PATH"
gst-inspect-1.0 rsristbondsink
```

### Testing

```bash
cargo test --workspace --lib                                   # Unit tests (no privileges needed)
sudo cargo test -p gst-rist-bonding --test end_to_end          # Integration (needs NET_ADMIN)
```

See the [Testing](https://github.com/RephlexZero/strata/wiki/Testing) wiki page for the full test matrix.

### Cross-compiling for aarch64 (Orange Pi 5 Plus)

A multi-stage Dockerfile handles cross-compilation — no cross toolchain setup needed:

```bash
docker build -f docker/Dockerfile.cross-aarch64 -o dist/ .
# Output: dist/libgstristbonding.so (aarch64)
```

---

## Documentation

Full documentation is in the **[Wiki](https://github.com/RephlexZero/strata/wiki)**.

| Page | Description |
|---|---|
| [Architecture](https://github.com/RephlexZero/strata/wiki/Architecture) | System design, component overview, key decisions |
| [Getting Started](https://github.com/RephlexZero/strata/wiki/Getting-Started) | Prerequisites, building, quick start |
| [Configuration Reference](https://github.com/RephlexZero/strata/wiki/Configuration-Reference) | Full TOML config for links, scheduler, receiver, lifecycle |
| [GStreamer Elements](https://github.com/RephlexZero/strata/wiki/GStreamer-Elements) | `rsristbondsink` and `rsristbondsrc` property + pad reference |
| [Integration Node](https://github.com/RephlexZero/strata/wiki/Integration-Node) | CLI binary for sender/receiver without pipeline code |
| [Telemetry](https://github.com/RephlexZero/strata/wiki/Telemetry) | Stats message schema and JSON relay |
| [Testing](https://github.com/RephlexZero/strata/wiki/Testing) | Test suites, how to run, privilege requirements |
| [Deployment](https://github.com/RephlexZero/strata/wiki/Deployment) | Privileges, performance budgets, operational guidance |

---

## Project Structure

```
crates/
  gst-rist-bonding/     GStreamer plugin + integration_node binary
  rist-bonding-core/     Core bonding logic (no GStreamer dependency)
  librist-sys/           FFI bindings to librist (built from source via Meson)
  rist-network-sim/      Linux netns + tc-netem test infrastructure
vendor/
  librist/               librist source (git submodule, statically linked)
docker/
  Dockerfile.cross-aarch64   Cross-compile for Orange Pi / aarch64
.devcontainer/           Dev Container config (recommended for development)
.github/workflows/       CI and release automation
```

---

## Releasing

Releases are automated via GitHub Actions. One command does everything:

```bash
# Patch release (0.1.1 → 0.1.2) — bumps version, commits, tags, pushes
cargo release -p gst-rist-bonding patch --execute

# With release notes
cargo release -p gst-rist-bonding patch --execute \
  --tag-message "Fix reconnection timeout under high packet loss"
```

This triggers the [Release workflow](.github/workflows/release.yml) which:
1. Verifies the tag matches the crate version
2. Builds the plugin for x86_64 (native) and aarch64 (cross-compiled via Docker)
3. Creates a GitHub Release with pre-built `.so` assets for both architectures

See the [wiki](https://github.com/RephlexZero/strata/wiki/Getting-Started#releasing) for the full release guide.

---

## License

This project is licensed under the [LGPL-2.1-or-later](LICENSE). See [vendor/librist/COPYING](vendor/librist/COPYING) for librist's license terms (BSD-2-Clause).
