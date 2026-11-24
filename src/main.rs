//! MacMixer — un mixeur façon jack_mixer pour macOS, sur CoreAudio.

mod aggregate;
mod engine;
mod tap;
mod vdev;

use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use engine::{Bus, BusInput, StripParams, Target, ANY_SOLO, BUFFER_FRAMES, VIRTUAL_PREFIX};
use serde::{Deserialize, Serialize};

const MIN_DB: f32 = -70.0;
const MAX_DB: f32 = 6.0;
const BUFFER_CHOICES: [u32; 7] = [32, 64, 128, 256, 512, 1024, 2048];
const MAX_BUSES: usize = 32;
const VDEV_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
/// Window height minus fader height: top bar, plus what a channel shows around its fader.
const STRIP_OVERHEAD: f32 = 242.0;
/// Inner width of a channel.
const STRIP_W: f32 = 78.0;

#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
enum SourceCfg {
    #[default]
    None,
    Device {
        name: String,
        left: u16,
        right: Option<u16>,
    },
    App {
        bundle_id: String,
        name: String,
    },
}

impl SourceCfg {
    fn label(&self) -> String {
        match self {
            SourceCfg::None => "Source…".into(),
            SourceCfg::Device { name, left, right: Some(r) } => format!("{name} {}-{}", left + 1, r + 1),
            SourceCfg::Device { name, left, right: None } => format!("{name} {}", left + 1),
            SourceCfg::App { name, .. } => name.clone(),
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
struct StripCfg {
    name: String,
    source: SourceCfg,
    db: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    /// Envoi vers chaque bus, dans l'ordre des bus.
    #[serde(default)]
    sends: Vec<bool>,
    /// Source application : couper sa sortie directe (elle ne s'entend que via le mixeur).
    #[serde(default = "yes")]
    mute_direct: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct BusCfg {
    name: String,
    /// `None` : sortie par défaut du système.
    device: Option<String>,
    db: f32,
    mute: bool,
    /// No hardware output at all (virtual device only).
    #[serde(default)]
    hw_off: bool,
    /// Slot of this output's virtual device in the MacMixer driver, if enabled.
    #[serde(default)]
    vslot: Option<usize>,
}

fn default_vdev_rate() -> u32 {
    48_000
}

fn five() -> f32 {
    5.0
}

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    #[serde(default)]
    buffer: u32,
    /// Automatically forget the numeric peak values after `peak_secs` seconds.
    #[serde(default = "yes")]
    peak_auto: bool,
    #[serde(default = "five")]
    peak_secs: f32,
    /// Sample rate given to a virtual device when it is created.
    #[serde(default = "default_vdev_rate")]
    vdev_rate: u32,
    /// Also publish each virtual device coupled with the system output (aggregate device).
    #[serde(default = "yes")]
    couple: bool,
    #[serde(default)]
    buses: Vec<BusCfg>,
    #[serde(default)]
    strips: Vec<StripCfg>,
}

impl Default for Config {
    fn default() -> Self {
        Self { buffer: 0, peak_auto: true, peak_secs: 5.0, vdev_rate: 48_000, couple: true, buses: vec![], strips: vec![] }
    }
}

/// The driver's plug-in object, unless MACMIXER_NO_DRIVER is set (used for UI tests).
fn find_driver() -> Option<u32> {
    if std::env::var_os("MACMIXER_NO_DRIVER").is_some() { None } else { vdev::plugin() }
}

fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MACMIXER_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Library/Application Support/MacMixer/config.json")
}

fn db_to_gain(db: f32) -> f32 {
    if db <= MIN_DB + 0.05 { 0.0 } else { 10f32.powf(db / 20.0) }
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

enum Handle {
    Device(#[allow(dead_code)] cpal::Stream),
    Tap(tap::TapSource),
}

struct Strip {
    id: u64,
    cfg: StripCfg,
    params: Arc<StripParams>,
    handle: Option<Handle>,
    status: String,
    meter: Meter,
}

struct BusUi {
    cfg: BusCfg,
    rt: Arc<Bus>,
    stream: Option<cpal::Stream>,
    dev_name: String,
    err: String,
    /// Second sink feeding this output's virtual device.
    vrt: Option<Arc<Bus>>,
    vstream: Option<cpal::Stream>,
    verr: String,
    /// Aggregate "system output + virtual device", for single-device applications.
    agg: Option<Aggregate>,
    /// Sample rates the two streams were opened at.
    rate: u32,
    vrate: u32,
    /// The preferred rate has been given to the virtual device (done once; after that
    /// MacMixer follows whatever rate the device is set to).
    vrate_set: bool,
    meter: Meter,
}

struct Aggregate {
    id: u32,
    /// (hardware device UID, virtual device name) it was built from.
    key: (String, String),
    name: String,
    hw_name: String,
}

fn virtual_name(cfg: &BusCfg) -> String {
    let name = cfg.name.replace(['\n', '\r'], " ");
    let name = if name.trim().is_empty() { "Output" } else { name.trim() };
    format!("{VIRTUAL_PREFIX}{name}")
}

enum Action {
    AddStrip,
    AddBus,
    RemoveStrip(usize),
    Reconnect(usize),
    RemoveBus(usize),
    ReopenBus(usize),
    ToggleVirtual(usize),
    VirtualRate(u32),
    Latency(u32),
}

struct App {
    strips: Vec<Strip>,
    buses: Vec<BusUi>,
    in_devs: Vec<(String, u16)>,
    out_devs: Vec<String>,
    apps: Vec<tap::AppInfo>,
    peak_auto: bool,
    peak_secs: f32,
    /// The virtual audio driver's plug-in object, when the driver is installed.
    driver: Option<u32>,
    vdev_rate: u32,
    couple: bool,
    /// MACMIXER_SCREENSHOT=<file.bmp>: save a picture of the window, then quit (UI tests).
    shot: Option<String>,
    shot_state: Arc<std::sync::atomic::AtomicU8>,
    started: Instant,
    last_scan: Instant,
    last_save: Instant,
    saved: String,
    next_id: u64,
}

impl App {
    fn new() -> Self {
        let mut cfg: Config = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if BUFFER_CHOICES.contains(&cfg.buffer) {
            BUFFER_FRAMES.store(cfg.buffer, Relaxed);
        }
        if cfg.buses.is_empty() {
            cfg.buses.push(BusCfg { name: "Main".into(), device: None, db: 0.0, mute: false, hw_off: false, vslot: None });
        }
        cfg.buses.truncate(MAX_BUSES);
        let mut app = Self {
            strips: vec![],
            buses: vec![],
            in_devs: engine::input_devices(),
            out_devs: engine::output_devices(),
            apps: tap::list_apps(),
            peak_auto: cfg.peak_auto,
            peak_secs: cfg.peak_secs.clamp(0.5, 60.0),
            driver: find_driver(),
            vdev_rate: if VDEV_RATES.contains(&cfg.vdev_rate) { cfg.vdev_rate } else { 48_000 },
            couple: cfg.couple,
            shot: std::env::var("MACMIXER_SCREENSHOT").ok(),
            shot_state: Arc::default(),
            started: Instant::now(),
            last_scan: Instant::now(),
            last_save: Instant::now(),
            saved: String::new(),
            next_id: 1,
        };
        if app.driver.is_some() {
            aggregate::destroy_stale();
        }
        for b in cfg.buses {
            app.add_bus(b);
        }
        for mut s in cfg.strips {
            if s.sends.is_empty() {
                s.sends.push(true);
            }
            app.add_strip(s);
        }
        app.saved = app.serialize();
        app
    }

    fn serialize(&self) -> String {
        let cfg = Config {
            buffer: BUFFER_FRAMES.load(Relaxed),
            peak_auto: self.peak_auto,
            peak_secs: self.peak_secs,
            vdev_rate: self.vdev_rate,
            couple: self.couple,
            buses: self.buses.iter().map(|b| b.cfg.clone()).collect(),
            strips: self.strips.iter().map(|s| s.cfg.clone()).collect(),
        };
        serde_json::to_string_pretty(&cfg).unwrap_or_default()
    }

    fn save_if_changed(&mut self) {
        let s = self.serialize();
        if s != self.saved {
            let path = config_path();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if std::fs::write(&path, &s).is_ok() {
                self.saved = s;
            }
        }
    }

    fn open_bus_output(&mut self, j: usize) {
        let b = &mut self.buses[j];
        b.stream = None;
        b.err.clear();
        if b.cfg.hw_off {
            b.dev_name.clear();
            return;
        }
        let target = match &b.cfg.device {
            Some(name) => Target::Name(name),
            None => Target::Default,
        };
        match engine::open_output(&target, b.rt.clone()) {
            Ok((stream, name, rate)) => {
                b.stream = Some(stream);
                b.dev_name = name;
                b.rate = rate;
            }
            Err(e) => {
                b.dev_name = b.cfg.device.clone().unwrap_or_default();
                b.err = e;
            }
        }
    }

    /// Starts playing an output channel into its virtual device.
    fn open_virtual(&mut self, j: usize) {
        let b = &mut self.buses[j];
        b.vstream = None;
        b.verr.clear();
        let (Some(slot), Some(vrt)) = (b.cfg.vslot, b.vrt.clone()) else { return };
        if self.driver.is_none() {
            b.verr = "driver not installed".into();
            return;
        }
        // The device appears a moment after it is requested; all of this is retried periodically.
        let Some(current) = vdev::get_rate(slot) else {
            b.verr = "virtual device starting…".into();
            return;
        };
        if !b.vrate_set {
            b.vrate_set = true;
            if current != self.vdev_rate {
                // Takes effect asynchronously; the stream is opened on the next retry.
                vdev::set_rate(slot, self.vdev_rate);
                b.verr = "virtual device starting…".into();
                return;
            }
        }
        match engine::open_output(&Target::Uid(&vdev::uid(slot)), vrt) {
            Ok((stream, _, rate)) => {
                b.vstream = Some(stream);
                b.vrate = rate;
            }
            Err(_) => b.verr = "virtual device starting…".into(),
        }
    }

    /// Tells the driver which virtual devices should exist (one per enabled output).
    fn sync_virtual_devices(&mut self) {
        let Some(plugin) = self.driver else { return };
        let mut lines = vec![String::new(); vdev::MAX_DEVICES];
        for b in &self.buses {
            if let Some(slot) = b.cfg.vslot.filter(|&s| s < vdev::MAX_DEVICES) {
                lines[slot] = virtual_name(&b.cfg);
            }
        }
        let text = lines.join("\n");
        if vdev::get_names(plugin).as_deref() != Some(&text) {
            vdev::set_names(plugin, &text);
        }
    }

    /// Keeps one aggregate "system output + virtual device" per virtual device, rebuilt
    /// when the system output or the name changes.
    fn sync_aggregates(&mut self) {
        let default = if self.couple && self.driver.is_some() { aggregate::default_output() } else { None };
        for b in &mut self.buses {
            let desired = match (&default, b.cfg.vslot) {
                // Only once the virtual device really exists.
                (Some(d), Some(slot)) if vdev::get_rate(slot).is_some() => {
                    Some((slot, (d.uid.clone(), virtual_name(&b.cfg)), d.name.clone()))
                }
                _ => None,
            };
            if b.agg.as_ref().map(|a| &a.key) == desired.as_ref().map(|d| &d.1) {
                continue;
            }
            if let Some(a) = b.agg.take() {
                aggregate::destroy(a.id);
            }
            if let Some((slot, key, hw_name)) = desired {
                let name = format!("{} + {}", key.1, hw_name);
                match aggregate::create(slot, &name, &key.0) {
                    Ok(id) => b.agg = Some(Aggregate { id, key, name, hw_name }),
                    Err(e) => b.verr = e,
                }
            }
        }
    }

    fn toggle_virtual(&mut self, j: usize) {
        if self.buses[j].cfg.vslot.is_some() {
            let b = &mut self.buses[j];
            if let Some(a) = b.agg.take() {
                aggregate::destroy(a.id);
            }
            b.cfg.vslot = None;
            b.vrt = None;
            b.vstream = None;
            b.verr.clear();
        } else {
            let used: Vec<usize> = self.buses.iter().filter_map(|b| b.cfg.vslot).collect();
            let Some(slot) = (0..vdev::MAX_DEVICES).find(|s| !used.contains(s)) else { return };
            let b = &mut self.buses[j];
            b.cfg.vslot = Some(slot);
            b.vrate_set = false;
            b.vrt = Some(Bus::sharing(b.rt.params.clone(), j));
        }
        self.sync_virtual_devices();
        self.open_virtual(j);
        self.reconnect_all();
    }

    fn add_bus(&mut self, cfg: BusCfg) {
        if self.buses.len() >= MAX_BUSES {
            return;
        }
        let j = self.buses.len();
        let rt = Bus::new(j);
        let vrt = cfg.vslot.map(|_| Bus::sharing(rt.params.clone(), j));
        self.buses.push(BusUi {
            cfg,
            rt,
            stream: None,
            dev_name: String::new(),
            err: String::new(),
            vrt,
            vstream: None,
            verr: String::new(),
            agg: None,
            rate: 0,
            vrate: 0,
            vrate_set: false,
            meter: Meter::default(),
        });
        self.sync_virtual_devices();
        self.open_bus_output(j);
        self.open_virtual(j);
        self.reconnect_all();
    }

    fn remove_bus(&mut self, j: usize) {
        if let Some(a) = self.buses.remove(j).agg {
            aggregate::destroy(a.id);
        }
        for (k, b) in self.buses.iter().enumerate() {
            b.rt.index.store(k, Relaxed);
            if let Some(v) = &b.vrt {
                v.index.store(k, Relaxed);
            }
        }
        for s in &mut self.strips {
            if j < s.cfg.sends.len() {
                s.cfg.sends.remove(j);
            }
        }
        self.sync_virtual_devices();
        self.reconnect_all();
    }

    /// Every running sink: the hardware output and/or the virtual device of each output.
    fn sinks(&self) -> Vec<Arc<Bus>> {
        self.buses
            .iter()
            .flat_map(|b| (!b.cfg.hw_off).then(|| b.rt.clone()).into_iter().chain(b.vrt.clone()))
            .collect()
    }

    fn detach(&self, strip_id: u64) {
        for b in &self.buses {
            b.rt.inputs.lock().unwrap().retain(|x| x.strip_id != strip_id);
            if let Some(v) = &b.vrt {
                v.inputs.lock().unwrap().retain(|x| x.strip_id != strip_id);
            }
        }
    }

    fn add_strip(&mut self, cfg: StripCfg) {
        let id = self.next_id;
        self.next_id += 1;
        self.strips.push(Strip { id, cfg, params: StripParams::new(), handle: None, status: String::new(), meter: Meter::default() });
        self.connect(self.strips.len() - 1);
    }

    fn remove_strip(&mut self, i: usize) {
        let s = self.strips.remove(i);
        self.detach(s.id);
    }

    fn reconnect_all(&mut self) {
        for i in 0..self.strips.len() {
            self.connect(i);
        }
    }

    /// (Ré)ouvre la source d'une tranche et la branche sur tous les bus.
    fn connect(&mut self, i: usize) {
        let sinks = self.sinks();
        let n = sinks.len();
        self.detach(self.strips[i].id);
        let n_outputs = self.buses.len();
        let s = &mut self.strips[i];
        s.cfg.sends.resize(n_outputs, false);
        s.handle = None;
        s.status.clear();
        let res = match &s.cfg.source {
            SourceCfg::None => return,
            SourceCfg::Device { name, left, right } => {
                engine::open_device_input(name, *left, *right, n).map(|(st, inp)| (Handle::Device(st), inp))
            }
            SourceCfg::App { bundle_id, .. } => match self.apps.iter().find(|a| &a.bundle_id == bundle_id) {
                Some(a) => tap::open_tap(&a.objects, s.cfg.mute_direct, n).map(|(t, inp)| (Handle::Tap(t), inp)),
                None => Err("app not running".into()),
            },
        };
        match res {
            Ok((h, inputs)) => {
                for (sink, input) in sinks.iter().zip(inputs) {
                    sink.inputs.lock().unwrap().push(BusInput { strip_id: s.id, params: s.params.clone(), input });
                }
                s.handle = Some(h);
            }
            Err(e) => s.status = e,
        }
    }

    fn set_latency(&mut self, frames: u32) {
        BUFFER_FRAMES.store(frames, Relaxed);
        for j in 0..self.buses.len() {
            self.open_bus_output(j);
            self.open_virtual(j);
        }
        self.reconnect_all();
    }

    /// Travail périodique : paramètres → threads audio, vu-mètres, rescans, sauvegarde.
    fn tick(&mut self) {
        let mut any_solo = false;
        let auto = self.peak_auto.then(|| Duration::from_secs_f32(self.peak_secs));
        for s in &mut self.strips {
            let mask = s.cfg.sends.iter().enumerate().fold(0u32, |m, (j, on)| m | ((*on as u32) << j));
            s.params.sends.store(mask, Relaxed);
            s.params.gain.set(db_to_gain(s.cfg.db));
            s.params.pan.set(s.cfg.pan);
            s.params.mute.store(s.cfg.mute, Relaxed);
            s.params.solo.store(s.cfg.solo, Relaxed);
            any_solo |= s.cfg.solo;
            s.meter.update([s.params.peak_l.take(), s.params.peak_r.take()], auto);
        }
        ANY_SOLO.store(any_solo, Relaxed);
        for b in &mut self.buses {
            let p = &b.rt.params;
            p.gain.set(db_to_gain(b.cfg.db));
            p.mute.store(b.cfg.mute, Relaxed);
            b.meter.update([p.peak_l.take(), p.peak_r.take()], auto);
        }

        if self.last_scan.elapsed() > Duration::from_secs(2) {
            self.last_scan = Instant::now();
            self.in_devs = engine::input_devices();
            self.out_devs = engine::output_devices();
            self.apps = tap::list_apps();
            for i in 0..self.strips.len() {
                let s = &self.strips[i];
                let stale = match (&s.cfg.source, &s.handle) {
                    (SourceCfg::None, _) => false,
                    (_, None) => true,
                    // L'appli a quitté ou lancé de nouveaux processus audio : on refait le tap.
                    (SourceCfg::App { bundle_id, .. }, Some(Handle::Tap(t))) => self
                        .apps
                        .iter()
                        .find(|a| &a.bundle_id == bundle_id)
                        .is_none_or(|a| a.objects != t.objects),
                    _ => false,
                };
                if stale {
                    self.connect(i);
                }
            }
            self.driver = find_driver();
            self.sync_virtual_devices();
            self.sync_aggregates();
            for j in 0..self.buses.len() {
                // Reopen an output whose device or sample rate changed behind our back
                // (system default switched, rate changed in Audio MIDI Setup or by another app).
                let b = &self.buses[j];
                let hw_stale = !b.cfg.hw_off && {
                    let target = match &b.cfg.device {
                        Some(name) => Target::Name(name),
                        None => Target::Default,
                    };
                    b.stream.is_none()
                        || engine::output_info(&target).is_some_and(|(n, r)| n != b.dev_name || r != b.rate)
                };
                let v_stale = b.cfg.vslot.is_some_and(|slot| {
                    b.vstream.is_none() || vdev::get_rate(slot).is_some_and(|r| r != b.vrate)
                });
                if hw_stale {
                    self.open_bus_output(j);
                }
                if v_stale {
                    self.open_virtual(j);
                }
            }
        }
        if self.last_save.elapsed() > Duration::from_secs(1) {
            self.last_save = Instant::now();
            self.save_if_changed();
        }
    }
}

/// Meter/fader scale: (dB, position 0..1), linear between marks. The marks are the graduation.
const SCALE: [(f32, f32); 13] = [
    (-70.0, 0.0),
    (-60.0, 0.08),
    (-50.0, 0.16),
    (-40.0, 0.25),
    (-30.0, 0.35),
    (-20.0, 0.46),
    (-15.0, 0.54),
    (-10.0, 0.63),
    (-6.0, 0.72),
    (-3.0, 0.79),
    (0.0, 0.86),
    (3.0, 0.93),
    (6.0, 1.0),
];

fn db_to_pos(db: f32) -> f32 {
    let db = db.clamp(MIN_DB, MAX_DB);
    for w in SCALE.windows(2) {
        let ((d0, p0), (d1, p1)) = (w[0], w[1]);
        if db <= d1 {
            return p0 + (db - d0) / (d1 - d0) * (p1 - p0);
        }
    }
    1.0
}

fn pos_to_db(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    for w in SCALE.windows(2) {
        let ((d0, p0), (d1, p1)) = (w[0], w[1]);
        if p <= p1 {
            return d0 + (p - p0) / (p1 - p0) * (d1 - d0);
        }
    }
    MAX_DB
}

fn to_db(lin: f32) -> f32 {
    20.0 * lin.max(1e-7).log10()
}

/// Meter state: falling level, short peak-hold marker, and the numeric peak value.
#[derive(Default)]
struct Meter {
    level: [f32; 2],
    hold: [f32; 2],
    hold_at: [Option<Instant>; 2],
    max: f32,
    max_at: Option<Instant>,
}

impl Meter {
    /// `auto_reset`: forget the numeric peak once it is older than this.
    fn update(&mut self, pk: [f32; 2], auto_reset: Option<Duration>) {
        for i in 0..2 {
            self.level[i] = pk[i].max(self.level[i] * 0.88);
            let expired = self.hold_at[i].is_none_or(|t| t.elapsed() > Duration::from_millis(1500));
            if pk[i] >= self.hold[i] || expired {
                self.hold[i] = self.level[i];
                self.hold_at[i] = Some(Instant::now());
            }
        }
        let m = pk[0].max(pk[1]);
        let expired = match (auto_reset, self.max_at) {
            (Some(d), Some(t)) => t.elapsed() > d,
            _ => false,
        };
        if (m >= self.max && m > 0.0) || expired {
            self.max = m;
            self.max_at = Some(Instant::now());
        }
    }

    fn reset(&mut self) {
        self.max = 0.0;
        self.max_at = None;
    }
}

fn level_color(db: f32) -> egui::Color32 {
    if db > 0.0 {
        egui::Color32::from_rgb(230, 60, 50)
    } else if db > -10.0 {
        egui::Color32::from_rgb(215, 185, 55)
    } else {
        egui::Color32::from_rgb(60, 170, 80)
    }
}

/// Stereo meter with the numbered graduation drawn on top of the bars.
fn meter_ui(ui: &mut egui::Ui, m: &Meter, height: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(40.0, height), egui::Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, egui::CornerRadius::same(2), egui::Color32::from_gray(20));
    let y_of = |db: f32| rect.bottom() - db_to_pos(db) * rect.height();
    for i in 0..2 {
        let x0 = rect.left() + 1.0 + i as f32 * 20.0;
        let db = to_db(m.level[i]);
        p.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x0, y_of(db)), egui::pos2(x0 + 18.0, rect.bottom())),
            egui::CornerRadius::ZERO,
            level_color(db),
        );
        // Peak-hold marker.
        let hold_db = to_db(m.hold[i]);
        if hold_db > MIN_DB {
            let y = y_of(hold_db).max(rect.top() + 1.0);
            p.hline(egui::Rangef::new(x0, x0 + 18.0), y, egui::Stroke::new(2.0, level_color(hold_db)));
        }
    }
    // Graduation: a faint line and a number at every scale mark.
    let font = egui::FontId::proportional(9.0);
    for (db, _) in SCALE {
        let y = y_of(db);
        let strong = db == 0.0;
        let line = egui::Color32::from_white_alpha(if strong { 150 } else { 55 });
        p.hline(rect.x_range(), y, egui::Stroke::new(1.0, line));
        let text = if db > 0.0 { format!("+{db:.0}") } else { format!("{db:.0}") };
        let pos = egui::pos2(rect.center().x, y.clamp(rect.top() + 6.0, rect.bottom() - 6.0));
        p.text(pos + egui::vec2(1.0, 1.0), egui::Align2::CENTER_CENTER, &text, font.clone(), egui::Color32::BLACK);
        p.text(pos, egui::Align2::CENTER_CENTER, &text, font.clone(), egui::Color32::from_gray(235));
    }
    response
}

/// Fader drawn on exactly the same vertical scale as the meter next to it.
fn fader_ui(ui: &mut egui::Ui, db: &mut f32, m: &mut Meter, h: f32) {
    ui.horizontal(|ui| {
        // Double-clicking the meter (not the fader) puts the fader back to 0 dB.
        if meter_ui(ui, m, h).double_clicked() {
            *db = 0.0;
        }
        let (rect, r) = ui.allocate_exact_size(egui::vec2(26.0, h), egui::Sense::click_and_drag());
        let r = r.on_hover_text("Drag to set the level. Double-click the meter for 0 dB.");
        if r.dragged() || r.is_pointer_button_down_on() {
            if let Some(pos) = r.interact_pointer_pos() {
                let v = pos_to_db((rect.bottom() - pos.y) / rect.height());
                *db = (v * 10.0).round() / 10.0;
            }
        }
        let p = ui.painter();
        let cx = rect.center().x;
        p.vline(cx, rect.y_range(), egui::Stroke::new(3.0, egui::Color32::from_gray(60)));
        for (mark, _) in SCALE {
            let y = rect.bottom() - db_to_pos(mark) * rect.height();
            let half = if mark == 0.0 { 9.0 } else { 5.0 };
            p.hline(egui::Rangef::new(cx - half, cx + half), y, egui::Stroke::new(1.0, egui::Color32::from_gray(110)));
        }
        let y = rect.bottom() - db_to_pos(*db) * rect.height();
        let handle = egui::Rect::from_center_size(egui::pos2(cx, y), egui::vec2(24.0, 12.0));
        let fill = if r.dragged() { egui::Color32::from_gray(210) } else { egui::Color32::from_gray(170) };
        p.rect_filled(handle, egui::CornerRadius::same(3), fill);
        p.hline(handle.x_range(), y, egui::Stroke::new(1.5, egui::Color32::BLACK));
    });
    ui.label(if *db <= MIN_DB + 0.05 { "-∞ dB".to_string() } else { format!("{:+.1} dB", *db) });
    let max_db = to_db(m.max);
    let text = if max_db <= MIN_DB { "peak  -∞".to_string() } else { format!("peak {max_db:+.1}") };
    let color = if max_db > 0.0 { egui::Color32::from_rgb(230, 60, 50) } else { ui.visuals().text_color() };
    if ui
        .add(egui::Button::new(egui::RichText::new(text).small().color(color)).frame(false))
        .on_hover_text("Highest peak (dB). Click to reset.")
        .clicked()
    {
        m.reset();
    }
}

fn source_menu(ui: &mut egui::Ui, in_devs: &[(String, u16)], apps: &[tap::AppInfo]) -> Option<SourceCfg> {
    let mut pick = None;
    if ui.button("None").clicked() {
        pick = Some(SourceCfg::None);
    }
    ui.menu_button("Devices", |ui| {
        if in_devs.is_empty() {
            ui.label("(no input device)");
        }
        for (name, ch) in in_devs {
            ui.menu_button(name, |ui| {
                for c in (0..ch.saturating_sub(1)).step_by(2) {
                    if ui.button(format!("Stereo {}-{}", c + 1, c + 2)).clicked() {
                        pick = Some(SourceCfg::Device { name: name.clone(), left: c, right: Some(c + 1) });
                    }
                }
                for c in 0..*ch {
                    if ui.button(format!("Mono {}", c + 1)).clicked() {
                        pick = Some(SourceCfg::Device { name: name.clone(), left: c, right: None });
                    }
                }
            });
        }
    });
    ui.menu_button("Applications", |ui| {
        if apps.is_empty() {
            ui.label("(no audio app)");
        }
        for a in apps {
            let label = if a.playing { format!("▶ {}", a.name) } else { a.name.clone() };
            if ui.button(label).on_hover_text(&a.bundle_id).clicked() {
                pick = Some(SourceCfg::App { bundle_id: a.bundle_id.clone(), name: a.name.clone() });
            }
        }
    });
    if pick.is_some() {
        ui.close();
    }
    pick
}

/// Fixed-height area under the source/device menu (option checkbox + status line), so
/// that the faders of all channels line up whatever each channel shows there.
fn options_area(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let size = egui::vec2(ui.available_width(), 34.0);
    ui.allocate_ui_with_layout(size, egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_min_height(size.y);
        add(ui);
    });
}

fn warn(ui: &mut egui::Ui, text: &str) {
    if !text.is_empty() {
        ui.colored_label(egui::Color32::from_rgb(230, 160, 60), egui::RichText::new(text).small());
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.tick();
        let mut action = None;
        // Channels follow the window height (but keep their width).
        let fader_h = (ui.available_height() - STRIP_OVERHEAD).max(100.0);

        egui::Panel::top("bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("+ Input").clicked() {
                    action = Some(Action::AddStrip);
                }
                if ui.add_enabled(self.buses.len() < MAX_BUSES, egui::Button::new("+ Output")).clicked() {
                    action = Some(Action::AddBus);
                }
                ui.separator();
                ui.menu_button("Settings", |ui| {
                    ui.strong("Latency").on_hover_text(
                        "Buffer size requested from each device.\n\
                         Smaller: less delay, but more risk of crackles.",
                    );
                    let cur = BUFFER_FRAMES.load(Relaxed);
                    for f in BUFFER_CHOICES {
                        let text = format!("{f} frames (~{:.1} ms)", f as f32 / 48.0);
                        if ui.radio(f == cur, text).clicked() && f != cur {
                            action = Some(Action::Latency(f));
                        }
                    }
                    ui.separator();
                    ui.strong("Virtual device sample rate").on_hover_text(
                        "Rate given to a virtual device when it is created.\n\
                         It can still be changed afterwards by other applications\n\
                         or in Audio MIDI Setup; MacMixer follows.",
                    );
                    for r in VDEV_RATES {
                        if ui.radio(r == self.vdev_rate, format!("{:.1} kHz", r as f32 / 1000.0)).clicked() && r != self.vdev_rate {
                            action = Some(Action::VirtualRate(r));
                        }
                    }
                    ui.checkbox(&mut self.couple, "Couple with the system output").on_hover_text(
                        "Also publishes every virtual device combined with the system output\n\
                         device (an aggregate device), for applications such as DAWs that can\n\
                         only open one audio device: mixer signal in, speakers out.",
                    );
                    ui.separator();
                    ui.strong("Peak values");
                    ui.checkbox(&mut self.peak_auto, "Auto-reset");
                    ui.add_enabled(
                        self.peak_auto,
                        egui::Slider::new(&mut self.peak_secs, 0.5..=60.0).logarithmic(true).suffix(" s").text("after"),
                    );
                });
            });
        });

        // The outputs panel is as wide as its channels, up to 60% of the window.
        let outputs_width = (self.buses.len() as f32 * (STRIP_W + 14.0) + 20.0).min(ui.available_width() * 0.6);
        egui::Panel::right("buses").resizable(false).exact_size(outputs_width).show(ui, |ui| {
            let n_buses = self.buses.len();
            let driver_ok = self.driver.is_some();
            ui.add_space(6.0); // same top margin as the input channels
            egui::ScrollArea::horizontal().id_salt("bus_scroll").show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (j, b) in self.buses.iter_mut().enumerate() {
                        ui.push_id(("bus", j), |ui| {
                            egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
                                ui.set_width(STRIP_W);
                                ui.spacing_mut().item_spacing.x = 4.0;
                                // Long names are cut with an ellipsis instead of wrapping.
                                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                                ui.vertical(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.strong(format!("{}", j + 1));
                                        ui.add(egui::TextEdit::singleline(&mut b.cfg.name).desired_width(f32::INFINITY));
                                    });
                                    let shown = if b.cfg.hw_off {
                                        "None".to_string()
                                    } else if b.cfg.device.is_none() {
                                        "System".to_string()
                                    } else {
                                        b.dev_name.clone()
                                    };
                                    egui::ComboBox::from_id_salt("dev").width(STRIP_W).selected_text(shown).show_ui(ui, |ui| {
                                        if ui
                                            .selectable_label(b.cfg.hw_off, "None")
                                            .on_hover_text("No hardware output (virtual device only)")
                                            .clicked()
                                        {
                                            b.cfg.hw_off = true;
                                            action = Some(Action::ReopenBus(j));
                                        }
                                        if ui.selectable_label(!b.cfg.hw_off && b.cfg.device.is_none(), "System output").clicked() {
                                            b.cfg.device = None;
                                            b.cfg.hw_off = false;
                                            action = Some(Action::ReopenBus(j));
                                        }
                                        for d in &self.out_devs {
                                            if ui.selectable_label(!b.cfg.hw_off && b.cfg.device.as_ref() == Some(d), d).clicked() {
                                                b.cfg.device = Some(d.clone());
                                                b.cfg.hw_off = false;
                                                action = Some(Action::ReopenBus(j));
                                            }
                                        }
                                    });
                                    options_area(ui, |ui| {
                                        let mut virt = b.cfg.vslot.is_some();
                                        let r = ui
                                            .add_enabled(driver_ok || virt, egui::Checkbox::new(&mut virt, "Virtual"))
                                            .on_hover_text(format!(
                                                "Publishes this output as an audio device named “{VIRTUAL_PREFIX}{}”,\n\
                                                 which other applications can select as their input.",
                                                b.cfg.name
                                            ))
                                            .on_disabled_hover_text("The MacMixer audio driver is not installed. Run ./install.sh to install it.");
                                        if r.changed() {
                                            action = Some(Action::ToggleVirtual(j));
                                        }
                                        if !b.err.is_empty() || !b.verr.is_empty() {
                                            warn(ui, if b.err.is_empty() { &b.verr } else { &b.err });
                                        } else if let Some(a) = &b.agg {
                                            ui.label(egui::RichText::new(short(&format!("+ {}", a.hw_name), 14)).small().weak())
                                                .on_hover_text(format!(
                                                    "Also published coupled with the system output, as the device\n“{}”.\n\
                                                     Use that one in applications that handle a single audio device.",
                                                    a.name
                                                ));
                                        }
                                    });
                                    fader_ui(ui, &mut b.cfg.db, &mut b.meter, fader_h);
                                    ui.add_space(22.0);
                                    ui.horizontal(|ui| {
                                        ui.toggle_value(&mut b.cfg.mute, "M").on_hover_text("Mute");
                                        if n_buses > 1 && ui.button(egui::RichText::new("×").size(16.0).strong()).on_hover_text("Remove this output channel").clicked() {
                                            action = Some(Action::RemoveBus(j));
                                        }
                                    });
                                });
                            });
                        });
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.strips.is_empty() {
                ui.label("Add an input channel with “+ Input”, then pick its source.");
            }
            let bus_names: Vec<String> = self.buses.iter().map(|b| b.cfg.name.clone()).collect();
            egui::ScrollArea::horizontal().id_salt("strip_scroll").show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (i, s) in self.strips.iter_mut().enumerate() {
                        ui.push_id(s.id, |ui| {
                            egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
                                ui.set_width(STRIP_W);
                                ui.spacing_mut().item_spacing.x = 4.0;
                                // Long names are cut with an ellipsis instead of wrapping.
                                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                                ui.vertical(|ui| {
                                    ui.add(egui::TextEdit::singleline(&mut s.cfg.name).desired_width(f32::INFINITY));
                                    let picked = ui
                                        .menu_button(s.cfg.source.label(), |ui| source_menu(ui, &self.in_devs, &self.apps))
                                        .inner
                                        .flatten();
                                    if let Some(src) = picked {
                                        if s.cfg.name.starts_with("Input") || s.cfg.name.is_empty() {
                                            if let SourceCfg::App { name, .. } | SourceCfg::Device { name, .. } = &src {
                                                s.cfg.name = name.clone();
                                            }
                                        }
                                        s.cfg.source = src;
                                        action = Some(Action::Reconnect(i));
                                    }
                                    options_area(ui, |ui| {
                                        if matches!(s.cfg.source, SourceCfg::App { .. })
                                            && ui
                                                .checkbox(&mut s.cfg.mute_direct, "Exclusive")
                                                .on_hover_text(
                                                    "Silences the app's own output: it is only heard through the mixer.\n\
                                                     Unchecked: the app also keeps playing on its usual output.",
                                                )
                                                .changed()
                                        {
                                            action = Some(Action::Reconnect(i));
                                        }
                                        warn(ui, &s.status);
                                    });
                                    fader_ui(ui, &mut s.cfg.db, &mut s.meter, fader_h);
                                    ui.scope(|ui| {
                                        ui.spacing_mut().slider_width = STRIP_W;
                                        let r = ui
                                            .add(egui::Slider::new(&mut s.cfg.pan, -1.0..=1.0).show_value(false))
                                            .on_hover_text("Balance (double-click: centre)");
                                        if r.double_clicked() {
                                            s.cfg.pan = 0.0;
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.toggle_value(&mut s.cfg.mute, "M").on_hover_text("Mute");
                                        ui.toggle_value(&mut s.cfg.solo, "S").on_hover_text("Solo");
                                        if ui.button(egui::RichText::new("×").size(16.0).strong()).on_hover_text("Remove this input channel").clicked() {
                                            action = Some(Action::RemoveStrip(i));
                                        }
                                    });
                                    ui.horizontal_wrapped(|ui| {
                                        for (j, on) in s.cfg.sends.iter_mut().enumerate() {
                                            let name = bus_names.get(j).map(String::as_str).unwrap_or("");
                                            ui.toggle_value(on, format!("{}", j + 1))
                                                .on_hover_text(format!("Send to output {} “{name}”", j + 1));
                                        }
                                    });
                                });
                            });
                        });
                    }
                });
            });
        });

        match action {
            Some(Action::AddStrip) => {
                let n = self.strips.len() + 1;
                let mut sends = vec![false; self.buses.len()];
                sends[0] = true;
                self.add_strip(StripCfg {
                    name: format!("Input {n}"),
                    source: SourceCfg::None,
                    db: 0.0,
                    pan: 0.0,
                    mute: false,
                    solo: false,
                    sends,
                    mute_direct: true,
                });
            }
            Some(Action::AddBus) => {
                let n = self.buses.len() + 1;
                self.add_bus(BusCfg { name: format!("Output {n}"), device: None, db: 0.0, mute: false, hw_off: false, vslot: None });
            }
            Some(Action::RemoveStrip(i)) => self.remove_strip(i),
            Some(Action::Reconnect(i)) => {
                self.apps = tap::list_apps();
                self.connect(i);
            }
            Some(Action::RemoveBus(j)) => self.remove_bus(j),
            Some(Action::ReopenBus(j)) => {
                self.open_bus_output(j);
                self.reconnect_all();
            }
            Some(Action::ToggleVirtual(j)) => self.toggle_virtual(j),
            Some(Action::VirtualRate(r)) => {
                self.vdev_rate = r;
                for j in 0..self.buses.len() {
                    self.buses[j].vrate_set = false;
                    self.open_virtual(j);
                }
            }
            Some(Action::Latency(f)) => self.set_latency(f),
            None => {}
        }
        if let Some(path) = self.shot.clone() {
            match self.shot_state.load(Relaxed) {
                0 if self.started.elapsed() > Duration::from_secs(3) => {
                    self.shot_state.store(1, Relaxed);
                    let state = self.shot_state.clone();
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(
                        move |img| {
                            write_bmp(&path, &img);
                            state.store(2, Relaxed);
                        },
                    )));
                }
                2 => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                _ => {}
            }
        }
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.save_if_changed();
        // The virtual devices only make sense while the mixer is running.
        for b in &mut self.buses {
            b.vstream = None;
            if let Some(a) = b.agg.take() {
                aggregate::destroy(a.id);
            }
        }
        if let Some(plugin) = self.driver {
            vdev::set_names(plugin, "");
        }
    }
}

/// Minimal 24-bit BMP writer (for MACMIXER_SCREENSHOT).
fn write_bmp(path: &str, img: &egui::ColorImage) {
    let [w, h] = img.size;
    let row = (w * 3).div_ceil(4) * 4;
    let mut out = Vec::with_capacity(54 + row * h);
    out.extend(b"BM");
    out.extend(((54 + row * h) as u32).to_le_bytes());
    out.extend([0u8; 4]);
    out.extend(54u32.to_le_bytes());
    out.extend(40u32.to_le_bytes());
    out.extend((w as i32).to_le_bytes());
    out.extend((h as i32).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(24u16.to_le_bytes());
    out.extend([0u8; 24]);
    let raw = img.as_raw();
    for y in (0..h).rev() {
        let line = &raw[y * w * 4..(y + 1) * w * 4];
        for px in line.chunks_exact(4) {
            out.extend([px[2], px[1], px[0]]);
        }
        out.resize(out.len() + row - w * 3, 0);
    }
    let _ = std::fs::write(path, out);
}

fn main() -> eframe::Result {
    if std::env::args().any(|a| a == "--driver-status") {
        match vdev::plugin() {
            Some(id) => {
                println!("driver loaded (plug-in object {id})");
                let names = vdev::get_names(id).unwrap_or_default();
                for (slot, name) in names.lines().enumerate().filter(|(_, n)| !n.is_empty()) {
                    println!("  slot {slot}: {name} @ {:?} Hz", vdev::get_rate(slot));
                }
            }
            None => println!("driver not loaded"),
        }
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MacMixer")
            // Empty icon data: keep the application bundle's own icon.
            .with_icon(egui::IconData::default()).with_inner_size([900.0, 500.0]).with_min_inner_size([420.0, 340.0]),
        ..Default::default()
    };
    eframe::run_native("MacMixer", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
