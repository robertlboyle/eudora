#!/bin/bash
# Makes Eudora.app from the release build and signs it: one place for scripts/install-dev.sh
# (this Mac's copy) and scripts/build-mac.sh (the .dmg for others), so every Mac build is signed
# the same way.
#
#   bash scripts/mac_app.sh DEST.app [--release]
#
# Signed with the keychain's "Developer ID Application" certificate (Planet Networks, Inc.,
# TEAMID1234; as PlanetPDF, by its SHA-1 fingerprint, since a name found in two keychains is
# ambiguous to codesign), hardened runtime, packaging/macos/entitlements.plist; --release adds
# Apple's secure timestamp (needed for notarizing). With no Developer ID here: signed ad hoc, and
# says so. A Developer ID signature keeps the same identity from build to build, so the Keychain
# doesn't ask again for saved sign-ins after each rebuild.
set -euo pipefail
dest="${1:?usage: mac_app.sh DEST.app [--release]}"
release=0
[ "${2:-}" = "--release" ] && release=1

root="$(cd "$(dirname "$0")/.." && pwd)"
# (the programs from the native build, or EUDORA_MAC_BUILD: build-mac.sh's universal ones)
build="${EUDORA_MAC_BUILD:-$root/target/release}"
icons="$root/crates/eudora-app/icons"
entitlements="$root/packaging/macos/entitlements.plist"
version="$(sed -n 's/^ *"version": *"\(.*\)",*$/\1/p' "$root/crates/eudora-app/tauri.conf.json" | head -1)"

for name in eudora eudora-cli; do
    [ -x "$build/$name" ] || { echo "$build/$name isn't built yet (cargo build --release -p eudora-app -p eudora-cli)" >&2; exit 1; }
done

rm -rf "$dest"
mkdir -p "$dest/Contents/MacOS" "$dest/Contents/Resources"
cp "$build/eudora" "$build/eudora-cli" "$dest/Contents/MacOS/"

# the icon: the 1024-pixel PNG made by scripts/make_icons.py, in every size macOS wants
work="$(mktemp -d)"
set_dir="$work/Eudora.iconset"
mkdir -p "$set_dir"
for size in 16 32 128 256 512; do
    sips -z $size $size "$icons/icon.png" --out "$set_dir/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) "$icons/icon.png" --out "$set_dir/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$set_dir" -o "$dest/Contents/Resources/Eudora.icns"
rm -rf "$work"

cat > "$dest/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Eudora</string>
    <key>CFBundleDisplayName</key><string>Eudora</string>
    <key>CFBundleIdentifier</key><string>net.planet.eudora</string>
    <key>CFBundleExecutable</key><string>eudora</string>
    <key>CFBundleIconFile</key><string>Eudora</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSHumanReadableCopyright</key><string>Copyright Planet Networks. MIT or Apache-2.0.</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key><string>Email address</string>
            <key>CFBundleURLSchemes</key><array><string>mailto</string></array>
        </dict>
    </array>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>Email message</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key><array><string>com.apple.mail.email</string><string>public.email-message</string></array>
            <key>CFBundleTypeExtensions</key><array><string>eml</string></array>
        </dict>
        <dict>
            <key>CFBundleTypeName</key><string>Outlook message</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>CFBundleTypeExtensions</key><array><string>msg</string></array>
        </dict>
        <dict>
            <key>CFBundleTypeName</key><string>Calendar file</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key><array><string>com.apple.ical.ics</string><string>public.calendar-event</string></array>
            <key>CFBundleTypeExtensions</key><array><string>ics</string></array>
        </dict>
    </array>
</dict>
</plist>
EOF

identity="$(security find-identity -v -p codesigning | sed -n 's/.*\([0-9A-F]\{40\}\) "Developer ID Application: .*/\1/p' | head -1)"
if [ -n "$identity" ]; then
    stamp="--timestamp=none"
    [ $release = 1 ] && stamp="--timestamp"
    sign=(codesign --force --sign "$identity" --options runtime "$stamp" --entitlements "$entitlements")
    # the inside first: the command-line program (the background download), then the app. Both
    # under the app's one identity: the Keychain lets a saved password be read without asking
    # only by the identity that saved it, and the window and its download share every one
    # (signed apart, the download asked about every minute: 2026-10-27)
    "${sign[@]}" --identifier net.planet.eudora "$dest/Contents/MacOS/eudora-cli"
    "${sign[@]}" "$dest"
    codesign --verify --deep --strict "$dest"
    echo "Signed: $(security find-identity -v -p codesigning | sed -n "s/.*$identity \"\(.*\)\"/\1/p")"
else
    codesign --force --sign - "$dest/Contents/MacOS/eudora-cli"
    codesign --force --sign - "$dest"
    echo "No Developer ID certificate in the keychain: signed ad hoc (runs on this Mac only)." >&2
fi
