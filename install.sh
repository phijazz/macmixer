#!/bin/sh
# Build MacMixer and install it into /Applications (or ~/Applications if not writable).
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

# 3. Build and install.
./bundle.sh
DEST=/Applications
[ -w "$DEST" ] || { DEST="$HOME/Applications"; mkdir -p "$DEST"; }
rm -rf "$DEST/MacMixer.app"
ditto target/MacMixer.app "$DEST/MacMixer.app"
echo "Installed: $DEST/MacMixer.app"
