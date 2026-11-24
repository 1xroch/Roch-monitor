#!/bin/sh
# Installs Roch Monitor for the current user: the binary to ~/.local/bin/rochmonitor, the launcher
# entry and its icon. Nothing runs as root here; the monitor runs as your normal user and asks for
# administrator access (pkexec) only when you choose Voltages > Read board voltages. Works from the
# release tarball (everything beside this script) and from the repository after
# `cargo build --release --locked -p roch-monitor`; or pass the binary.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
if [ -x "$here/rochmonitor" ]; then default_bin="$here/rochmonitor"; else default_bin="$root/target/release/RochMonitor"; fi
if [ -f "$here/RochMonitor.png" ]; then icon="$here/RochMonitor.png"; else icon="$root/assets/icon.png"; fi
bin=${1:-"$default_bin"}
data=${XDG_DATA_HOME:-"$HOME/.local/share"}
install -Dm755 "$bin" "$HOME/.local/bin/rochmonitor"
sed "s|^Exec=.*|Exec=\"$HOME/.local/bin/rochmonitor\"|" "$here/RochMonitor.desktop" > "$here/.RochMonitor.desktop.tmp"
install -Dm644 "$here/.RochMonitor.desktop.tmp" "$data/applications/RochMonitor.desktop"
rm -f "$here/.RochMonitor.desktop.tmp"
install -Dm644 "$icon" "$data/icons/hicolor/256x256/apps/RochMonitor.png"
command -v update-desktop-database >/dev/null && update-desktop-database "$data/applications" || true
echo "Installed: $HOME/.local/bin/rochmonitor (launcher: Roch Monitor)"
if [ -e /usr/local/bin/rochmonitor ]; then
  echo "Run 'sudo $HOME/.local/bin/rochmonitor setup' again so the password-free helper matches this version."
fi
