//! Client side of the MacMixer virtual audio driver (see `driver/`): tells the driver
//! which virtual devices should exist and how they are named.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
};
use objc2_core_foundation::{CFRetained, CFString};

pub const MAX_DEVICES: usize = 16;
const DRIVER_BUNDLE_ID: &str = "org.macmixer.driver";
const SYSTEM_OBJECT: AudioObjectID = 1;

const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | s[3] as u32
}
/// kAudioHardwarePropertyTranslateBundleIDToPlugIn
const PROP_BUNDLE_ID_TO_PLUGIN: u32 = fourcc(b"bidp");
/// The driver's custom property: device names, one line per slot (empty = no device).
const PROP_DEVICE_NAMES: u32 = fourcc(b"mmxd");

fn addr(sel: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: sel,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// UID of the virtual device in a slot (must match the driver).
pub fn uid(slot: usize) -> String {
    format!("MacMixer_UID_{slot}")
}

/// The driver's plug-in object, if the driver is installed and loaded.
pub fn plugin() -> Option<AudioObjectID> {
    let bundle = CFString::from_str(DRIVER_BUNDLE_ID);
    let qualifier: *const CFString = &*bundle;
    let a = addr(PROP_BUNDLE_ID_TO_PLUGIN);
    let mut id: AudioObjectID = 0;
    let mut size = 4u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            NonNull::from(&a),
            size_of::<*const CFString>() as u32,
            (&qualifier as *const *const CFString).cast::<c_void>(),
            NonNull::from(&mut size),
            NonNull::from(&mut id).cast(),
        )
    };
    (st == 0 && id != 0).then_some(id)
}

pub fn get_names(plugin: AudioObjectID) -> Option<String> {
    let a = addr(PROP_DEVICE_NAMES);
    let mut s: *mut CFString = std::ptr::null_mut();
    let mut size = size_of::<*mut CFString>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            plugin,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut s).cast(),
        )
    };
    if st != 0 {
        return None;
    }
    // The property returns an owned (+1) string.
    let s = unsafe { CFRetained::from_raw(NonNull::new(s)?) };
    Some(s.to_string())
}

pub fn set_names(plugin: AudioObjectID, text: &str) -> bool {
    let a = addr(PROP_DEVICE_NAMES);
    let s = CFString::from_str(text);
    let p: *const CFString = &*s;
    let st = unsafe {
        AudioObjectSetPropertyData(
            plugin,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            size_of::<*const CFString>() as u32,
            NonNull::from(&p).cast(),
        )
    };
    st == 0
}
