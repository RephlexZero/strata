# ── Stage 1: Build GStreamer 1.28 from source ────────────────────────
# Debian 12 ships GStreamer 1.22; we build 1.28 from the upstream mono-repo.
# A separate builder stage keeps meson/ninja/nasm/flex/bison out of the
# final image, saving ~400 MB.
#
# IMPORTANT: the builder MUST use the same base distro (and therefore the
# same glibc) as the runtime stage.  Using a newer distro (e.g. Ubuntu
# 25.10 with glibc 2.41) produces .so files that reference GLIBC_2.38+
# symbols and crash at runtime with "GLIBC_2.38 not found".
FROM mcr.microsoft.com/devcontainers/base:debian-12 AS gst-builder

ARG GST_VERSION=1.28.0

# NVIDIA CUDA repository — provides build headers so GStreamer's nvcodec
# plugin compiles.  The actual GPU driver is dlopen'd at runtime from the
# host (via --device=/dev/dri + NVIDIA Container Toolkit).
RUN curl -fsSL https://developer.download.nvidia.com/compute/cuda/repos/debian12/x86_64/cuda-keyring_1.1-1_all.deb \
    -o /tmp/cuda-keyring.deb && dpkg -i /tmp/cuda-keyring.deb && rm /tmp/cuda-keyring.deb

# meson 1.0.x in Debian 12 is too old for GStreamer 1.28 (requires >= 1.4);
# install a current release via pip before building.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl python3 python3-pip \
    build-essential pkg-config \
    ninja-build nasm flex bison \
    libglib2.0-dev libfreetype-dev \
    libx264-dev libx265-dev \
    libvorbis-dev libopus-dev libogg-dev \
    libpng-dev libjpeg-dev \
    libavcodec-dev libavformat-dev libavutil-dev \
    libswresample-dev libswscale-dev \
    # VA-API (Intel + AMD hardware encoding via libva)
    libva-dev libdrm-dev \
    # NVIDIA NVENC/NVDEC build headers (plugin dlopen's the driver at runtime)
    cuda-cudart-dev-12-6 \
    && ln -sf /usr/local/cuda-12.6 /usr/local/cuda \
    && pip3 install --break-system-packages 'meson>=1.4' \
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
    -Dgpl=enabled \
    -Dgst-plugins-ugly:x264=enabled \
    -Dgst-plugins-bad:x265=enabled \
    -Dgst-plugins-bad:va=enabled \
    -Dgst-plugins-bad:nvcodec=enabled \
    -Dgst-editing-services:enabled=disabled \
    -Dgst-devtools:enabled=disabled \
    && ninja -C /tmp/build -j"$(nproc)" \
    && DESTDIR=/gst-install ninja -C /tmp/build install

# ── Stage 2: Dev container ───────────────────────────────────────────
FROM mcr.microsoft.com/devcontainers/base:debian-12

# Runtime + dev libraries needed for Rust compilation against GStreamer.
# GStreamer build-only tools (meson, ninja, nasm, flex, bison) are NOT included.
#
# Enable non-free-firmware for Intel VA-API driver (iHD).
RUN sed -i 's/Components: main$/Components: main non-free non-free-firmware/' \
    /etc/apt/sources.list.d/debian.sources 2>/dev/null || true \
    && apt-get update && export DEBIAN_FRONTEND=noninteractive \
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
    # VA-API runtime + GPU drivers (Intel iHD, AMD Mesa).
    # NVIDIA uses host driver via GPU passthrough — no package needed here.
    libva2 libva-drm2 \
    intel-media-va-driver-non-free mesa-va-drivers vainfo \
    # Network / system tools
    iproute2 iputils-ping net-tools tcpdump kmod \
    && rm -rf /var/lib/apt/lists/*

# Copy GStreamer 1.28 artifacts (libraries, headers, plugins, tools, pkg-config)
# from the builder stage. This replaces Debian 12's GStreamer 1.22 packages.
COPY --from=gst-builder /gst-install/usr/ /usr/
RUN ldconfig

ENV PKG_CONFIG_PATH="/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
