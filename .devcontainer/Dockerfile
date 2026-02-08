FROM mcr.microsoft.com/devcontainers/base:debian-12

# ── System dependencies ──────────────────────────────────────────────
RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get -y install --no-install-recommends \
    # Build toolchain
    build-essential \
    clang \
    cmake \
    pkg-config \
    # GStreamer dev + runtime
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev \
    libfontconfig1-dev \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav \
    gstreamer1.0-tools \
    # librist build deps (meson/ninja)
    meson \
    ninja-build \
    # Network tooling (integration tests use netns + tc)
    iproute2 \
    iputils-ping \
    net-tools \
    tcpdump \
    iptables \
    # Kernel headers for netns tests
    linux-headers-generic \
    # Useful utilities
    git \
    curl \
    jq \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

# Ensure pkg-config can find libraries
ENV PKG_CONFIG_PATH="/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
