#!/usr/bin/env bash
#
# install-icon.sh — give the termem binary a real icon + launcher entry on
# Ubuntu (GNOME). Idempotent: re-run it any time to *update* the icon and
# desktop entry (it overwrites in place and refreshes the icon/desktop caches).
#
# What it installs (per-user, no sudo):
#   ~/.local/share/icons/hicolor/scalable/apps/termem.svg   (+ PNG sizes)
#   ~/.local/share/applications/termem.desktop
#
# The .desktop sets StartupWMClass=termem, which the app sets as its X11
# WM class / Wayland app_id (see src/main.rs), so the dock/launcher icon
# binds to the running window.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$SCRIPT_DIR")"
BIN="$REPO/target/release/termem-demo"

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_BASE="$DATA/icons/hicolor"
APPS="$DATA/applications"
DESKTOP="$APPS/termem.desktop"

log() { printf '  %s\n' "$*"; }

# 1. Ensure the release binary exists (build if needed).
if [ ! -x "$BIN" ]; then
    log "building release binary…"
    ( cd "$REPO" && cargo build --release )
fi
[ -x "$BIN" ] || { echo "error: $BIN not found after build" >&2; exit 1; }

# 2. Write the icon as SVG (scalable). It mirrors the actual UI: a
#    borderless Win2k window — left tree sidebar with the info square,
#    a gradient title bar, and a terminal with run markers + cursor.
mkdir -p "$ICON_BASE/scalable/apps"
SVG="$ICON_BASE/scalable/apps/termem.svg"
cat > "$SVG" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
  <defs>
    <linearGradient id="title" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#d9d9d9"/>
      <stop offset="1" stop-color="#bebebe"/>
    </linearGradient>
  </defs>
  <!-- window body + explicit Win2k border -->
  <rect x="20" y="28" width="216" height="200" fill="#ffffff"
        stroke="#1a1a1a" stroke-width="5"/>
  <!-- title bar -->
  <rect x="22.5" y="30.5" width="211" height="34" fill="url(#title)"/>
  <line x1="22.5" y1="64.5" x2="233.5" y2="64.5" stroke="#808080" stroke-width="2"/>
  <!-- window buttons -->
  <rect x="170" y="40" width="14" height="3" fill="#1a1a1a"/>
  <rect x="192" y="38" width="13" height="13" fill="none" stroke="#1a1a1a" stroke-width="2"/>
  <path d="M214 38 l13 13 M227 38 l-13 13" stroke="#1a1a1a" stroke-width="2"/>
  <!-- left sidebar -->
  <rect x="22.5" y="64.5" width="66" height="161" fill="#c0c0c0"/>
  <line x1="88.5" y1="64.5" x2="88.5" y2="225.5" stroke="#808080" stroke-width="2"/>
  <!-- info square (the inspector toggle) -->
  <rect x="31" y="73" width="16" height="16" fill="#ffffff" stroke="#1a1a1a" stroke-width="2"/>
  <rect x="38" y="76.5" width="3" height="3" fill="#1a1a1a"/>
  <rect x="38" y="81" width="3" height="6" fill="#1a1a1a"/>
  <!-- tree rows -->
  <rect x="31" y="98"  width="46" height="6" fill="#7a7a7a"/>
  <rect x="37" y="110" width="40" height="6" fill="#9a9a9a"/>
  <rect x="37" y="122" width="40" height="6" fill="#9a9a9a"/>
  <rect x="31" y="134" width="46" height="6" fill="#7a7a7a"/>
  <!-- terminal content -->
  <rect x="100" y="80"  width="118" height="7" fill="#1a1a1a"/>
  <rect x="100" y="80"  width="9"   height="7" fill="#107c10"/>
  <rect x="100" y="98"  width="92"  height="7" fill="#3a3a3a"/>
  <rect x="100" y="116" width="104" height="7" fill="#3a3a3a"/>
  <rect x="100" y="134" width="70"  height="7" fill="#3a3a3a"/>
  <rect x="100" y="152" width="11"  height="13" fill="#1a1a1a"/>
</svg>
SVG
log "icon  -> $SVG"

# 3. Rasterize a few PNG sizes too, if a converter is available (optional;
#    GNOME renders the SVG fine on its own).
rasterize() {
    local size="$1" out="$ICON_BASE/${1}x${1}/apps/termem.png"
    mkdir -p "$(dirname "$out")"
    if   command -v rsvg-convert >/dev/null; then rsvg-convert -w "$size" -h "$size" "$SVG" -o "$out"
    elif command -v inkscape     >/dev/null; then inkscape "$SVG" --export-type=png -w "$size" -h "$size" -o "$out" >/dev/null 2>&1
    elif command -v convert      >/dev/null; then convert -background none -resize "${size}x${size}" "$SVG" "$out"
    else return 1
    fi
    log "icon  -> $out"
}
for s in 48 64 128 256; do rasterize "$s" || { log "(no SVG rasterizer; SVG only)"; break; }; done

# 4. Desktop entry. Path= so the app's ./workspace resolves to this repo;
#    StartupWMClass binds the launcher icon to the live window.
mkdir -p "$APPS"
cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=termem
GenericName=Workspace Terminal
Comment=Workspace-organized terminal emulator
Exec=$BIN
Path=$REPO
Icon=termem
Terminal=false
Categories=System;TerminalEmulator;
StartupWMClass=termem
StartupNotify=true
EOF
chmod +x "$DESKTOP"
log "entry -> $DESKTOP"

# 5. Refresh caches so the change shows up without a re-login.
gtk-update-icon-cache -f -t "$ICON_BASE" >/dev/null 2>&1 || true
update-desktop-database "$APPS"          >/dev/null 2>&1 || true
# Mark the launcher trusted on GNOME (silences the "Allow Launching" prompt).
command -v gio >/dev/null && gio set "$DESKTOP" metadata::trusted true >/dev/null 2>&1 || true

echo
echo "Done. 'termem' is now in your app grid / dock."
echo "If the icon doesn't refresh immediately, log out and back in."
