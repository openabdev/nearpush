#!/bin/sh
# Install (or reinstall) the nearpushd LaunchAgent for the current user.
# Expects ~/.local/bin/nearpushd to exist (build on the build host, scp it over).
set -eu

label=dev.nearpush.daemon
here=$(cd "$(dirname "$0")" && pwd)
plist="$HOME/Library/LaunchAgents/$label.plist"
domain="gui/$(id -u)"

[ -x "$HOME/.local/bin/nearpushd" ] || { echo "missing $HOME/.local/bin/nearpushd" >&2; exit 1; }

mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/nearpush"
sed "s|@HOME@|$HOME|g" "$here/$label.plist.in" > "$plist"
plutil -lint "$plist"

if launchctl print "$domain/$label" >/dev/null 2>&1; then
  launchctl bootout "$domain/$label" || true
fi
launchctl bootstrap "$domain" "$plist"
launchctl print "$domain/$label" | grep -E 'state =|pid =' || true
