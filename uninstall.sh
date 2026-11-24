#!/bin/sh
# Remove MacMixer, its virtual audio driver and (optionally) its settings.
HAL=/Library/Audio/Plug-Ins/HAL
rm -rf /Applications/MacMixer.app "$HOME/Applications/MacMixer.app"
echo "Removed the application."
if [ -d "$HAL/MacMixer.driver" ]; then
    echo "Removing the virtual audio driver (administrator password required)."
    sudo sh -c "rm -rf '$HAL/MacMixer.driver' && killall coreaudiod" && echo "Removed the driver."
fi
printf "Also delete the saved mixer setup? [y/N] "
read -r answer
case "$answer" in
    y|Y|yes|o|O|oui) rm -rf "$HOME/Library/Application Support/MacMixer"; echo "Removed the settings." ;;
esac
