//! Capture du son d'une application via les « process taps » CoreAudio (macOS 14.2+).
//! Aucun pilote virtuel (BlackHole, etc.) n'est nécessaire.

#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::{c_void, CStr};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyBufferFrameSize,
    kAudioHardwarePropertyProcessObjectList, AudioObjectSetPropertyData,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertySelector, CATapDescription, CATapMuteBehavior,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};
use objc2_core_foundation::{
    kCFAllocatorDefault, kCFTypeArrayCallBacks, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks, CFArray, CFDictionary, CFMutableDictionary, CFRetained,
    CFString,
};
use objc2_foundation::{NSArray, NSNumber, NSString};

use crate::engine::{fanout, Fanout, Input, BUFFER_FRAMES};

const SYSTEM_OBJECT: AudioObjectID = 1;
static INSTANCE: AtomicU32 = AtomicU32::new(0);

fn addr(sel: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: sel,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Lit une propriété de taille fixe. `T` doit être un type C simple.
unsafe fn get<T>(obj: AudioObjectID, sel: AudioObjectPropertySelector) -> Option<T> {
    let mut v: T = std::mem::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    let a = addr(sel);
    let st = AudioObjectGetPropertyData(
        obj,
        NonNull::from(&a),
        0,
        std::ptr::null(),
        NonNull::from(&mut size),
        NonNull::from(&mut v).cast(),
    );
    (st == 0).then_some(v)
}

fn process_objects() -> Vec<AudioObjectID> {
    unsafe {
        let a = addr(kAudioHardwarePropertyProcessObjectList);
        let mut size = 0u32;
        if AudioObjectGetPropertyDataSize(
            SYSTEM_OBJECT,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        ) != 0
            || size == 0
        {
            return vec![];
        }
        let mut ids = vec![0 as AudioObjectID; size as usize / 4];
        let st = AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(ids.as_mut_ptr()).unwrap().cast(),
        );
        if st != 0 {
            return vec![];
        }
        ids.truncate(size as usize / 4);
        ids
    }
}

fn proc_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// Une application audio, tous ses processus regroupés par identifiant de bundle.
#[derive(Clone, Debug)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
    pub playing: bool,
    pub objects: Vec<AudioObjectID>,
}

pub fn list_apps() -> Vec<AppInfo> {
    let me = std::process::id() as i32;
    let mut apps: Vec<AppInfo> = Vec::new();
    for obj in process_objects() {
        unsafe {
            let Some(pid) = get::<i32>(obj, kAudioProcessPropertyPID) else { continue };
            if pid == me {
                continue;
            }
            let Some(ptr) = get::<*mut NSString>(obj, kAudioProcessPropertyBundleID) else {
                continue;
            };
            // La propriété rend une CFString possédée (+1), compatible NSString.
            let Some(s) = Retained::from_raw(ptr) else { continue };
            let bundle_id = s.to_string();
            if bundle_id.is_empty() {
                continue;
            }
            let playing = get::<u32>(obj, kAudioProcessPropertyIsRunningOutput).unwrap_or(0) != 0;
            if let Some(a) = apps.iter_mut().find(|a| a.bundle_id == bundle_id) {
                a.objects.push(obj);
                a.playing |= playing;
            } else {
                let name = proc_name(pid).unwrap_or_else(|| bundle_id.clone());
                apps.push(AppInfo { bundle_id, name, playing, objects: vec![obj] });
            }
        }
    }
    for a in &mut apps {
        a.objects.sort_unstable();
    }
    apps.sort_by(|a, b| b.playing.cmp(&a.playing).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    apps
}

struct Ctx {
    fan: Fanout,
}

/// Un tap actif. Tout est détruit au `drop`.
pub struct TapSource {
    tap: AudioObjectID,
    agg: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    ctx: *mut Ctx,
    pub objects: Vec<AudioObjectID>,
}

impl Drop for TapSource {
    fn drop(&mut self) {
        unsafe {
            if self.agg != 0 {
                if self.proc_id.is_some() {
                    AudioDeviceStop(self.agg, self.proc_id);
                    AudioDeviceDestroyIOProcID(self.agg, self.proc_id);
                }
                AudioHardwareDestroyAggregateDevice(self.agg);
            }
            if self.tap != 0 {
                AudioHardwareDestroyProcessTap(self.tap);
            }
            if !self.ctx.is_null() {
                drop(Box::from_raw(self.ctx));
            }
        }
    }
}

unsafe fn dict_set(d: &CFMutableDictionary, key: &CStr, val: *const c_void) {
    let k = CFString::from_str(key.to_str().unwrap());
    CFMutableDictionary::set_value(Some(d), &*k as *const CFString as *const c_void, val);
}

unsafe fn new_dict() -> CFRetained<CFMutableDictionary> {
    CFMutableDictionary::new(
        kCFAllocatorDefault,
        0,
        &kCFTypeDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks,
    )
    .unwrap()
}

unsafe extern "C-unwind" fn ioproc(
    _dev: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _in_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _out_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    let ctx = &mut *(client as *mut Ctx);
    let abl = input.as_ptr();
    let nb = (*abl).mNumberBuffers as usize;
    if nb == 0 {
        return 0;
    }
    let bufs = std::slice::from_raw_parts((*abl).mBuffers.as_ptr(), nb);
    let b0 = &bufs[0];
    if b0.mData.is_null() {
        return 0;
    }
    let mut push = |l: f32, r: f32| ctx.fan.push(l, r);
    if nb >= 2 && b0.mNumberChannels == 1 && !bufs[1].mData.is_null() {
        // Non entrelacé : un tampon par canal.
        let n = (b0.mDataByteSize.min(bufs[1].mDataByteSize) / 4) as usize;
        let l = std::slice::from_raw_parts(b0.mData as *const f32, n);
        let r = std::slice::from_raw_parts(bufs[1].mData as *const f32, n);
        for i in 0..n {
            push(l[i], r[i]);
        }
    } else {
        let nch = b0.mNumberChannels.max(1) as usize;
        let n = (b0.mDataByteSize / 4) as usize;
        let s = std::slice::from_raw_parts(b0.mData as *const f32, n);
        for fr in s.chunks_exact(nch) {
            push(fr[0], if nch > 1 { fr[1] } else { fr[0] });
        }
    }
    0
}

/// Crée un tap stéréo sur les processus donnés, copié vers `n` bus. Si `mute_direct`, leur
/// sortie directe est coupée tant que le tap est lu : le son ne passe plus que par le mixeur.
pub fn open_tap(
    objects: &[AudioObjectID],
    mute_direct: bool,
    n: usize,
) -> Result<(TapSource, Vec<Input>), String> {
    let mut src = TapSource {
        tap: 0,
        agg: 0,
        proc_id: None,
        ctx: std::ptr::null_mut(),
        objects: objects.to_vec(),
    };
    let inst = INSTANCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    unsafe {
        let nums: Vec<Retained<NSNumber>> = objects.iter().map(|&o| NSNumber::new_u32(o)).collect();
        let arr = NSArray::from_retained_slice(&nums);
        let desc = CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &arr);
        desc.setMuteBehavior(if mute_direct {
            CATapMuteBehavior::MutedWhenTapped
        } else {
            CATapMuteBehavior::Unmuted
        });
        desc.setPrivate(true);
        desc.setName(&NSString::from_str(&format!("MacMixer tap {pid}.{inst}")));

        let st = AudioHardwareCreateProcessTap(Some(&desc), &mut src.tap);
        if st != 0 || src.tap == 0 {
            src.tap = 0;
            return Err(format!("tap creation failed ({st})"));
        }
        let tap_uid = desc.UUID().UUIDString();

        let rate = get::<AudioStreamBasicDescription>(src.tap, kAudioTapPropertyFormat)
            .map(|f| f.mSampleRate as u32)
            .filter(|&r| r > 0)
            .unwrap_or(48_000);

        // Périphérique agrégé privé ne contenant que le tap.
        let yes = NSNumber::new_bool(true);
        let yes_ptr = Retained::as_ptr(&yes) as *const c_void;
        let sub = new_dict();
        dict_set(&sub, kAudioSubTapUIDKey, Retained::as_ptr(&tap_uid) as *const c_void);
        dict_set(&sub, kAudioSubTapDriftCompensationKey, yes_ptr);
        let subs = [&*sub as *const CFMutableDictionary as *const c_void];
        let taps = CFArray::new(
            kCFAllocatorDefault,
            subs.as_ptr() as *mut *const c_void,
            1,
            &kCFTypeArrayCallBacks,
        )
        .unwrap();
        let d = new_dict();
        let name = CFString::from_str(&format!("MacMixer aggregate {pid}.{inst}"));
        let uid = CFString::from_str(&format!("org.macmixer.tap.{pid}.{inst}"));
        dict_set(&d, kAudioAggregateDeviceNameKey, &*name as *const CFString as *const c_void);
        dict_set(&d, kAudioAggregateDeviceUIDKey, &*uid as *const CFString as *const c_void);
        dict_set(&d, kAudioAggregateDeviceTapListKey, &*taps as *const CFArray as *const c_void);
        dict_set(&d, kAudioAggregateDeviceTapAutoStartKey, yes_ptr);
        dict_set(&d, kAudioAggregateDeviceIsPrivateKey, yes_ptr);
        let d: CFRetained<CFDictionary> = CFRetained::cast_unchecked(d);

        let st = AudioHardwareCreateAggregateDevice(&d, NonNull::from(&mut src.agg));
        if st != 0 {
            src.agg = 0;
            return Err(format!("aggregate device failed ({st})"));
        }

        // Taille de tampon du réglage « Latence » (ignorée si le système la refuse).
        let frames: u32 = BUFFER_FRAMES.load(Ordering::Relaxed);
        let a = addr(kAudioDevicePropertyBufferFrameSize);
        AudioObjectSetPropertyData(
            src.agg,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            4,
            NonNull::from(&frames).cast(),
        );

        let (fan, inputs) = fanout(rate, n);
        src.ctx = Box::into_raw(Box::new(Ctx { fan }));
        let st = AudioDeviceCreateIOProcID(
            src.agg,
            Some(ioproc),
            src.ctx.cast(),
            NonNull::from(&mut src.proc_id),
        );
        if st != 0 {
            src.proc_id = None;
            return Err(format!("IOProc failed ({st})"));
        }
        let st = AudioDeviceStart(src.agg, src.proc_id);
        if st != 0 {
            return Err(format!("start failed ({st})"));
        }
        Ok((src, inputs))
    }
}
