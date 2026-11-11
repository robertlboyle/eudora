#!/usr/bin/env bash
# Sets up a Linux computer to build Eudora (BUILDING.md): the system's packages Tauri needs
# (WebKitGTK 4.1, GTK 3, the tray-icon library) and Rust. Debian / Ubuntu, Fedora, and Arch
# (others: it says what to install); uses sudo for the system's packages.
#
#   bash scripts/setup-linux.sh          # asks before installing
#   bash scripts/setup-linux.sh --yes    # doesn't ask
#
# (Release packages, the AppImage, .deb, and .rpm, are built in Docker by scripts/build_linux.py,
# which needs only Docker.)
set -euo pipefail

yes=0
[ "${1:-}" = "--yes" ] && yes=1
confirm() {
    [ $yes = 1 ] && return 0
    read -r -p "Install $1? [Y/n] " answer
    [ -z "$answer" ] || [[ "$answer" =~ ^[Yy] ]]
}

echo "Step 1 of 2: the system's packages"
if command -v apt-get >/dev/null 2>&1; then
    packages="build-essential curl file git pkg-config libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev libxdo-dev libdbus-1-dev"
    if confirm "$packages (apt)"; then sudo apt-get update && sudo apt-get install -y $packages; fi
elif command -v dnf >/dev/null 2>&1; then
    packages="gcc gcc-c++ make curl file git pkgconf-pkg-config webkit2gtk4.1-devel gtk3-devel libappindicator-gtk3-devel librsvg2-devel openssl-devel libxdo-devel dbus-devel"
    if confirm "$packages (dnf)"; then sudo dnf install -y $packages; fi
elif command -v pacman >/dev/null 2>&1; then
    packages="base-devel curl file git pkgconf webkit2gtk-4.1 gtk3 libayatana-appindicator librsvg openssl xdotool dbus"
    if confirm "$packages (pacman)"; then sudo pacman -S --needed --noconfirm $packages; fi
else
    echo "  This system's package manager isn't one this script knows: install a C compiler, pkg-config,"
    echo "  and the development files of WebKitGTK 4.1, GTK 3, ayatana-appindicator, librsvg, OpenSSL, xdo, and D-Bus."
fi

echo "Step 2 of 2: Rust"
if command -v rustup >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/rustup" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
    echo "  Already there; bringing stable Rust up to date"
    rustup update stable
elif confirm "Rust (rustup, from rustup.rs)"; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    export PATH="$HOME/.cargo/bin:$PATH"
fi
rustup default stable
echo "  $(rustc --version)"

echo
echo "Done. Then:"
echo "  cargo build --release -p eudora-app -p eudora-cli"
