# ── Stage 1: Build GStreamer 1.28 from source ────────────────────────
# Debian 12 ships GStreamer 1.22 and Meson 1.3; we need Meson ≥ 1.4 for
# GStreamer 1.28, so we install a newer version via pip.
# A separate builder stage keeps build-only tools out of the final image.
FROM debian:12-slim AS gst-builder

ARG GST_VERSION=1.28.0

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    build-essential pkg-config \
    python3 python3-pip ninja-build nasm flex bison \
    libglib2.0-dev libfreetype-dev \
    libx264-dev libx265-dev \
    libvorbis-dev libopus-dev libogg-dev \
    libpng-dev libjpeg-dev \
    libavcodec-dev libavformat-dev libavutil-dev \
    libswresample-dev libswscale-dev \
    && pip3 install --break-system-packages meson>=1.4 \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL \
    "https://gitlab.freedesktop.org/gstreamer/gstreamer/-/archive/${GST_VERSION}/gstreamer-${GST_VERSION}.tar.gz" \
    | tar -xz -C /tmp --strip-components=1 \
    && meson setup /tmp/build /tmp \
    --prefix=/usr \
    --buildtype=release \
    -Dintrospection=disabled \
    -Ddoc=disabled \
    -Dtests=disabled \
    -Dexamples=disabled \
    -Dbenchmarks=disabled \
    && ninja -C /tmp/build -j"$(nproc)" \
    && DESTDIR=/gst-install ninja -C /tmp/build install

# ── Stage 2: Dev container ───────────────────────────────────────────
FROM mcr.microsoft.com/devcontainers/base:debian-12

# Runtime + dev libraries needed for Rust compilation against GStreamer.
# GStreamer build-only tools (meson, ninja, nasm, flex, bison) are NOT included.
RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get -y install --no-install-recommends \
    ca-certificates curl git build-essential pkg-config libssl-dev \
    nodejs npm \
    # GLib (required by GStreamer core — both runtime and dev for pkg-config)
    libglib2.0-dev \
    # Codec libraries consumed by GStreamer plugins
    libx264-dev libx265-dev \
    libvorbis-dev libopus-dev libogg-dev \
    libpng-dev libjpeg-dev \
    libavcodec-dev libavformat-dev libavutil-dev \
    libswresample-dev libswscale-dev \
    # Network / system tools
    iproute2 iputils-ping net-tools tcpdump kmod \
    && rm -rf /var/lib/apt/lists/*

# Copy GStreamer 1.28 artifacts (libraries, headers, plugins, tools, pkg-config)
# from the builder stage. This replaces Debian 12's GStreamer 1.22 packages.
COPY --from=gst-builder /gst-install/usr/ /usr/
RUN ldconfig

# cargo-binstall lets post-create.sh install tools as pre-built binaries
# instead of compiling from source (~seconds vs ~minutes).
RUN curl -fsSL https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash

ENV PKG_CONFIG_PATH="/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig" \
    GST_PLUGIN_PATH="/workspaces/strata/target/debug"
