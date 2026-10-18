#!/bin/sh
# Install the Parley desktop client for the current user.
#
#   ./install.sh              install to ~/.local
#   PREFIX=/opt ./install.sh  install elsewhere (needs write permission)
#   ./install.sh --uninstall  remove it again
#
# No root, no system files. The desktop entry and icon go to the same
# PREFIX, so launchers find them.

set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PREFIX=${PREFIX:-"$HOME/.local"}
DEST="$PREFIX/opt/parley"
BIN="$PREFIX/bin/parley"
DESKTOP="$PREFIX/share/applications/io.github.dartlumix.Parley.desktop"
ICON="$PREFIX/share/icons/hicolor/scalable/apps/parley.svg"

remove_files() {
    rm -rf "$DEST"
    rm -f "$BIN" "$DESKTOP" "$ICON"
    echo "Parley removed from $PREFIX"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true
    fi
}

if [ "${1:-}" = "--uninstall" ]; then
    remove_files
    exit 0
fi

if [ ! -x "$HERE/parley" ] || [ ! -d "$HERE/lib" ]; then
    echo "install.sh must sit next to the parley binary and its lib/ directory." >&2
    echo "Extract the release tarball fully, then run it from there." >&2
    exit 1
fi

# Runtime dependencies. The bundle carries Flutter, the Rust bridge and
# the AEC (statically linked); the desktop still provides GTK, PipeWire
# (screen capture), ALSA (audio) and libstdc++.
if command -v ldd >/dev/null 2>&1; then
    missing=$(ldd "$HERE/parley" "$HERE/lib/libparley_flutter.so" 2>/dev/null |
        awk '/not found/ {print $1}' | sort -u)
    if [ -n "$missing" ]; then
        echo "Missing shared libraries:" >&2
        echo "$missing" | sed 's/^/  /' >&2
        echo >&2
        echo "Debian/Ubuntu: sudo apt install libgtk-3-0 libpipewire-0.3-0 libasound2 libstdc++6" >&2
        echo "Arch:          sudo pacman -S gtk3 pipewire alsa-lib" >&2
        echo "Fedora:        sudo dnf install gtk3 pipewire-libs alsa-lib" >&2
        exit 1
    fi
fi

mkdir -p "$PREFIX/opt" "$PREFIX/bin" "$(dirname "$DESKTOP")" "$(dirname "$ICON")"
rm -rf "$DEST"
mkdir -p "$DEST"
cp -R "$HERE"/. "$DEST"/
chmod +x "$DEST/parley" "$DEST/install.sh" 2>/dev/null || true
ln -sfn "$DEST/parley" "$BIN"
cp "$HERE/parley.svg" "$ICON"

# Absolute Exec: the desktop entry is generated at install time, so a
# non-default PREFIX works and the icon theme lookup stays simple.
cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=Parley
GenericName=Team voice
Comment=Self-hosted voice, chat and screen share
Exec=$DEST/parley
Icon=parley
Terminal=false
Categories=Network;Chat;AudioVideo;
Keywords=voice;chat;screen share;teamspeak;parley;
StartupNotify=true
StartupWMClass=io.github.dartlumix.Parley
EOF

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true
fi

echo "Parley installed:"
echo "  binary:  $BIN"
echo "  desktop: $DESKTOP"
case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "  note:    add $PREFIX/bin to PATH to run 'parley' from a shell" ;;
esac
