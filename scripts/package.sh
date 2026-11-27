#!/bin/sh
# Packages a release build: dist/clipnest-VERSION-x86_64-linux.tar.gz (+ .sha256).
# Usage: scripts/package.sh VERSION   (run after `cargo build --release`)
set -eu
cd "$(dirname "$0")/.."
VERSION="${1:?usage: scripts/package.sh VERSION}"
NAME="clipnest-$VERSION-x86_64-linux"
rm -rf "dist/$NAME" "dist/$NAME.tar.gz" "dist/$NAME.tar.gz.sha256"
mkdir -p "dist/$NAME"
install -m755 target/release/clipnest "dist/$NAME/clipnest"
strip "dist/$NAME/clipnest" 2>/dev/null || true
cp -r data install.sh uninstall.sh README.md LICENSE "dist/$NAME/"
tar -C dist -czf "dist/$NAME.tar.gz" --owner=0 --group=0 "$NAME"
(cd dist && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
echo "dist/$NAME.tar.gz"
