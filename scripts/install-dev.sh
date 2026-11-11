#!/bin/bash
# The Mac's install-dev.ps1: puts the Eudora just built into ~/Applications/Eudora.app (a real app
# bundle, signed with the Developer ID by scripts/mac_app.sh: its own Dock icon, Spotlight and
# Launchpad, and .eml / .ics files open with it), then starts it again. The build's own
# target/release/eudora is never the copy that runs, so rebuilding never touches a running
# program (2026-10-27, first Mac build).
#
#   bash scripts/install-dev.sh                 # install, restart Eudora on the usual library
#   bash scripts/install-dev.sh --no-start      # install only
#
# The library is Eudora's usual one (~/Library/Application Support/Eudora/Library) or the one
# last opened by name; never put it inside Dropbox.
set -euo pipefail
start=1
[ "${1:-}" = "--no-start" ] && start=0

root="$(cd "$(dirname "$0")/.." && pwd)"
app="$HOME/Applications/Eudora.app"

# made in full beside the old one, then put in place by renaming
bash "$root/scripts/mac_app.sh" "$app.new"

if [ $start = 1 ]; then
    pkill -x eudora 2>/dev/null || true
    # the window's background download too (it runs from <library>/bin and would stay on the old
    # program): the window starts the new one within 30 s, and it resumes where it was
    pkill -f "/bin/eudora -l .* graph download" 2>/dev/null || true
    sleep 2
fi
rm -rf "$app.old"
[ -e "$app" ] && mv "$app" "$app.old"
mv "$app.new" "$app"
rm -rf "$app.old"
# tells Finder and Launch Services about the bundle (icon, Open With for .eml / .ics)
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$app" >/dev/null 2>&1 || true
echo "Installed $app"

if [ $start = 1 ]; then
    open "$app"
    sleep 6
    if pgrep -x eudora >/dev/null; then echo "Eudora is running from $app (process $(pgrep -x eudora | tr '\n' ' '))."; else echo "Eudora did not start." >&2; exit 1; fi
fi
