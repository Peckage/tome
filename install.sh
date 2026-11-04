#!/bin/sh
# Install Tome for the current user, with an entry in the application menu.
#
#   ./install.sh            install (no root needed)
#   ./install.sh --remove   uninstall
#
# Works from a release download (uses the `tome` next to this script) and
# from a source checkout (builds it with cargo first). It writes:
#   ~/.local/bin/tome
#   ~/.local/share/applications/io.github.peckage.Tome.desktop
#   ~/.local/share/icons/hicolor/scalable/apps/io.github.peckage.Tome.svg
# and refreshes the menu cache. Set PREFIX to use another place
# than ~/.local (for example PREFIX=/usr/local with sudo).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
prefix="${PREFIX:-$HOME/.local}"
if [ -n "${PREFIX:-}" ]; then
    share="$prefix/share"
else
    share="${XDG_DATA_HOME:-$HOME/.local/share}"
fi
bin="$prefix/bin/tome"
apps="$share/applications"
entry="$apps/io.github.peckage.Tome.desktop"
icon="$share/icons/hicolor/scalable/apps/io.github.peckage.Tome.svg"

refresh() {
    command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
    # Only refresh an icon cache that exists: creating one would hide icons
    # that other programs add there later without refreshing it.
    if [ -f "$share/icons/hicolor/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null; then
        gtk-update-icon-cache -q -t "$share/icons/hicolor" 2>/dev/null || true
    fi
}

if [ "${1:-}" = "--remove" ]; then
    rm -f "$bin" "$entry" "$icon" "$apps/io.github.xml2json.Converter.desktop"
    refresh
    echo "Removed Tome."
    exit 0
fi

# The program: next to this script in a release download, else build it.
if [ -f "$here/tome" ] && [ ! -f "$here/Cargo.toml" ]; then
    program="$here/tome"
    svg="$here/tome.svg"
else
    cargo build --release --manifest-path "$here/Cargo.toml"
    program="$here/target/release/tome"
    svg="$here/data/tome.svg"
fi

mkdir -p "$(dirname "$bin")" "$apps" "$(dirname "$icon")"
# Copy to a new name and rename, so a running Tome is not overwritten.
cp "$program" "$bin.new"
chmod 755 "$bin.new"
mv "$bin.new" "$bin"
cp "$svg" "$icon"
rm -f "$apps/io.github.xml2json.Converter.desktop"

cat > "$entry" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Tome
GenericName=Text Viewer
Comment=Open, search, edit and convert huge text files
Exec="$bin" %f
Icon=io.github.peckage.Tome
Terminal=false
Categories=Utility;TextTools;
MimeType=text/plain;text/x-log;text/csv;application/xml;text/xml;application/json;application/x-ndjson;
Keywords=text;editor;viewer;log;large;xml;json;csv;convert;
StartupWMClass=io.github.peckage.Tome
DESKTOP

command -v desktop-file-validate >/dev/null && desktop-file-validate "$entry"
refresh
echo "Installed $bin"
echo "Tome is in the application menu, and under \"Open with\" for text, XML and JSON files."
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "Note: $prefix/bin is not on your PATH. Add it to use \`tome\` in a terminal." ;;
esac
