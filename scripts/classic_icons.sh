#!/usr/bin/env bash
# Classic mode's icons (crates/eudora-app/ui/classic/*.svg), redrawn from Eudora 7.1's own
# (Eudora_W_source/Eudora71/Eudora/res/icons/tbar32.bmp and the status / priority bitmaps).
#
# Every icon is built from the same few parts, so they match icon to icon:
#   - one envelope: always the same size and color;
#   - one blue tray: always the same size, color and place;
#   - one glossy arrow (a lit 3D tube, no seam between head and shaft), the same size everywhere;
#     its shaft goes behind the envelope and its head over it;
#   - one green badge and one teal badge (glossy balls with a black arrow).
# Run from the repository: bash scripts/classic_icons.sh
set -euo pipefail
OUT="$(cd "$(dirname "$0")/.." && pwd)/crates/eudora-app/ui/classic"
mkdir -p "$OUT"

# ── the parts ──────────────────────────────────────────────────────────────────────────────

# The arrow, in its own coordinates (the Reply arrow: tail at the bottom right, head pointing left).
ARROW='M21.5 23 C21.5 15.5 19.5 12.8 15.2 12.8 L15.4 15.8 C15.4 16.4 14.9 16.6 14.5 16.2 L6.2 8.6 C5.8 8.2 5.8 7.6 6.2 7.2 L14.5 -0.4 C14.9 -0.8 15.4 -0.6 15.4 0 L15.2 2.6 C24.5 2.6 30 8 30 23 Z'
SHINE='M27.4 23 C27.4 10 22.5 5.2 13.5 5.2 L7.5 7.9'
SHADE='M22.6 23 C22.6 15 20 11.6 14 11.6 L8.5 9'
ARROW_SCALE=0.82
# colors: light mid dark outline
TEAL='86ece0 22a9a3 0b6566 0a4e50'
BLUE='9fd2ff 2c86dc 0d4a96 0b2f66'
GREEN='a6f29a 3cbb3c 13701d 0d4714'

# The envelope: 20 × 15, always this size and color; at X Y.
envelope() {
  local x=$1 y=$2
  cat <<EOF
  <g transform="translate($x $y)">
    <rect x="1.5" y="1.5" width="20" height="15" fill="#5a4a2a" opacity="0.33"/>
    <rect x="0" y="0" width="20" height="15" fill="url(#envelope)" stroke="#a46d1c"/>
    <path d="M0.5 0.5 L10 8 L19.5 0.5 M0.5 14.5 L7.5 7.5 M19.5 14.5 L12.5 7.5" fill="none" stroke="#c48a2c" stroke-width="0.9"/>
    <path d="M1 1 L10 7.6" fill="none" stroke="#fff6d6" stroke-width="0.7" opacity="0.8"/>
  </g>
EOF
}

# The blue tray: the gold standard, traced from Eudora 7.1's own tray (nomail.ico): a 3/4 aerial
# view, the opening a diamond seen from above at an angle, the two walls facing you. Always the
# same size, color and place. `tray` is its back (rim, inner walls, floor: drawn before what
# stands in it), `tray_front` its front (the near rim and the two walls: drawn after).
#   rim, outside:  left 1,18  back 14.2,9  right 31,14  front 16.4,23.8   (walls 4.6 deep)
#   rim, inside:   left 4.3,18.3  back 14.4,11.4  right 27.8,14.6  front 16.2,21.4
tray() {
  cat <<'EOF'
  <path d="M1 18 L14.2 9 L31 14 L16.4 23.8 Z" fill="url(#trayrim)" stroke="#123f6e" stroke-width="0.9" stroke-linejoin="round"/>
  <path d="M4.3 18.3 L14.4 11.4 L27.8 14.6 L16.2 21.4 Z" fill="url(#traywall)"/>
  <path d="M5.2 20.2 L14.6 13.8 L27.2 16.8 L16.2 22.6 Z" fill="url(#trayfloor)" clip-path="url(#trayopening)"/>
EOF
}
tray_front() {
  cat <<'EOF'
  <path d="M1 18 L16.4 23.8 L31 14 L27.8 14.6 L16.2 21.4 L4.3 18.3 Z" fill="url(#trayrim)" stroke="#123f6e" stroke-width="0.9" stroke-linejoin="round"/>
  <path d="M1 18 L16.4 23.8 V28.8 L1 22.6 Z" fill="url(#trayleft)" stroke="#123f6e" stroke-width="0.9" stroke-linejoin="round"/>
  <path d="M16.4 23.8 L31 14 V18.8 L16.4 28.8 Z" fill="url(#trayright)" stroke="#123f6e" stroke-width="0.9" stroke-linejoin="round"/>
  <path d="M2.2 19.6 L16.4 25 L29.8 16" fill="none" stroke="#bfe8fd" stroke-width="0.8" opacity="0.75"/>
EOF
}

# An arrow: ID COLORS TRANSFORM PART (all | shaft | head): its gradient and clips go in DEFS.
DEFS=""
arrow_defs() {
  local id=$1; shift
  read -r light mid dark line <<<"$*"
  DEFS+="    <linearGradient id=\"a$id\" x1=\"1\" y1=\"0\" x2=\"0.3\" y2=\"0.9\"><stop offset=\"0\" stop-color=\"#$light\"/><stop offset=\"0.5\" stop-color=\"#$mid\"/><stop offset=\"1\" stop-color=\"#$dark\"/></linearGradient>
    <clipPath id=\"in$id\"><path d=\"$ARROW\"/></clipPath>
"
}
arrow() {
  local id=$1 colors=$2 tf=$3 part=$4
  read -r _ _ _ line <<<"$colors"
  # head = the part left of x 15.6 in the arrow's own coordinates
  local clip=""
  case $part in
    head) clip=' clip-path="url(#headonly)"' ;;
    shaft) clip=' clip-path="url(#shaftonly)"' ;;
  esac
  cat <<EOF
  <g transform="$tf"><g$clip>
    <path d="$ARROW" fill="url(#a$id)"/>
    <g clip-path="url(#in$id)">
      <path d="$SHINE" fill="none" stroke="#f4fffd" stroke-width="2.4" stroke-linecap="round" opacity="0.85" filter="url(#soft)"/>
      <path d="$SHADE" fill="none" stroke="#02262a" stroke-width="2" stroke-linecap="round" opacity="0.5" filter="url(#soft)"/>
    </g>
    <path d="$ARROW" fill="none" stroke="#$line" stroke-width="1" stroke-linejoin="round"/>
  </g></g>
EOF
}

# A badge (a glossy ball with a black arrow): KIND (green | teal), the arrow inside: right | down |
# up | plus | into (a curved arrow going down and right, into the tray).
badge() {
  local kind=$1 what=$2 cx=7.5 cy=7.5 r=6.2
  local mark
  case $what in
    right) mark="M4.3 7.5 H10.3 M8.1 5 L10.6 7.5 L8.1 10" ;;
    down) mark="M7.5 4.2 V10.4 M5 8 L7.5 10.6 L10 8" ;;
    up) mark="M7.5 10.8 V4.6 M5 7 L7.5 4.4 L10 7" ;;
    plus) mark="M7.5 4.2 V10.8 M4.2 7.5 H10.8" ;;
    into) mark="M4.2 5.2 C6.8 4.6 9.2 5.8 10.2 8.9 M11.2 6.2 L10.4 9.9 L6.9 9" ;;
  esac
  cat <<EOF
  <circle cx="$cx" cy="$cy" r="$r" fill="url(#badge$kind)" stroke="$([ "$kind" = green ] && echo '#145a22' || echo '#0b3b42')"/>
  <ellipse cx="6" cy="4.6" rx="3.4" ry="1.9" fill="#ffffff" opacity="0.45"/>
  <path d="$mark" fill="none" stroke="#0d1a10" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>
EOF
}

# Writes one icon: NAME COMMENT BODY (the drawing; the parts' definitions are added).
icon() {
  local name=$1 comment=$2 body=$3
  cat >"$OUT/$name.svg" <<EOF
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" width="32" height="32">
  <!-- $comment -->
  <!-- generated by scripts/classic_icons.sh: edit that, not this file -->
  <defs>
    <linearGradient id="envelope" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#ffe9a8"/><stop offset="1" stop-color="#e8b64e"/></linearGradient>
    <linearGradient id="trayrim" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#8fd2fb"/><stop offset="0.5" stop-color="#3f9ee3"/><stop offset="1" stop-color="#2a7fc9"/></linearGradient>
    <linearGradient id="traywall" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#0f4b86"/><stop offset="0.5" stop-color="#1d66ad"/><stop offset="1" stop-color="#4aa3e4"/></linearGradient>
    <linearGradient id="trayfloor" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#ffffff"/><stop offset="0.55" stop-color="#dff5fe"/><stop offset="1" stop-color="#9fd8f5"/></linearGradient>
    <linearGradient id="trayleft" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#5db7ef"/><stop offset="0.5" stop-color="#2b88d4"/><stop offset="1" stop-color="#16599b"/></linearGradient>
    <linearGradient id="trayright" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#3b92d8"/><stop offset="1" stop-color="#0f4682"/></linearGradient>
    <clipPath id="trayopening"><path d="M4.3 18.3 L14.4 11.4 L27.8 14.6 L16.2 21.4 Z"/></clipPath>
    <radialGradient id="badgegreen" cx="0.38" cy="0.32" r="0.75"><stop offset="0" stop-color="#e9ffe9"/><stop offset="0.45" stop-color="#7fe08a"/><stop offset="1" stop-color="#259a3a"/></radialGradient>
    <radialGradient id="badgeteal" cx="0.38" cy="0.32" r="0.75"><stop offset="0" stop-color="#d9fffb"/><stop offset="0.45" stop-color="#3cc6bd"/><stop offset="1" stop-color="#0f6f74"/></radialGradient>
    <linearGradient id="page" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#fff6cf"/><stop offset="1" stop-color="#f0cf78"/></linearGradient>
    <filter id="soft" x="-20%" y="-20%" width="140%" height="140%"><feGaussianBlur stdDeviation="0.55"/></filter>
    <clipPath id="headonly"><rect x="4" y="-3" width="11.6" height="22"/></clipPath>
    <clipPath id="shaftonly"><rect x="15.6" y="-3" width="20" height="30"/></clipPath>
$DEFS  </defs>
$body
</svg>
EOF
  DEFS=""
}

# ── the icons ──────────────────────────────────────────────────────────────────────────────

ENV_X=3; ENV_Y=12          # where the envelope sits in the envelope-and-arrow icons
REPLY_TF="translate(5.3 3.4) scale($ARROW_SCALE)"
FORWARD_TF="translate(26.7 3.4) scale(-$ARROW_SCALE $ARROW_SCALE)"

arrow_defs r $TEAL
icon reply "Reply (tbar32 #6): the envelope, and the teal arrow curving up and back to the left" \
"$(arrow r "$TEAL" "$REPLY_TF" shaft)
$(envelope $ENV_X $ENV_Y)
$(arrow r "$TEAL" "$REPLY_TF" head)"

arrow_defs f $TEAL
icon forward "Forward (tbar32 #10): the envelope, and the teal arrow curving up and on to the right" \
"$(arrow f "$TEAL" "$FORWARD_TF" shaft)
$(envelope $ENV_X $ENV_Y)
$(arrow f "$TEAL" "$FORWARD_TF" head)"

ALL_BACK="translate(8 1.2) scale($ARROW_SCALE)"
ALL_FRONT="translate(4.3 5.4) scale($ARROW_SCALE)"
arrow_defs t $TEAL; arrow_defs b $BLUE
icon reply-all "Reply to All (tbar32 #9): two parallel arrows of the same shape, teal behind and blue laid over it" \
"$(arrow t "$TEAL" "$ALL_BACK" shaft)
$(arrow b "$BLUE" "$ALL_FRONT" shaft)
$(envelope $ENV_X $ENV_Y)
$(arrow t "$TEAL" "$ALL_BACK" head)
$(arrow b "$BLUE" "$ALL_FRONT" head)"

# Redirect: a blue arrow coming in to the envelope from the left (its head over the envelope), and
# the green arrow leaving it up and to the right
IN_TF="translate(12.5 13.2) scale(-0.62 0.62)"
arrow_defs i $BLUE; arrow_defs o $GREEN
icon redirect "Redirect (tbar32 #12): a blue arrow coming in to the envelope, and a green arrow leaving it (the message goes on as it is)" \
"$(arrow o "$GREEN" "$FORWARD_TF" shaft)
$(arrow i "$BLUE" "$IN_TF" shaft)
$(envelope $ENV_X $ENV_Y)
$(arrow i "$BLUE" "$IN_TF" head)
$(arrow o "$GREEN" "$FORWARD_TF" head)"

icon new-message "New Message (tbar32 #4): the envelope, a page, and the green badge" \
"$(envelope $ENV_X $ENV_Y)
  <path d=\"M18.5 18.5 H29.5 V31 H18.5 Z\" fill=\"url(#page)\" stroke=\"#a46d1c\"/>
  <path d=\"M20.5 22 H27.5 M20.5 24.5 H27.5 M20.5 27 H25.5\" stroke=\"#8c7a49\" stroke-width=\"1\"/>
$(badge green plus)"

icon attach "Attach File (tbar32 #18): the envelope with a page clipped to it" \
"$(envelope $ENV_X 4)
  <path d=\"M14.5 13.5 H25 L29.5 18 V31 H14.5 Z\" fill=\"url(#page)\" stroke=\"#a46d1c\"/>
  <path d=\"M25 13.5 V18 H29.5\" fill=\"#e2b552\" stroke=\"#a46d1c\"/>
  <path d=\"M17 21 H27 M17 23.5 H27 M17 26 H27 M17 28.5 H23\" stroke=\"#8c7a49\" stroke-width=\"1\"/>
  <path d=\"M19 8.5 V19.5 C19 22 22.5 22 22.5 19.5 V11 C22.5 9.5 20.5 9.5 20.5 11 V18.5\" fill=\"none\" stroke=\"#3b3f44\" stroke-width=\"1.3\" stroke-linecap=\"round\"/>"

icon spelling "Check Spelling (tbar32 #19): the envelope, and in front a page with red check marks" \
"$(envelope $ENV_X 3)
  <path d=\"M13.5 11.5 H25 L29.5 16 V31 H13.5 Z\" fill=\"url(#page)\" stroke=\"#a46d1c\"/>
  <path d=\"M25 11.5 V16 H29.5\" fill=\"#e2b552\" stroke=\"#a46d1c\"/>
  <path d=\"M21 18 H27.5 M21 23 H27.5 M21 28 H27.5\" stroke=\"#5a4a2a\" stroke-width=\"1.3\"/>
  <path d=\"M15 17.5 L16.8 19.5 L20 15.5 M15 22.5 L16.8 24.5 L20 20.5 M15 27.5 L16.8 29.5 L20 25.5\" fill=\"none\" stroke=\"#d01b1b\" stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>"

# the tray icons: the tray never moves; the envelope (when there is one) stands in it
IN_TRAY_X=11; IN_TRAY_Y=5    # the envelope always in front of the tray
icon in "In mailbox (tbar32 #1): the blue tray, IN, and the green badge with the arrow down" \
"$(tray)
$(tray_front)
  <rect x=\"15\" y=\"2.5\" width=\"15\" height=\"8\" rx=\"1\" fill=\"#a9adb1\" stroke=\"#3b3f44\"/>
  <text x=\"22.5\" y=\"9.2\" font-family=\"Verdana, Arial, sans-serif\" font-size=\"6.6\" font-weight=\"bold\" fill=\"#fff\" stroke=\"#2b2f33\" stroke-width=\"0.9\" paint-order=\"stroke\" text-anchor=\"middle\">IN</text>
$(badge green down)"

icon out "Out mailbox (tbar32 #2): the blue tray with a waiting page, OUT, and the green badge with the arrow up" \
"$(tray)
  <path d=\"M6 19.5 L11.5 15.5 H25 L19.5 19.5 Z\" fill=\"url(#page)\" stroke=\"#a46d1c\" stroke-width=\"0.8\"/>
$(tray_front)
  <rect x=\"14\" y=\"2.5\" width=\"17\" height=\"8\" rx=\"1\" fill=\"#a9adb1\" stroke=\"#3b3f44\"/>
  <text x=\"22.5\" y=\"9.2\" font-family=\"Verdana, Arial, sans-serif\" font-size=\"6.2\" font-weight=\"bold\" fill=\"#fff\" stroke=\"#2b2f33\" stroke-width=\"0.9\" paint-order=\"stroke\" text-anchor=\"middle\">OUT</text>
$(badge green up)"

icon check-mail "Check Mail (tbar32 #3): the envelope dropping into the blue tray, and the green badge with the arrow down" \
"$(tray)
$(tray_front)
$(envelope $IN_TRAY_X $IN_TRAY_Y)
$(badge green down)"

icon send-again "Send Again (tbar32 #14): the envelope leaving the blue tray once more, and the green badge with the arrow right" \
"$(tray)
$(tray_front)
$(envelope $IN_TRAY_X $IN_TRAY_Y)
$(badge green right)"

icon send "Send Immediately (tbar32 #15): the envelope in the blue tray, and the teal badge sending it on its way" \
"$(tray)
$(tray_front)
$(envelope $IN_TRAY_X $IN_TRAY_Y)
$(badge teal into)"

icon send-queued "Send Queued Messages (tbar32 #29): as Send Immediately (classic drew them alike)" \
"$(tray)
$(tray_front)
$(envelope $IN_TRAY_X $IN_TRAY_Y)
$(badge teal into)"

icon transfer "Transfer (tbar32 #57): the blue tray, and the teal badge with the arrow going in" \
"$(tray)
$(tray_front)
$(badge teal into)"

icon delete "Delete (tbar32 #0): the grey can, and the green badge with the arrow going in" \
"  <path d=\"M9 12 H25 L23.5 30 H10.5 Z\" fill=\"url(#can)\" stroke=\"#2d3133\"/>
  <path d=\"M13 14.5 L13.6 27.5 M17 14.5 V27.5 M21 14.5 L20.4 27.5\" stroke=\"#3a4044\" stroke-width=\"1.2\" opacity=\"0.8\"/>
  <rect x=\"7.5\" y=\"9.5\" width=\"19\" height=\"3\" rx=\"1\" fill=\"#9aa1a5\" stroke=\"#2d3133\"/>
  <path d=\"M14.5 9.5 V8 H19.5 V9.5\" fill=\"none\" stroke=\"#2d3133\" stroke-width=\"1.2\"/>
$(badge green right)"
sed -i 's|<filter id="soft"|<linearGradient id="can" x1="0" x2="1"><stop offset="0" stop-color="#5d6367"/><stop offset="0.45" stop-color="#b9c0c4"/><stop offset="1" stop-color="#4b5155"/></linearGradient>\n    <filter id="soft"|' "$OUT/delete.svg"

# the straight arrows: the same lit tube as the curved one
straight() {
  local name=$1 comment=$2 colors=$3 path=$4 shine=$5
  read -r light mid dark line <<<"$colors"
  DEFS+="    <linearGradient id=\"s\" x1=\"0\" x2=\"1\"><stop offset=\"0\" stop-color=\"#$dark\"/><stop offset=\"0.35\" stop-color=\"#$light\"/><stop offset=\"0.6\" stop-color=\"#$mid\"/><stop offset=\"1\" stop-color=\"#$dark\"/></linearGradient>
    <clipPath id=\"sc\"><path d=\"$path\"/></clipPath>
"
  icon "$name" "$comment" "  <path d=\"$path\" fill=\"url(#s)\"/>
  <g clip-path=\"url(#sc)\"><path d=\"$shine\" fill=\"none\" stroke=\"#ffffff\" stroke-width=\"2.4\" stroke-linecap=\"round\" opacity=\"0.85\" filter=\"url(#soft)\"/></g>
  <path d=\"$path\" fill=\"none\" stroke=\"#$line\" stroke-width=\"1\" stroke-linejoin=\"round\"/>"
}
straight previous "Previous Message (tbar32 #16): the green arrow up" "$GREEN" \
  "M16 2.5 C16.5 2.5 17 2.8 17.4 3.2 L27.6 14.6 C28.1 15.2 27.7 16 26.9 16 H21.5 V28.5 C21.5 29.1 21.1 29.5 20.5 29.5 H11.5 C10.9 29.5 10.5 29.1 10.5 28.5 V16 H5.1 C4.3 16 3.9 15.2 4.4 14.6 L14.6 3.2 C15 2.8 15.5 2.5 16 2.5 Z" \
  "M13.6 27.5 V15 M14.5 6.5 L8.5 13.5"
straight next "Next Message (tbar32 #17): the teal arrow down" "$TEAL" \
  "M16 29.5 C15.5 29.5 15 29.2 14.6 28.8 L4.4 17.4 C3.9 16.8 4.3 16 5.1 16 H10.5 V3.5 C10.5 2.9 10.9 2.5 11.5 2.5 H20.5 C21.1 2.5 21.5 2.9 21.5 3.5 V16 H26.9 C27.7 16 28.1 16.8 27.6 17.4 L17.4 28.8 C17 29.2 16.5 29.5 16 29.5 Z" \
  "M13.6 4.5 V17 M8 18.5 L13.5 24.5"

# Print: a modern laser printer, a page coming out of the top
DEFS+='    <linearGradient id="case" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#fbfcfd"/><stop offset="0.6" stop-color="#d9dee3"/><stop offset="1" stop-color="#9aa3ab"/></linearGradient>
    <linearGradient id="lid" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#5b6570"/><stop offset="1" stop-color="#2c333a"/></linearGradient>
'
icon print "Print: a modern laser printer, the printed page coming out at the top" \
"  <path d=\"M9.5 2.5 H22.5 V12 H9.5 Z\" fill=\"#ffffff\" stroke=\"#8a929a\"/>
  <path d=\"M12 5.5 H20 M12 7.8 H20 M12 10 H17\" stroke=\"#a9b1b9\" stroke-width=\"1\"/>
  <path d=\"M4 13 C4 11.6 5 10.5 6.5 10.5 H25.5 C27 10.5 28 11.6 28 13 V14 H4 Z\" fill=\"url(#lid)\" stroke=\"#20262c\"/>
  <path d=\"M8 12 H24\" stroke=\"#11161a\" stroke-width=\"1.4\" stroke-linecap=\"round\"/>
  <path d=\"M3 14 H29 V25.5 C29 26.9 28 28 26.5 28 H5.5 C4 28 3 26.9 3 25.5 Z\" fill=\"url(#case)\" stroke=\"#5b646c\"/>
  <path d=\"M7 23.5 H25\" stroke=\"#8a939b\" stroke-width=\"1.2\"/>
  <path d=\"M7 25.5 H25\" stroke=\"#c3cad0\" stroke-width=\"0.8\"/>
  <rect x=\"20\" y=\"16.8\" width=\"5.5\" height=\"2.8\" rx=\"0.6\" fill=\"#24384d\" stroke=\"#1a2733\" stroke-width=\"0.6\"/>
  <circle cx=\"17.6\" cy=\"18.2\" r=\"0.9\" fill=\"#3ad15a\"/>
  <path d=\"M4.6 15.5 H27.4\" stroke=\"#ffffff\" stroke-width=\"0.9\" opacity=\"0.9\"/>"

# Junk: a tin can of fish, lid rolled back, and a fish skeleton
DEFS+='    <linearGradient id="tin" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#f2f4f6"/><stop offset="0.5" stop-color="#b8c0c7"/><stop offset="1" stop-color="#7d8790"/></linearGradient>
    <linearGradient id="oil" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#e9c46a"/><stop offset="1" stop-color="#b98a2e"/></linearGradient>
    <linearGradient id="fish" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#dfe7ef"/><stop offset="1" stop-color="#8a9bb0"/></linearGradient>
'
icon junk "Junk mailbox: a tin can of fish with its lid rolled back, and a fish skeleton" \
"  <ellipse cx=\"14.5\" cy=\"24\" rx=\"12\" ry=\"5.2\" fill=\"#3a4148\" opacity=\"0.25\"/>
  <path d=\"M2.5 17.5 C2.5 14.5 5.5 13 9 13 H20 C23.5 13 26.5 14.5 26.5 17.5 V21.5 C26.5 24.5 23.5 26 20 26 H9 C5.5 26 2.5 24.5 2.5 21.5 Z\" fill=\"url(#tin)\" stroke=\"#4b545c\"/>
  <path d=\"M4 17.3 C4 15.4 6.2 14.5 9 14.5 H20 C22.8 14.5 25 15.4 25 17.3 C25 19.2 22.8 20.1 20 20.1 H9 C6.2 20.1 4 19.2 4 17.3 Z\" fill=\"url(#oil)\" stroke=\"#7a5a1c\" stroke-width=\"0.8\"/>
  <path d=\"M6 17.6 C8 16 11.5 16 13 17.4 L14.5 16.3 V18.7 L13 17.8 C11.5 19 8 19 6 17.6 Z\" fill=\"url(#fish)\" stroke=\"#4f6070\" stroke-width=\"0.6\"/>
  <path d=\"M13.5 17.6 C15.5 16 19 16 20.5 17.4 L22 16.3 V18.7 L20.5 17.8 C19 19 15.5 19 13.5 17.6 Z\" fill=\"url(#fish)\" stroke=\"#4f6070\" stroke-width=\"0.6\"/>
  <path d=\"M19 13.2 C21 9.5 25.5 8 29.5 9.5 C27.5 10.5 26.5 12 26.5 14.2\" fill=\"#d5dbe0\" stroke=\"#4b545c\"/>
  <path d=\"M26.5 13.8 C27.3 12.4 28.4 11.6 29.6 11.2\" fill=\"none\" stroke=\"#4b545c\" stroke-width=\"1.4\" stroke-linecap=\"round\"/>
  <g transform=\"translate(1.5 2.5)\" stroke=\"#3c3c3c\" stroke-width=\"0.9\" stroke-linecap=\"round\" fill=\"none\">
    <path d=\"M4 5 H16\" stroke-width=\"1.1\"/>
    <path d=\"M7 2.5 V7.5 M9.5 2 V8 M12 2.5 V7.5 M14.5 3.2 V6.8\"/>
    <path d=\"M16 5 L19.5 2.2 M16 5 L19.5 7.8\" stroke-width=\"1.1\"/>
    <circle cx=\"2.6\" cy=\"5\" r=\"2\" fill=\"#f4f4f4\"/>
    <circle cx=\"2.1\" cy=\"4.6\" r=\"0.55\" fill=\"#3c3c3c\" stroke=\"none\"/>
  </g>"

# The tray icons (the system tray: crates/eudora-app/icons): the same tray, twice the size; new
# mail = the glossy green arrow coming down into it (Eudora 7.1's havemail.ico / nomail.ico)
TRAY_OUT="$(cd "$(dirname "$0")/.." && pwd)/crates/eudora-app/icons"
DOWN='M16 29.5 C15.5 29.5 15 29.2 14.6 28.8 L4.4 17.4 C3.9 16.8 4.3 16 5.1 16 H10.5 V3.5 C10.5 2.9 10.9 2.5 11.5 2.5 H20.5 C21.1 2.5 21.5 2.9 21.5 3.5 V16 H26.9 C27.7 16 28.1 16.8 27.6 17.4 L17.4 28.8 C17 29.2 16.5 29.5 16 29.5 Z'
for kind in nomail newmail; do
  arrowpart=""
  if [ $kind = newmail ]; then
    read -r light mid dark line <<<"$GREEN"
    DEFS+="    <linearGradient id=\"s\" x1=\"0\" x2=\"1\"><stop offset=\"0\" stop-color=\"#$dark\"/><stop offset=\"0.35\" stop-color=\"#$light\"/><stop offset=\"0.6\" stop-color=\"#$mid\"/><stop offset=\"1\" stop-color=\"#$dark\"/></linearGradient>
    <clipPath id=\"sc\"><path d=\"$DOWN\"/></clipPath>
"
    arrowpart="  <g transform=\"translate(5 -1) scale(0.62)\">
    <path d=\"$DOWN\" fill=\"url(#s)\"/>
    <g clip-path=\"url(#sc)\"><path d=\"M13.6 4.5 V17 M8 18.5 L13.5 24.5\" fill=\"none\" stroke=\"#ffffff\" stroke-width=\"2.4\" stroke-linecap=\"round\" opacity=\"0.85\" filter=\"url(#soft)\"/></g>
    <path d=\"$DOWN\" fill=\"none\" stroke=\"#$line\" stroke-width=\"1.2\" stroke-linejoin=\"round\"/>
  </g>"
  fi
  icon "tray-$kind" "The tray icon: $( [ $kind = newmail ] && echo 'new mail, the green arrow coming into the blue tray' || echo 'no new mail, the empty blue tray' )" \
"  <g transform=\"translate(0 3)\">
$(tray)
$(tray_front)
  </g>
$arrowpart"
  sed 's|viewBox="0 0 32 32" width="32" height="32"|viewBox="0 0 32 32" width="64" height="64"|' "$OUT/tray-$kind.svg" > "$TRAY_OUT/tray-$kind.svg"
  rm "$OUT/tray-$kind.svg"
done

echo "Wrote the classic icons into $OUT"
