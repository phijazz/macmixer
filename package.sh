#!/bin/sh
# Build the graphical installer target/MacMixer-<version>.pkg, which installs the
# application into /Applications and the virtual audio driver into
# /Library/Audio/Plug-Ins/HAL (asking for the administrator password itself).
set -e
cd "$(dirname "$0")"
./bundle.sh

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
WORK=target/pkg
rm -rf "$WORK"
mkdir -p "$WORK/root/Applications" "$WORK/root/Library/Audio/Plug-Ins/HAL" "$WORK/scripts" "$WORK/resources"
ditto target/MacMixer.app "$WORK/root/Applications/MacMixer.app"
ditto target/MacMixer.driver "$WORK/root/Library/Audio/Plug-Ins/HAL/MacMixer.driver"

cat > "$WORK/scripts/postinstall" <<'EOS'
#!/bin/sh
# Restart the system audio service so that it loads the new driver.
killall coreaudiod 2>/dev/null || true
exit 0
EOS
chmod +x "$WORK/scripts/postinstall"

cat > "$WORK/resources/welcome.html" <<'EOS'
<html><body style="font-family: -apple-system, Helvetica, sans-serif; font-size: 13px;">
<p><b>MacMixer</b> is an audio mixer for macOS: input channels from audio devices or
running applications, mixed into any number of output channels.</p>
<p>This installer puts:</p>
<ul>
<li>the application in <tt>/Applications</tt>;</li>
<li>the MacMixer virtual audio driver in <tt>/Library/Audio/Plug-Ins/HAL</tt>. It lets
other applications use a MacMixer output as their audio input.</li>
</ul>
<p>Quit MacMixer first if it is running. At the end of the installation the system
audio service restarts: sound stops for a second or two in all applications.</p>
<p>Requires macOS 14.2 or later on Apple silicon.</p>
</body></html>
EOS

# Install the bundles exactly where the package says, never over a copy found elsewhere.
pkgbuild --analyze --root "$WORK/root" "$WORK/components.plist" >/dev/null
i=0
while /usr/libexec/PlistBuddy -c "Print :$i:RootRelativeBundlePath" "$WORK/components.plist" >/dev/null 2>&1; do
    /usr/libexec/PlistBuddy -c "Delete :$i:BundleIsRelocatable" "$WORK/components.plist" 2>/dev/null || true
    /usr/libexec/PlistBuddy -c "Add :$i:BundleIsRelocatable bool false" "$WORK/components.plist"
    i=$((i + 1))
done

pkgbuild --root "$WORK/root" --component-plist "$WORK/components.plist" --scripts "$WORK/scripts" \
    --identifier org.macmixer.pkg --version "$VERSION" --install-location / \
    "$WORK/MacMixer-component.pkg" >/dev/null

productbuild --synthesize --package "$WORK/MacMixer-component.pkg" "$WORK/distribution.xml" >/dev/null
# Add a title, the welcome page and the supported architecture to the generated description.
sed -i '' -E 's|<options [^>]*/>|<title>MacMixer</title><welcome file="welcome.html"/><options customize="never" require-scripts="false" hostArchitectures="arm64"/>|' "$WORK/distribution.xml"
productbuild --distribution "$WORK/distribution.xml" --package-path "$WORK" --resources "$WORK/resources" \
    "target/MacMixer-$VERSION.pkg" >/dev/null
echo "OK : target/MacMixer-$VERSION.pkg"
