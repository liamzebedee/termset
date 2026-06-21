#!/usr/bin/env bash
#
# install-icon.sh — give the termset binary (terms) a real icon + launcher entry on
# Ubuntu (GNOME). Idempotent: re-run it any time to *update* the icon and
# desktop entry (it overwrites in place and refreshes the icon/desktop caches).
#
# What it installs (per-user, no sudo):
#   ~/.local/share/icons/hicolor/scalable/apps/termset.svg   (+ PNG sizes)
#   ~/.local/share/applications/termset.desktop
#
# The assets it installs live OUTSIDE this script so you can edit them directly:
#   termset.svg                  — the icon artwork (also used in the README)
#   assets/termset.desktop.in    — the launcher entry template (@PLACEHOLDERS@)
#
# The launcher opens $WORKSPACE (default ~/workspace01.yml) — your "home"
# workspace. Override it with WORKSPACE=… ./scripts/install-icon.sh, or just
# edit the file once it's been seeded.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$SCRIPT_DIR")"
BIN="$REPO/target/release/terms"

# Editable assets, kept as files rather than inlined heredocs.
SVG_SRC="$REPO/termset.svg"
DESKTOP_TEMPLATE="$REPO/assets/termset.desktop.in"

# The workspace the launcher opens. A missing file just opens the default
# layout, but we seed it below so the icon always lands on a real workspace.
WORKSPACE="${WORKSPACE:-$HOME/.terms/workspace01.yaml}"

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_BASE="$DATA/icons/hicolor"
APPS="$DATA/applications"
DESKTOP="$APPS/termset.desktop"

log() { printf '  %s\n' "$*"; }

[ -f "$SVG_SRC" ]          || { echo "error: icon $SVG_SRC not found" >&2; exit 1; }
[ -f "$DESKTOP_TEMPLATE" ] || { echo "error: template $DESKTOP_TEMPLATE not found" >&2; exit 1; }

# 1. Ensure the release binary exists (build if needed).
if [ ! -x "$BIN" ]; then
    log "building release binary…"
    ( cd "$REPO" && cargo build --release )
fi
[ -x "$BIN" ] || { echo "error: $BIN not found after build" >&2; exit 1; }

# 2. Install the icon SVG (scalable) by copying the editable source in place.
mkdir -p "$ICON_BASE/scalable/apps"
SVG="$ICON_BASE/scalable/apps/termset.svg"
cp "$SVG_SRC" "$SVG"
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

# 4. Seed the home workspace if it doesn't exist yet, so the launcher opens a
#    real layout (the bundled default template, with placeholders filled in).
if [ ! -e "$WORKSPACE" ]; then
    mkdir -p "$(dirname "$WORKSPACE")"
    name="$(basename "${WORKSPACE%.*}")"
    { printf '# %s — edit freely; the termset launcher opens this file.\n' "$name"
      grep -v '^#' "$REPO/assets/default-termset.yml" \
        | sed -e "s|{{name}}|$name|g" -e "s|{{dir}}|$HOME|g"
    } > "$WORKSPACE"
    log "workspace seeded -> $WORKSPACE"
else
    log "workspace -> $WORKSPACE (kept)"
fi

# 5. Desktop entry, rendered from assets/termset.desktop.in. Path= so the app's
#    relative paths resolve under $HOME; Exec opens $WORKSPACE.
mkdir -p "$APPS"
sed -e "s|@BIN@|$BIN|g" \
    -e "s|@WORKSPACE@|$WORKSPACE|g" \
    -e "s|@PATH@|$HOME|g" \
    "$DESKTOP_TEMPLATE" | grep -v '^#' > "$DESKTOP"
chmod +x "$DESKTOP"
log "entry -> $DESKTOP"

# 5b. Clean up legacy artifacts from when the bin was named `mtm` and the
#     launcher was `termem.desktop`. cargo leaves the old binary behind on a
#     rename, and the dock/dash can stay pinned to the dead .desktop — so a
#     taskbar click launches a stale build (or nothing). Remove both, and
#     migrate any dash pin to the current entry.
STALE_BIN="$REPO/target/release/mtm"
STALE_DESKTOP="$APPS/termem.desktop"
[ -e "$STALE_BIN" ]     && { rm -f "$STALE_BIN";     log "removed stale binary  -> $STALE_BIN"; }
[ -e "$STALE_DESKTOP" ] && { rm -f "$STALE_DESKTOP"; log "removed stale launcher -> $STALE_DESKTOP"; }

if command -v dconf >/dev/null; then
    favs=$(dconf read /org/gnome/shell/favorite-apps 2>/dev/null || true)
    case "$favs" in
        *"'termem.desktop'"*)
            dconf write /org/gnome/shell/favorite-apps \
                "$(printf '%s' "$favs" | sed "s/'termem.desktop'/'termset.desktop'/")"
            log "repinned dash: termem.desktop -> termset.desktop" ;;
    esac
fi

# 6. Refresh caches so the change shows up without a re-login.
gtk-update-icon-cache -f -t "$ICON_BASE" >/dev/null 2>&1 || true
update-desktop-database "$APPS"          >/dev/null 2>&1 || true
# Mark the launcher trusted on GNOME (silences the "Allow Launching" prompt).
command -v gio >/dev/null && gio set "$DESKTOP" metadata::trusted true >/dev/null 2>&1 || true

echo
echo "Done. 'termset' is now in your app grid / dock, opening $WORKSPACE."
echo "If the icon doesn't refresh immediately, log out and back in."
