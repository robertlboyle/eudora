# The Linux build computer for Eudora (2026-11-06), for x86-64 or ARM64, as PlanetPDF's:
#
#     python3 scripts/build_linux.py --arch arm64     # from the Mac (Colima) or any Docker host
#     python3 scripts/build_linux.py --arch both
#
# Ubuntu 24.04 (glibc 2.39: the packages run on Ubuntu 24.04, Debian 13, Fedora 40, and newer),
# Rust (the version the project builds with), WebKitGTK 4.1 and GTK 3 for Tauri's window, Tauri's
# command line (for the AppImage, which carries WebKitGTK with it), and the tools that make the
# .deb, the .rpm, and the apt / dnf repositories.
FROM ubuntu:24.04

ARG RUST_VERSION=1.99.0
ARG TAURI_CLI_VERSION=2.12.4

ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl wget file xz-utils build-essential pkg-config patchelf git \
        libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev \
        libssl-dev libxdo-dev libdbus-1-dev \
        python3 squashfs-tools rpm dpkg-dev apt-utils createrepo-c gnupg desktop-file-utils \
        xvfb xauth \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain ${RUST_VERSION} --no-modify-path \
    && cargo install tauri-cli --version ${TAURI_CLI_VERSION} --locked \
    && rm -rf /opt/cargo/registry /opt/cargo/git \
    && chmod -R a+rwX /opt/cargo /opt/rustup

# (the AppImage tools Tauri fetches run without FUSE inside a container)
ENV APPIMAGE_EXTRACT_AND_RUN=1
