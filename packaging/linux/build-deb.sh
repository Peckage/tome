#!/bin/sh
# Build a .deb from a compiled binary:
#   packaging/linux/build-deb.sh target/release/tome 0.1.0 dist/
# Needs only dpkg-deb. Installs /usr/bin/tome, the menu entry and the icon.
set -eu

binary=$1
version=$2
out=$3
repo=$(cd "$(dirname "$0")/../.." && pwd)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
chmod 755 "$root"

install -Dm755 "$binary" "$root/usr/bin/tome"
install -Dm644 "$repo/packaging/linux/io.github.peckage.Tome.desktop" "$root/usr/share/applications/io.github.peckage.Tome.desktop"
install -Dm644 "$repo/data/tome.svg" "$root/usr/share/icons/hicolor/scalable/apps/io.github.peckage.Tome.svg"
install -Dm644 "$repo/LICENSE" "$root/usr/share/doc/tome/copyright"
install -Dm644 "$repo/README.md" "$root/usr/share/doc/tome/README.md"

size=$(du -sk "$root/usr" | cut -f1)
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: tome
Version: $version
Architecture: amd64
Maintainer: Peckage <https://github.com/Peckage/tome>
Installed-Size: $size
Depends: libc6 (>= 2.35), libgcc-s1, libgl1, libegl1, libxkbcommon0
Recommends: libwayland-client0, libwayland-egl1, libx11-6, libxcursor1, libxi6, libxrandr2, libxkbcommon-x11-0, xdg-desktop-portal
Section: editors
Priority: optional
Homepage: https://github.com/Peckage/tome
Description: Text viewer, editor and converter built for huge files
 Tome opens a 500 MB file in a fraction of a second, scrolls and searches
 millions of lines without loading them, edits them, and converts XML and
 JSON to JSON, NDJSON, YAML, XML, CSV or TSV with a live preview.
CONTROL

mkdir -p "$out"
deb="$out/tome_${version}_amd64.deb"
dpkg-deb --root-owner-group --build "$root" "$deb" >/dev/null
echo "$deb"
