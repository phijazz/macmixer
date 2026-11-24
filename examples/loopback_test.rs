//! Plays a sine into a MacMixer virtual device and records it back from the same device.
//! Usage: cargo run --example loopback_test -- "MacMixer: Test"
//! (the device must exist: MacMixer running with "Virtual device" ticked on an output).
//! Run it from a terminal that has microphone permission, otherwise macOS hands back
//! silence for any input device.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "MacMixer: Test".into());
    let host = cpal::default_host();
    let is = |d: &cpal::Device| d.description().is_ok_and(|x| x.name() == name);
    let out = host.output_devices().unwrap().find(|d| is(d)).expect("output side not found");
    let inp = host.input_devices().unwrap().find(|d| is(d)).expect("input side not found");
    let ocfg = out.default_output_config().unwrap();
    let icfg = inp.default_input_config().unwrap();
    println!("output {} Hz {} ch, input {} Hz {} ch", ocfg.sample_rate(), ocfg.channels(), icfg.sample_rate(), icfg.channels());

    let och = ocfg.channels() as usize;
    let step = 440.0 * std::f32::consts::TAU / ocfg.sample_rate() as f32;
    let mut phase = 0f32;
    let ostream = out
        .build_output_stream(
            ocfg.config(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for fr in data.chunks_mut(och) {
                    phase = (phase + step) % std::f32::consts::TAU;
                    fr.fill(phase.sin() * 0.5);
                }
            },
            |e| eprintln!("out: {e}"),
            None,
        )
        .unwrap();

    // Sum of squares (scaled) and sample count of what comes back.
    let acc = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
    let acc2 = acc.clone();
    let istream = inp
        .build_input_stream(
            icfg.config(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let sq: f64 = data.iter().map(|x| (*x as f64) * (*x as f64)).sum();
                acc2.0.fetch_add((sq * 1e6) as u64, Ordering::Relaxed);
                acc2.1.fetch_add(data.len() as u64, Ordering::Relaxed);
            },
            |e| eprintln!("in: {e}"),
            None,
        )
        .unwrap();
    ostream.play().unwrap();
    istream.play().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (sq, n) = (acc.0.load(Ordering::Relaxed) as f64 / 1e6, acc.1.load(Ordering::Relaxed));
    let rms = if n > 0 { (sq / n as f64).sqrt() } else { 0.0 };
    // A 0.5-amplitude sine has an RMS of 0.354.
    println!("recorded {n} samples, RMS {rms:.3} (expected ~0.354)");
}
