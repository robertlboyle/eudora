#!/usr/bin/env bash
# Sets up a Mac (macOS 11 or newer) to build Eudora (BUILDING.md). Checks what's installed and
# installs only what's missing.
#
#   bash scripts/setup-mac.sh          # the Mac version (Apple silicon and Intel)
#   bash scripts/setup-mac.sh --ios    # also the iOS version (needs Xcode from the App Store)
#   bash scripts/setup-mac.sh --yes    # don't ask before each install
set -euo pipefail

ios=0; yes=0
for arg in "$@"; do
    case "$arg" in
        --ios) ios=1 ;;
        --yes) yes=1 ;;
        *) echo "Unknown option: $arg"; exit 2 ;;
    esac
done
total=3; [ $ios = 1 ] && total=4
step=0
say() { step=$((step + 1)); echo "Step $step of $total: $1"; }
confirm() {
    [ $yes = 1 ] && return 0
    read -r -p "Install $1? [Y/n] " answer
    [ -z "$answer" ] || [[ "$answer" =~ ^[Yy] ]]
}

say "Apple's command-line tools (compiler, linker, Git)"
if xcode-select -p >/dev/null 2>&1; then
    echo "  Already there: $(xcode-select -p)"
else
    echo "  macOS asks to install them in a window of its own; run this script again when it's done."
    xcode-select --install || true
    exit 0
fi

say "Rust"
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

say "Rust's targets for both kinds of Mac (one app for Apple silicon and Intel)"
rustup target add aarch64-apple-darwin x86_64-apple-darwin

if [ $ios = 1 ]; then
    say "iOS: Rust targets, Tauri's command line, CocoaPods, XcodeGen"
    if [ ! -d /Applications/Xcode.app ]; then
        echo "  Xcode isn't installed: get it from the App Store, then run:"
        echo "    sudo xcode-select -s /Applications/Xcode.app && sudo xcodebuild -license accept"
    fi
    rustup target add aarch64-apple-ios aarch64-apple-ios-sim
    cargo install tauri-cli --version 2.12.4 --locked
    if ! command -v brew >/dev/null 2>&1; then
        echo "  Homebrew isn't installed (https://brew.sh): install it, then: brew install cocoapods xcodegen"
    elif confirm "CocoaPods and XcodeGen (Homebrew)"; then
        brew install cocoapods xcodegen
    fi
fi

echo
echo "Done. Then:"
echo "  cargo build --release -p eudora-app -p eudora-cli"
