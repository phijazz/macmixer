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
- **Virtual devices**: with "Virtual device" ticked, an output channel is also published
  as an audio device named "MacMixer: <output name>" that any other application can
  select as its input. This uses MacMixer's own small audio driver; the devices exist
  only while MacMixer is running. They are stereo, 44.1 to 192 kHz: the rate can be set
  in MacMixer's settings, in Audio MIDI Setup or by the application using the device.
- **Coupled devices**: each virtual device is also published combined with the system
  output device, as "MacMixer: <output name> + <system output>" (an aggregate device).
  Applications that can only open one audio device (DAWs, plug-in hosts) use that one:
  the mixer signal arrives on its last two inputs, the speakers are its first outputs.
  Can be turned off in the settings.
- **Settings**: latency (device buffer size), virtual device sample rate and peak
  auto-reset time.
- Channels grow with the window height.
- The whole setup is saved automatically to
  `~/Library/Application Support/MacMixer/config.json`.

## Requirements

- macOS 14.2 or later (process taps).
- A Rust toolchain (`cargo`) and the Xcode command line tools. `install.sh` offers to
  install them if they are missing.

## Install

```sh
./install.sh
```

This builds a release binary, wraps it in `MacMixer.app`, ad-hoc signs it and copies it
to `/Applications`. It then offers to install the virtual audio driver into
`/Library/Audio/Plug-Ins/HAL` (administrator password required; the system audio
service restarts, so sound stops for a moment in all applications). The driver is
optional: without it everything works except the "Virtual device" option. On first use macOS asks for permission to use the microphone
(device inputs) and to capture audio from other applications.

To build the bundle without installing: `./bundle.sh` (result in `target/MacMixer.app`).
Run the app from the bundle rather than with `cargo run`: the permission prompts are
attached to the bundle.

To uninstall the application and the driver: `./uninstall.sh`.

## Layout

- `src/` — the application (`engine.rs` mixing and device I/O, `tap.rs` application
  capture, `vdev.rs` driver client, `main.rs` user interface).
- `driver/` — the virtual audio driver, a CoreAudio AudioServerPlugIn
  (`cargo test -p macmixer-driver` runs its tests).

## Not there yet

- MIDI control, pre-fader metering.
- Virtual devices are stereo only.

## License

BSD 2-Clause, see [LICENSE](LICENSE).
