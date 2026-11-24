#!/bin/sh
# Build MacMixer and install it into /Applications (or ~/Applications if not writable),
# then offer to install its virtual audio driver.
# Installs the build prerequisites first if they are missing.
set -e
cd "$(dirname "$0")"

# 1. Xcode command line tools (compiler and linker).
if ! xcode-select -p >/dev/null 2>&1; then
    echo "The Xcode command line tools are required."
    echo "A system dialog is opening: finish that installation, then run ./install.sh again."
    xcode-select --install || true
    exit 1
fi

# 2. Rust toolchain.
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
if ! command -v cargo >/dev/null 2>&1; then
    echo "Rust (cargo) is not installed."
    printf "Install it now with rustup, the official installer from https://rustup.rs? [y/N] "
    read -r answer
    case "$answer" in
        y|Y|yes|o|O|oui)
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
            . "$HOME/.cargo/env"
            ;;
        *)
            echo "Install Rust (https://rustup.rs or 'brew install rust'), then run ./install.sh again."
            exit 1
            ;;
    esac
fi

# 3. Build and install the application.
./bundle.sh
DEST=/Applications
[ -w "$DEST" ] || { DEST="$HOME/Applications"; mkdir -p "$DEST"; }
rm -rf "$DEST/MacMixer.app"
ditto target/MacMixer.app "$DEST/MacMixer.app"
echo "Installed: $DEST/MacMixer.app"

# 4. Virtual audio driver (optional): lets other applications record MacMixer outputs.
HAL=/Library/Audio/Plug-Ins/HAL
if cmp -s target/MacMixer.driver/Contents/MacOS/MacMixerDriver "$HAL/MacMixer.driver/Contents/MacOS/MacMixerDriver"; then
    echo "Virtual audio driver already up to date."
    exit 0
fi
echo
echo "The virtual audio driver lets other applications use a MacMixer output as their"
echo "audio input. Installing it needs your administrator password and restarts the"
echo "system audio service (sound stops for a second or two in all applications)."
printf "Install the virtual audio driver now? [Y/n] "
read -r answer
case "$answer" in
    n|N|no|non)
        echo "Driver not installed. Run ./install.sh again to install it later."
        ;;
    *)
        sudo sh -c "mkdir -p '$HAL' && rm -rf '$HAL/MacMixer.driver' && ditto target/MacMixer.driver '$HAL/MacMixer.driver' && chown -R root:wheel '$HAL/MacMixer.driver' && killall coreaudiod"
        echo "Installed: $HAL/MacMixer.driver"
        ;;
esac
