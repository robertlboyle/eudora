#!/usr/bin/env bash
# Builds the natural voices' program for the Mac (Apple silicon and Intel in one file), signed with
# the Developer ID as Eudora is (hardened runtime, Apple's timestamp), into dist/voice/. It's never
# part of Eudora.app: a download of its own (core/voices.rs). Then, to make the signed list:
#
#   cargo run -p eudora-release -- voices --platform macos --program dist/voice/eudora-voice-macos --models <kokoro folder>
#
# (the Kokoro folder holds kokoro-v1.0.onnx and voices-v1.0.bin, as PlanetPDF's models/kokoro)

set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/dist/voice"
mkdir -p "$out"

echo "Step 1 of 3: Building (Apple silicon and Intel)"
(cd "$root" && cargo build --release -p eudora-voice)
(cd "$root" && cargo build --release --target x86_64-apple-darwin -p eudora-voice)

echo "Step 2 of 3: Joining them into one program"
lipo -create "$root/target/release/eudora-voice" "$root/target/x86_64-apple-darwin/release/eudora-voice" -output "$out/eudora-voice-macos"

echo "Step 3 of 3: Signing"
identity="$(security find-identity -v -p codesigning | sed -n 's/.*\([0-9A-F]\{40\}\) "Developer ID Application: .*/\1/p' | head -1)"
if [ -n "$identity" ]; then
    codesign --force --options runtime --timestamp --identifier net.planet.eudora.voice --sign "$identity" "$out/eudora-voice-macos"
    codesign --verify --strict "$out/eudora-voice-macos"
    echo "Signed with the Developer ID."
else
    codesign --force --sign - "$out/eudora-voice-macos"
    echo "No Developer ID certificate in the keychain: signed for this Mac only."
fi
"$out/eudora-voice-macos" --version
echo "Done: $out/eudora-voice-macos"
