#!/bin/sh
# Builds Clipnest (or uses the prebuilt program in a release download) and installs it
# for the current user only (under ~/.local).
# Needs no root and changes no system files.
set -eu
cd "$(dirname "$0")"
PREFIX="${PREFIX:-$HOME/.local}"
APP_ID=io.github.Rong0715.Clipnest

if [ -x ./clipnest ] && [ ! -f Cargo.toml ]; then
    # A release download: the program is already built.
    BIN=./clipnest
else
    if ! command -v cargo >/dev/null 2>&1; then
        echo "cargo was not found. Install the build tools first, e.g. on Ubuntu/Debian:" >&2
        echo "  sudo apt install cargo build-essential pkg-config libgtk-4-dev libadwaita-1-dev" >&2
        exit 1
    fi
    cargo build --release
    BIN=target/release/clipnest
fi

install -Dm755 "$BIN" "$PREFIX/bin/clipnest"
install -Dm644 "data/icons/hicolor/scalable/apps/$APP_ID.svg" "$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg"
install -Dm644 "data/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg" "$PREFIX/share/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg"
mkdir -p "$PREFIX/share/applications"
sed "s|^Exec=clipnest|Exec=$PREFIX/bin/clipnest|" "data/$APP_ID.desktop" > "$PREFIX/share/applications/$APP_ID.desktop"
# Bump folder timestamps so GNOME reloads icons replaced in place.
touch "$PREFIX/share/icons/hicolor" "$PREFIX/share/icons/hicolor/scalable/apps" \
      "$PREFIX/share/icons/hicolor/symbolic/apps" "$PREFIX/share/applications"
# Only refresh an icon cache that already exists; never create one.
if [ -f "$PREFIX/share/icons/hicolor/icon-theme.cache" ]; then
    gtk4-update-icon-cache -q -t -f "$PREFIX/share/icons/hicolor" 2>/dev/null ||
        gtk-update-icon-cache -q -t -f "$PREFIX/share/icons/hicolor" 2>/dev/null || true
fi

echo
echo "Installed Clipnest to $PREFIX."
echo "Open “Clipnest” from your app grid, or run: $PREFIX/bin/clipnest"
