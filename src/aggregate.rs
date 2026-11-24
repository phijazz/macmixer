//! Couples a MacMixer virtual device with the system output device in one aggregate
//! device, for applications (DAWs, plug-in hosts) that can only open a single device:
//! they get the mixer's signal as input and the speakers as output.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceUIDKey,
    kAudioDevicePropertyDeviceUID, kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyName,
    kAudioSubDeviceDriftCompensationKey, kAudioSubDeviceUIDKey, AudioHardwareCreateAggregateDevice,
    AudioHardwareDestroyAggregateDevice, AudioObjectID,
};
use objc2_core_foundation::{
    kCFAllocatorDefault, kCFTypeArrayCallBacks, CFArray, CFDictionary, CFMutableDictionary, CFRetained, CFString,
};
use objc2_foundation::{NSNumber, NSString};

use crate::tap::{dict_set, get, new_dict};
use crate::vdev;

const SYSTEM_OBJECT: AudioObjectID = 1;

pub fn uid(slot: usize) -> String {
    format!("MacMixer_Agg_{slot}")
}

fn string_property(obj: AudioObjectID, sel: u32) -> Option<String> {
    // The property returns an owned (+1) CFString, toll-free bridged to NSString.
    let s = unsafe { Retained::from_raw(get::<*mut NSString>(obj, sel)?)? };
    Some(s.to_string())
}

pub struct DefaultOutput {
    pub uid: String,
    pub name: String,
}

/// The system default output device, unless it is one of MacMixer's own devices.
pub fn default_output() -> Option<DefaultOutput> {
    let id = unsafe { get::<AudioObjectID>(SYSTEM_OBJECT, kAudioHardwarePropertyDefaultOutputDevice)? };
    if id == 0 {
        return None;
    }
    let uid = string_property(id, kAudioDevicePropertyDeviceUID)?;
    if uid.starts_with("MacMixer_") {
        return None;
    }
    Some(DefaultOutput { uid, name: string_property(id, kAudioObjectPropertyName)? })
}

fn ptr(s: &CFString) -> *const c_void {
    s as *const CFString as *const c_void
}

/// Creates the aggregate "hardware output + virtual device of `slot`". The hardware
/// comes first (its channels are 1-2 on the output side) and is the clock; the virtual
/// device is drift-compensated.
pub fn create(slot: usize, name: &str, hw_uid: &str) -> Result<AudioObjectID, String> {
    unsafe {
        let yes = NSNumber::new_bool(true);
        let no = NSNumber::new_bool(false);
        let hw = CFString::from_str(hw_uid);
        let virt = CFString::from_str(&vdev::uid(slot));

        let sub_hw = new_dict();
        dict_set(&sub_hw, kAudioSubDeviceUIDKey, ptr(&hw));
        let sub_virt = new_dict();
        dict_set(&sub_virt, kAudioSubDeviceUIDKey, ptr(&virt));
        dict_set(&sub_virt, kAudioSubDeviceDriftCompensationKey, Retained::as_ptr(&yes) as *const c_void);
        let subs = [
            &*sub_hw as *const CFMutableDictionary as *const c_void,
            &*sub_virt as *const CFMutableDictionary as *const c_void,
        ];
        let list = CFArray::new(kCFAllocatorDefault, subs.as_ptr() as *mut *const c_void, 2, &kCFTypeArrayCallBacks)
            .ok_or("aggregate device failed")?;

        let d = new_dict();
        dict_set(&d, kAudioAggregateDeviceNameKey, ptr(&CFString::from_str(name)));
        dict_set(&d, kAudioAggregateDeviceUIDKey, ptr(&CFString::from_str(&uid(slot))));
        dict_set(&d, kAudioAggregateDeviceSubDeviceListKey, &*list as *const CFArray as *const c_void);
        dict_set(&d, kAudioAggregateDeviceMainSubDeviceKey, ptr(&hw));
        dict_set(&d, kAudioAggregateDeviceIsPrivateKey, Retained::as_ptr(&no) as *const c_void);
        dict_set(&d, kAudioAggregateDeviceIsStackedKey, Retained::as_ptr(&no) as *const c_void);
        let d: CFRetained<CFDictionary> = CFRetained::cast_unchecked(d);

        let mut id: AudioObjectID = 0;
        let st = AudioHardwareCreateAggregateDevice(&d, NonNull::from(&mut id));
        if st != 0 || id == 0 {
            return Err(format!("aggregate device failed ({st})"));
        }
        Ok(id)
    }
}

pub fn destroy(id: AudioObjectID) {
    unsafe { AudioHardwareDestroyAggregateDevice(id) };
}

/// Removes aggregates left behind by a previous run that did not exit cleanly.
pub fn destroy_stale() {
    for slot in 0..vdev::MAX_DEVICES {
        if let Some(id) = vdev::device_by_uid(&uid(slot)) {
            destroy(id);
        }
    }
}
