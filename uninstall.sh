#!/bin/sh
# Removes Clipnest from ~/.local. History and settings are kept unless you pass --purge.
set -eu
PREFIX="${PREFIX:-$HOME/.local}"
APP_ID=io.github.Rong0715.Clipnest

"$PREFIX/bin/clipnest" quit 2>/dev/null || true
rm -f "$PREFIX/bin/clipnest" \
      "$PREFIX/share/applications/$APP_ID.desktop" \
      "$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg" \
      "$PREFIX/share/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg" \
      "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/$APP_ID.desktop"
if [ "${1:-}" = "--purge" ]; then
    rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/clipnest" \
           "${XDG_CONFIG_HOME:-$HOME/.config}/clipnest" \
           "${XDG_STATE_HOME:-$HOME/.local/state}/clipnest"
    echo "Clipnest and its history were removed."
else
    echo "Clipnest was removed. Your history is still in ${XDG_DATA_HOME:-$HOME/.local/share}/clipnest (use --purge to delete it)."
fi
