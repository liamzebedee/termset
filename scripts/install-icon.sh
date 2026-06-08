#!/usr/bin/env bash
#
# install-icon.sh — give the termset binary (mtm) a real icon + launcher entry on
# Ubuntu (GNOME). Idempotent: re-run it any time to *update* the icon and
# desktop entry (it overwrites in place and refreshes the icon/desktop caches).
#
# What it installs (per-user, no sudo):
#   ~/.local/share/icons/hicolor/scalable/apps/termset.svg   (+ PNG sizes)
#   ~/.local/share/applications/termset.desktop
#
# The .desktop sets StartupWMClass=termset, which the app sets as its X11
# WM class / Wayland app_id (see src/main.rs), so the dock/launcher icon
# binds to the running window.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$SCRIPT_DIR")"
BIN="$REPO/target/release/terms"

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_BASE="$DATA/icons/hicolor"
APPS="$DATA/applications"
DESKTOP="$APPS/termset.desktop"

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
SVG="$ICON_BASE/scalable/apps/termset.svg"
cat > "$SVG" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="256" height="256">
  <defs>
    <!-- Dark-green terminal vignette (matches our prompt background) -->
    <radialGradient id="bgr" cx="50%" cy="42%" r="75%">
      <stop offset="0%"  stop-color="#3a7361"/>
      <stop offset="60%" stop-color="#244a3e"/>
      <stop offset="100%" stop-color="#15241d"/>
    </radialGradient>
    <!-- Light-blue powerline prompt -->
    <linearGradient id="prompt" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%"   stop-color="#a9dcf7"/>
      <stop offset="100%" stop-color="#6cb6e6"/>
    </linearGradient>
    <!-- Top sheen -->
    <linearGradient id="sheen" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%"   stop-color="#ffffff" stop-opacity="0.16"/>
      <stop offset="14%"  stop-color="#ffffff" stop-opacity="0"/>
    </linearGradient>
  </defs>

  <!-- Rounded terminal body -->
  <rect x="20" y="20" width="216" height="216" rx="46" fill="url(#bgr)"/>
  <rect x="20" y="20" width="216" height="216" rx="46" fill="url(#sheen)"/>
  <!-- Inner edge for a touch of depth -->
  <rect x="20.5" y="20.5" width="215" height="215" rx="45.5"
        fill="none" stroke="#000000" stroke-opacity="0.30" stroke-width="1"/>
  <rect x="22.5" y="22.5" width="211" height="211" rx="44"
        fill="none" stroke="#ffffff" stroke-opacity="0.10" stroke-width="1"/>

  <!-- >_  light-blue powerline prompt -->
  <g fill="none" stroke="url(#prompt)" stroke-width="20"
     stroke-linecap="round" stroke-linejoin="round">
    <polyline points="84,92 128,131 84,170"/>
  </g>
  <rect x="132" y="158" width="56" height="18" rx="9" fill="url(#prompt)"/>
</svg>
SVG
log "icon  -> $SVG"

# 3. Rasterize a few PNG sizes too, if a converter is available (optional;
#    GNOME renders the SVG fine on its own).
rasterize() {
    local size="$1" out="$ICON_BASE/${1}x${1}/apps/termset.png"
    mkdir -p "$(dirname "$out")"
    if   command -v rsvg-convert            >/dev/null; then rsvg-convert -w "$size" -h "$size" "$SVG" -o "$out"
    elif command -v inkscape                >/dev/null; then inkscape "$SVG" --export-type=png -w "$size" -h "$size" -o "$out" >/dev/null 2>&1
    elif command -v gdk-pixbuf-thumbnailer  >/dev/null; then gdk-pixbuf-thumbnailer -s "$size" "$SVG" "$out"
    elif command -v convert                 >/dev/null; then convert -background none -density 384 "$SVG" -resize "${size}x${size}" "$out"
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
Name=termset
GenericName=Workspace Terminal
Comment=Save your terminal layouts
Exec=$BIN
Path=$REPO
Icon=termset
Terminal=false
Categories=System;TerminalEmulator;
StartupWMClass=termset
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
echo "Done. 'termset' is now in your app grid / dock."
echo "If the icon doesn't refresh immediately, log out and back in."
