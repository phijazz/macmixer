#!/bin/sh
# Build target/MacMixer.app (the bundle carries the microphone and audio-capture permissions).
set -e
cd "$(dirname "$0")"
cargo build --release
APP=target/MacMixer.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp Info.plist "$APP/Contents/Info.plist"
cp target/release/macmixer "$APP/Contents/MacOS/macmixer"
codesign --force --sign - "$APP"
echo "OK : $APP"
