FROM mcr.microsoft.com/devcontainers/base:debian-12

RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get -y install --no-install-recommends \
    ca-certificates \
    curl \
    git \
    build-essential \
    clang \
    cmake \
    pkg-config \
    libssl-dev \
    nodejs \
    npm \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav \
    gstreamer1.0-tools \
    meson \
    ninja-build \
    iproute2 \
    iputils-ping \
    net-tools \
    tcpdump \
    kmod \
    linux-headers-amd64 \
    && rm -rf /var/lib/apt/lists/*

# Ensure pkg-config can find libraries
ENV PKG_CONFIG_PATH="/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
