# MacMixer

An audio mixer for macOS in the spirit of [jack_mixer](https://rdio.space/jackmixer/),
written in Rust and built directly on CoreAudio — no JACK and no third-party virtual
audio driver required.

## Features

- **Input channels** with fader, balance, mute, solo, stereo meter with peak hold and a
  numeric peak value (click to reset, optional auto-reset).
- **Sources chosen from a menu**:
  - any CoreAudio input device (stereo pair or single mono channel);
  - any running application, captured directly with CoreAudio process taps.
    "Exclusive" (on by default) silences the app's own output so it is only heard
    through the mixer.
- **Output channels**, each with its own output device, fader, meter and mute.
  Every input channel can be sent to any combination of outputs.
- **Settings**: latency (device buffer size) and peak auto-reset time.
- The whole setup is saved automatically to
  `~/Library/Application Support/MacMixer/config.json`.

## Requirements

- macOS 14.2 or later (process taps).
- A Rust toolchain (`cargo`) and the Xcode command line tools.

## Install

```sh
./install.sh
```

This builds a release binary, wraps it in `MacMixer.app`, ad-hoc signs it and copies it
to `/Applications`. On first use macOS asks for permission to use the microphone
(device inputs) and to capture audio from other applications.

To build the bundle without installing: `./bundle.sh` (result in `target/MacMixer.app`).
Run the app from the bundle rather than with `cargo run`: the permission prompts are
attached to the bundle.

To uninstall, delete `/Applications/MacMixer.app` and
`~/Library/Application Support/MacMixer`.

## Not there yet

- Sending an output channel to the *input* of another application (needs a virtual
  audio device shipped with MacMixer).
- MIDI control, pre-fader metering.
