#!/bin/sh
# Installs (or updates) rxcord for the current user on Linux:
#   curl -fsSL https://raw.githubusercontent.com/RexSAS/rxcord/master/installer/install.sh | sh
# The binary goes to ~/.local/bin (writable, so rxcord's self-update works), plus a menu entry.
set -eu
asset=rxcord-linux-x86_64
url="https://github.com/RexSAS/rxcord/releases/latest/download/$asset"
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

[ "$(uname -m)" = x86_64 ] || { echo "rxcord: only x86_64 builds are published" >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading rxcord…"
curl -fL --progress-bar -o "$tmp/$asset" "$url"
curl -fsSL -o "$tmp/$asset.sha256" "$url.sha256"
(cd "$tmp" && sha256sum -c --quiet "$asset.sha256") || { echo "rxcord: checksum mismatch, not installing" >&2; exit 1; }

mkdir -p "$bin" "$apps"
install -m 755 "$tmp/$asset" "$bin/rxcord"
cat > "$apps/rxcord.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=rxcord
Comment=Lightweight Discord client
Exec=$bin/rxcord
Terminal=false
Categories=Network;InstantMessaging;
DESKTOP

echo "Installed $bin/rxcord"
case ":$PATH:" in *":$bin:"*) ;; *) echo "Note: $bin is not in your PATH; start rxcord from the app menu or add it." ;; esac
