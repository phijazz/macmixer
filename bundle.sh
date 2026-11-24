#!/bin/sh
# Build target/MacMixer.app (the bundle carries the microphone and audio-capture
# permissions) and target/MacMixer.driver (the virtual audio driver).
set -e
cd "$(dirname "$0")"
cargo build --release --workspace

APP=target/MacMixer.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp Info.plist "$APP/Contents/Info.plist"
cp assets/MacMixer.icns "$APP/Contents/Resources/MacMixer.icns"
cp target/release/macmixer "$APP/Contents/MacOS/macmixer"
codesign --force --sign - "$APP"
echo "OK : $APP"

DRIVER=target/MacMixer.driver
rm -rf "$DRIVER"
mkdir -p "$DRIVER/Contents/MacOS"
cp driver/Info.plist "$DRIVER/Contents/Info.plist"
cp target/release/libmacmixer_driver.dylib "$DRIVER/Contents/MacOS/MacMixerDriver"
codesign --force --sign - "$DRIVER"
echo "OK : $DRIVER"
