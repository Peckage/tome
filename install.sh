#!/bin/sh
# Build Tome and add it to the application menu for the current user.
#
#   ./install.sh            build, copy the binary to ./bin, write the menu entry
#   ./install.sh --remove   remove the menu entry again
#
# Outside the repository this writes the menu entry below and refreshes the
# MIME cache next to it (update-desktop-database rewrites mimeinfo.cache):
#   ~/.local/share/applications/io.github.peckage.Tome.desktop
# It also removes the entry of the old name, io.github.xml2json.Converter.
set -eu

repo=$(cd "$(dirname "$0")" && pwd)
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
entry="$apps/io.github.peckage.Tome.desktop"
old_entry="$apps/io.github.xml2json.Converter.desktop"

if [ "${1:-}" = "--remove" ]; then
    rm -f "$entry" "$old_entry"
    command -v update-desktop-database >/dev/null && update-desktop-database "$apps" || true
    echo "removed $entry"
    exit 0
fi

cargo build --release --manifest-path "$repo/Cargo.toml"
mkdir -p "$repo/bin" "$apps"
# Copy rather than link, so `cargo clean` does not break the menu entry.
cp "$repo/target/release/tome" "$repo/bin/tome.new"
mv "$repo/bin/tome.new" "$repo/bin/tome"
rm -f "$repo/bin/xml2json" "$old_entry"

cat > "$entry" <<EOF
[Desktop Entry]
Type=Application
Name=Tome
GenericName=Structured Text Viewer
Comment=View, search and convert huge XML and JSON files
Exec="$repo/bin/tome" %f
Icon=$repo/data/tome.svg
Terminal=false
Categories=Utility;TextTools;
MimeType=application/xml;text/xml;application/json;application/x-ndjson;
Keywords=xml;json;ndjson;csv;yaml;convert;viewer;
StartupWMClass=io.github.peckage.Tome
EOF

command -v desktop-file-validate >/dev/null && desktop-file-validate "$entry"
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" || true
echo "installed $entry"
