#!/bin/sh
# Build MacMixer and install it into /Applications (or ~/Applications if not writable).
set -e
cd "$(dirname "$0")"
./bundle.sh
DEST=/Applications
[ -w "$DEST" ] || { DEST="$HOME/Applications"; mkdir -p "$DEST"; }
rm -rf "$DEST/MacMixer.app"
ditto target/MacMixer.app "$DEST/MacMixer.app"
echo "Installed: $DEST/MacMixer.app"
