//! Cœur du mixeur : paramètres partagés, bus de sortie, mixage temps réel, E/S (cpal).
//!
//! Chaque bus a son propre périphérique de sortie, donc sa propre horloge : une source
//! écrit dans un tampon circulaire par bus (`Fanout`), et chaque bus lit le sien.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const R: Ordering = Ordering::Relaxed;

/// Taille de tampon demandée aux périphériques (réglage « Latence »), en trames.
pub static BUFFER_FRAMES: AtomicU32 = AtomicU32::new(256);
/// Vrai si au moins une tranche est en solo (calculé par l'UI).
pub static ANY_SOLO: AtomicBool = AtomicBool::new(false);

pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(R))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), R)
    }
    /// Valable pour des valeurs >= 0 (l'ordre des bits suit l'ordre des flottants).
    pub fn raise(&self, v: f32) {
        self.0.fetch_max(v.to_bits(), R);
    }
    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, R))
    }
}

/// Paramètres d'une tranche ou d'un bus, écrits par l'UI, lus par les threads audio.
pub struct StripParams {
    pub gain: AtomicF32,
    pub pan: AtomicF32,
    pub mute: AtomicBool,
    pub solo: AtomicBool,
    /// Masque des bus vers lesquels la tranche envoie (bit i = bus d'indice i).
    pub sends: AtomicU32,
    pub peak_l: AtomicF32,
    pub peak_r: AtomicF32,
}

impl StripParams {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            gain: AtomicF32::new(1.0),
            pan: AtomicF32::new(0.0),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            sends: AtomicU32::new(0),
            peak_l: AtomicF32::new(0.0),
            peak_r: AtomicF32::new(0.0),
        })
    }
}

/// Lecteur d'une source : tampon circulaire stéréo entrelacé + rééchantillonnage linéaire.
pub struct Input {
    consumer: rtrb::Consumer<f32>,
    src_rate: f64,
    phase: f64,
    prev: [f32; 2],
    next: [f32; 2],
    /// Tampon vidé : on attend qu'il se regarnisse avant de relire.
    starved: bool,
    /// Remplissage minimal observé sur la fenêtre courante (trames), pour borner la latence.
    min_fill: usize,
    window: usize,
}

impl Input {
    fn new(consumer: rtrb::Consumer<f32>, src_rate: u32) -> Self {
        Self {
            consumer,
            src_rate: src_rate as f64,
            phase: 0.0,
            prev: [0.0; 2],
            next: [0.0; 2],
            starved: true,
            min_fill: usize::MAX,
            window: 0,
        }
    }

    /// À appeler une fois par bloc de sortie, avant de lire `needed` trames source.
    fn prepare(&mut self, needed: usize) {
        let target = BUFFER_FRAMES.load(R) as usize;
        let fill = self.consumer.slots() / 2;
        if self.starved {
            if fill >= target + needed {
                self.starved = false;
            }
            return;
        }
        // Les horloges dérivent : si même au plus bas le tampon reste trop plein sur ~1 s,
        // on jette l'excédent pour revenir à la latence visée.
        self.min_fill = self.min_fill.min(fill.saturating_sub(needed));
        self.window += needed;
        if self.window as f64 >= self.src_rate {
            if self.min_fill > target * 2 {
                for _ in 0..(self.min_fill - target) * 2 {
                    let _ = self.consumer.pop();
                }
            }
            self.min_fill = usize::MAX;
            self.window = 0;
        }
    }

    fn pop_frame(&mut self) -> [f32; 2] {
        if self.starved {
            return [0.0; 2];
        }
        if self.consumer.slots() >= 2 {
            let l = self.consumer.pop().unwrap_or(0.0);
            let r = self.consumer.pop().unwrap_or(0.0);
            [l, r]
        } else {
            self.starved = true;
            [0.0; 2]
        }
    }

    #[inline]
    fn frame(&mut self, step: f64) -> [f32; 2] {
        self.phase += step;
        while self.phase >= 1.0 {
            self.prev = self.next;
            self.next = self.pop_frame();
            self.phase -= 1.0;
        }
        let t = self.phase as f32;
        [
            self.prev[0] + (self.next[0] - self.prev[0]) * t,
            self.prev[1] + (self.next[1] - self.prev[1]) * t,
        ]
    }
}

/// Côté écriture d'une source : une copie du signal par bus.
pub struct Fanout(Vec<rtrb::Producer<f32>>);

impl Fanout {
    #[inline]
    pub fn push(&mut self, l: f32, r: f32) {
        for p in &mut self.0 {
            if p.slots() >= 2 {
                let _ = p.push(l);
                let _ = p.push(r);
            }
        }
    }
}

pub fn fanout(rate: u32, n: usize) -> (Fanout, Vec<Input>) {
    let mut prods = Vec::with_capacity(n);
    let mut inputs = Vec::with_capacity(n);
    for _ in 0..n {
        let (p, c) = rtrb::RingBuffer::new(rate as usize * 2); // 1 s stéréo
        prods.push(p);
        inputs.push(Input::new(c, rate));
    }
    (Fanout(prods), inputs)
}

pub struct BusInput {
    pub strip_id: u64,
    pub params: Arc<StripParams>,
    pub input: Input,
}

/// Un bus de sortie : somme les tranches qui lui sont envoyées, vers un périphérique.
pub struct Bus {
    pub params: Arc<StripParams>,
    pub index: AtomicUsize,
    pub inputs: Mutex<Vec<BusInput>>,
}

impl Bus {
    pub fn new(index: usize) -> Arc<Self> {
        Arc::new(Self {
            params: StripParams::new(),
            index: AtomicUsize::new(index),
            inputs: Mutex::new(Vec::new()),
        })
    }

    /// A second sink for the same output channel (its virtual device): same parameters.
    pub fn sharing(params: Arc<StripParams>, index: usize) -> Arc<Self> {
        Arc::new(Self { params, index: AtomicUsize::new(index), inputs: Mutex::new(Vec::new()) })
    }

    /// Appelé par le thread audio du périphérique de sortie.
    fn render(&self, data: &mut [f32], channels: usize, out_rate: f64) {
        data.fill(0.0);
        // try_lock : l'UI ne tient le verrou que brièvement (changement de source).
        let Ok(mut inputs) = self.inputs.try_lock() else { return };
        let frames = data.len() / channels;
        let any_solo = ANY_SOLO.load(R);
        let bit = 1u32 << self.index.load(R);

        for s in inputs.iter_mut() {
            let p = &s.params;
            let gain = p.gain.get();
            let pan = p.pan.get();
            let gl = gain * (1.0 - pan).min(1.0);
            let gr = gain * (1.0 + pan).min(1.0);
            let audible =
                p.sends.load(R) & bit != 0 && !p.mute.load(R) && (!any_solo || p.solo.load(R));
            let step = s.input.src_rate / out_rate;
            s.input.prepare((frames as f64 * step).ceil() as usize + 1);
            let (mut pk_l, mut pk_r) = (0.0f32, 0.0f32);
            for f in 0..frames {
                let [l, r] = s.input.frame(step);
                let (l, r) = (l * gl, r * gr);
                pk_l = pk_l.max(l.abs());
                pk_r = pk_r.max(r.abs());
                if audible {
                    if channels >= 2 {
                        data[f * channels] += l;
                        data[f * channels + 1] += r;
                    } else {
                        data[f] += (l + r) * 0.5;
                    }
                }
            }
            p.peak_l.raise(pk_l);
            p.peak_r.raise(pk_r);
        }

        let m = &self.params;
        let g = if m.mute.load(R) { 0.0 } else { m.gain.get() };
        let (mut pk_l, mut pk_r) = (0.0f32, 0.0f32);
        for f in 0..frames {
            let i = f * channels;
            data[i] *= g;
            pk_l = pk_l.max(data[i].abs());
            if channels >= 2 {
                data[i + 1] *= g;
                pk_r = pk_r.max(data[i + 1].abs());
            } else {
                pk_r = pk_l;
            }
        }
        m.peak_l.raise(pk_l);
        m.peak_r.raise(pk_r);
    }
}

fn dev_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|d| d.name().to_string())
}

/// (nom, nombre de canaux d'entrée)
pub fn input_devices() -> Vec<(String, u16)> {
    let host = cpal::default_host();
    let Ok(devs) = host.input_devices() else { return vec![] };
    devs.filter_map(|d| {
        let ch = d.default_input_config().ok()?.channels();
        Some((dev_name(&d)?, ch))
    })
    // MacMixer's own aggregates (virtual device + system output) are for other applications.
    .filter(|(n, _)| !(n.starts_with(VIRTUAL_PREFIX) && n.contains(" + ")))
    .collect()
}

pub fn output_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devs) = host.output_devices() else { return vec![] };
    // MacMixer's own virtual devices are driven by the "Virtual device" option instead.
    devs.filter_map(|d| dev_name(&d)).filter(|n| !n.starts_with(VIRTUAL_PREFIX)).collect()
}

/// Name prefix of the virtual devices published by the MacMixer driver.
pub const VIRTUAL_PREFIX: &str = "MacMixer: ";

/// Where an output channel plays.
pub enum Target<'a> {
    Default,
    Name(&'a str),
    /// CoreAudio device UID.
    Uid(&'a str),
}

/// Configurations à essayer : d'abord la taille de tampon demandée, sinon celle du pilote.
fn configs(cfg: &cpal::SupportedStreamConfig) -> [cpal::StreamConfig; 2] {
    let mut fixed = cfg.config();
    fixed.buffer_size = cpal::BufferSize::Fixed(BUFFER_FRAMES.load(R));
    [fixed, cfg.config()]
}

/// Ouvre une entrée de périphérique, copiée vers `n` bus ; `right == None` → mono dupliqué.
pub fn open_device_input(
    name: &str,
    left: u16,
    right: Option<u16>,
    n: usize,
) -> Result<(cpal::Stream, Vec<Input>), String> {
    let host = cpal::default_host();
    let dev = host
        .input_devices()
        .map_err(|e| e.to_string())?
        .find(|d| dev_name(d).as_deref() == Some(name))
        .ok_or_else(|| "device not found".to_string())?;
    let cfg = dev.default_input_config().map_err(|e| e.to_string())?;
    let nch = cfg.channels() as usize;
    let rate = cfg.sample_rate();
    let (l, r) = (left as usize, right.map(|c| c as usize));
    if l >= nch || r.is_some_and(|c| c >= nch) {
        return Err("no such channel".into());
    }
    let mut err = String::new();
    for c in configs(&cfg) {
        let (mut fan, inputs) = fanout(rate, n);
        let res = dev.build_input_stream(
            c,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                for fr in data.chunks_exact(nch) {
                    let a = fr[l];
                    fan.push(a, r.map_or(a, |c| fr[c]));
                }
            },
            |e| eprintln!("input: {e}"),
            None,
        );
        match res {
            Ok(stream) => {
                stream.play().map_err(|e| e.to_string())?;
                return Ok((stream, inputs));
            }
            Err(e) => err = e.to_string(),
        }
    }
    Err(err)
}

fn find_output(target: &Target) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let mut devs = host.output_devices().map_err(|e| e.to_string())?;
    match target {
        Target::Default => host.default_output_device().ok_or_else(|| "no audio output".to_string()),
        Target::Name(n) => {
            devs.find(|d| dev_name(d).as_deref() == Some(*n)).ok_or_else(|| "device not found".to_string())
        }
        Target::Uid(u) => {
            devs.find(|d| d.id().is_ok_and(|id| id.id() == *u)).ok_or_else(|| "device not found".to_string())
        }
    }
}

/// Current (device name, sample rate) of a target, to notice when either changes.
pub fn output_info(target: &Target) -> Option<(String, u32)> {
    let dev = find_output(target).ok()?;
    Some((dev_name(&dev)?, dev.default_output_config().ok()?.sample_rate()))
}

/// Opens an output sink. Returns the stream, the actual device name and its sample rate.
pub fn open_output(target: &Target, bus: Arc<Bus>) -> Result<(cpal::Stream, String, u32), String> {
    let dev = find_output(target)?;
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let ch = cfg.channels() as usize;
    let rate = cfg.sample_rate() as f64;
    let mut err = String::new();
    for c in configs(&cfg) {
        let bus = bus.clone();
        let res = dev.build_output_stream(
            c,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| bus.render(data, ch, rate),
            |e| eprintln!("output: {e}"),
            None,
        );
        match res {
            Ok(stream) => {
                stream.play().map_err(|e| e.to_string())?;
                return Ok((stream, dev_name(&dev).unwrap_or_default(), rate as u32));
            }
            Err(e) => err = e.to_string(),
        }
    }
    Err(err)
}
