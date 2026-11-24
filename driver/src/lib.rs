//! MacMixer virtual audio driver (CoreAudio AudioServerPlugIn / HAL plug-in).
//!
//! It publishes up to `MAX_DEVICES` loopback devices. Each one has a stereo output
//! stream and a stereo input stream: whatever is played to the device can be recorded
//! from it. MacMixer plays an output channel into a device; any other application can
//! then select that device as its audio input.
//!
//! Which devices exist, and their names, is controlled by MacMixer through a custom
//! property on the plug-in object (`PROP_DEVICE_NAMES`): a string with one line per
//! slot, an empty line meaning "no device in this slot".

#![allow(non_snake_case, non_upper_case_globals, unsafe_op_in_unsafe_fn, clippy::missing_safety_doc)]

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use objc2_core_audio::AudioObjectPropertyAddress;
use objc2_core_audio_types::AudioTimeStamp;
use objc2_core_foundation::{CFRetained, CFString, CFUUIDBytes};

type OSStatus = i32;
type ObjectID = u32;

const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | s[3] as u32
}

pub const MAX_DEVICES: usize = 16;
/// Custom plug-in property (CFString): device names, one line per slot.
pub const PROP_DEVICE_NAMES: u32 = fourcc(b"mmxd");
pub const SAMPLE_RATE: f64 = 48_000.0;
const CHANNELS: usize = 2;
/// Ring buffer length in frames; also the zero-timestamp period.
const RING: usize = 16_384;

const PLUGIN_ID: ObjectID = 1;
const FIRST_DEVICE_ID: ObjectID = 100;

pub const fn device_id(slot: usize) -> ObjectID {
    FIRST_DEVICE_ID + slot as u32 * 10
}
pub const fn input_stream_id(slot: usize) -> ObjectID {
    device_id(slot) + 1
}
pub const fn output_stream_id(slot: usize) -> ObjectID {
    device_id(slot) + 2
}
pub fn device_uid(slot: usize) -> String {
    format!("MacMixer_UID_{slot}")
}

// Error codes.
const ERR_UNKNOWN_PROPERTY: OSStatus = fourcc(b"who?") as i32;
const ERR_BAD_OBJECT: OSStatus = fourcc(b"!obj") as i32;
const ERR_BAD_SIZE: OSStatus = fourcc(b"!siz") as i32;
const ERR_ILLEGAL: OSStatus = fourcc(b"nope") as i32;
const ERR_UNSUPPORTED: OSStatus = fourcc(b"unop") as i32;

// Property selectors, scopes and classes (four-character codes from the CoreAudio headers).
const SCOPE_INPUT: u32 = fourcc(b"inpt");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");

const P_BASE_CLASS: u32 = fourcc(b"bcls");
const P_CLASS: u32 = fourcc(b"clas");
const P_OWNER: u32 = fourcc(b"stdv");
const P_NAME: u32 = fourcc(b"lnam");
const P_MANUFACTURER: u32 = fourcc(b"lmak");
const P_OWNED_OBJECTS: u32 = fourcc(b"ownd");
const P_CUSTOM_INFO_LIST: u32 = fourcc(b"cust");

const P_PLUGIN_BOX_LIST: u32 = fourcc(b"box#");
const P_PLUGIN_UID_TO_BOX: u32 = fourcc(b"uidb");
const P_PLUGIN_DEVICE_LIST: u32 = fourcc(b"dev#");
const P_PLUGIN_UID_TO_DEVICE: u32 = fourcc(b"uidd");
const P_PLUGIN_RESOURCE_BUNDLE: u32 = fourcc(b"rsrc");

const P_DEV_UID: u32 = fourcc(b"uid ");
const P_DEV_MODEL_UID: u32 = fourcc(b"muid");
const P_DEV_TRANSPORT: u32 = fourcc(b"tran");
const P_DEV_RELATED: u32 = fourcc(b"akin");
const P_DEV_CLOCK_DOMAIN: u32 = fourcc(b"clkd");
const P_DEV_ALIVE: u32 = fourcc(b"livn");
const P_DEV_RUNNING: u32 = fourcc(b"goin");
const P_DEV_CAN_BE_DEFAULT: u32 = fourcc(b"dflt");
const P_DEV_CAN_BE_SYSTEM_DEFAULT: u32 = fourcc(b"sflt");
const P_LATENCY: u32 = fourcc(b"ltnc");
const P_DEV_STREAMS: u32 = fourcc(b"stm#");
const P_CONTROL_LIST: u32 = fourcc(b"ctrl");
const P_DEV_SAFETY_OFFSET: u32 = fourcc(b"saft");
const P_DEV_SAMPLE_RATE: u32 = fourcc(b"nsrt");
const P_DEV_AVAILABLE_RATES: u32 = fourcc(b"nsr#");
const P_DEV_HIDDEN: u32 = fourcc(b"hidn");
const P_DEV_STEREO_CHANNELS: u32 = fourcc(b"dch2");
const P_DEV_CHANNEL_LAYOUT: u32 = fourcc(b"srnd");
const P_DEV_ZERO_TS_PERIOD: u32 = fourcc(b"ring");
const P_DEV_CLOCK_STABLE: u32 = fourcc(b"cstb");

const P_STREAM_ACTIVE: u32 = fourcc(b"sact");
const P_STREAM_DIRECTION: u32 = fourcc(b"sdir");
const P_STREAM_TERMINAL: u32 = fourcc(b"term");
const P_STREAM_START_CHANNEL: u32 = fourcc(b"schn");
const P_STREAM_VIRTUAL_FORMAT: u32 = fourcc(b"sfmt");
const P_STREAM_AVAILABLE_VIRTUAL: u32 = fourcc(b"sfma");
const P_STREAM_PHYSICAL_FORMAT: u32 = fourcc(b"pft ");
const P_STREAM_AVAILABLE_PHYSICAL: u32 = fourcc(b"pfta");

const CLASS_OBJECT: u32 = fourcc(b"aobj");
const CLASS_PLUGIN: u32 = fourcc(b"aplg");
const CLASS_DEVICE: u32 = fourcc(b"adev");
const CLASS_STREAM: u32 = fourcc(b"astr");
const TRANSPORT_VIRTUAL: u32 = fourcc(b"virt");
const TERMINAL_MICROPHONE: u32 = fourcc(b"mic ");
const TERMINAL_SPEAKER: u32 = fourcc(b"spkr");
const LAYOUT_TAG_STEREO: u32 = (101 << 16) | 2;

const CUSTOM_TYPE_NONE: u32 = 0;
const CUSTOM_TYPE_CFSTRING: u32 = fourcc(b"cfst");

const OP_READ_INPUT: u32 = fourcc(b"read");
const OP_WRITE_MIX: u32 = fourcc(b"rite");

// ---------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------

#[repr(C)]
pub struct HostInterface {
    pub PropertiesChanged: unsafe extern "C" fn(
        host: *const HostInterface,
        object: ObjectID,
        count: u32,
        addresses: *const AudioObjectPropertyAddress,
    ) -> OSStatus,
    pub CopyFromStorage: *const c_void,
    pub WriteToStorage: *const c_void,
    pub DeleteFromStorage: *const c_void,
    pub RequestDeviceConfigurationChange: *const c_void,
}

static HOST: AtomicPtr<HostInterface> = AtomicPtr::new(std::ptr::null_mut());
/// Device name per slot; `None` = no device.
static NAMES: Mutex<[Option<String>; MAX_DEVICES]> = Mutex::new([const { None }; MAX_DEVICES]);

struct Clock {
    io_count: AtomicU32,
    anchor_host_time: AtomicU64,
    periods: AtomicU64,
}
static CLOCKS: [Clock; MAX_DEVICES] = [const {
    Clock { io_count: AtomicU32::new(0), anchor_host_time: AtomicU64::new(0), periods: AtomicU64::new(0) }
}; MAX_DEVICES];

/// Loopback storage for one device. Only touched from that device's IO thread.
struct Ring(UnsafeCell<Box<[f32]>>);
unsafe impl Sync for Ring {}
static RINGS: OnceLock<Vec<Ring>> = OnceLock::new();

fn rings() -> &'static Vec<Ring> {
    RINGS.get_or_init(|| {
        (0..MAX_DEVICES).map(|_| Ring(UnsafeCell::new(vec![0.0f32; RING * CHANNELS].into_boxed_slice()))).collect()
    })
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

fn host_ticks_per_frame() -> f64 {
    static TICKS: OnceLock<f64> = OnceLock::new();
    *TICKS.get_or_init(|| {
        let mut tb = MachTimebaseInfo { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut tb) };
        let ticks_per_second = 1e9 * tb.denom as f64 / tb.numer as f64;
        ticks_per_second / SAMPLE_RATE
    })
}

fn names() -> std::sync::MutexGuard<'static, [Option<String>; MAX_DEVICES]> {
    NAMES.lock().unwrap_or_else(|e| e.into_inner())
}

fn active_slots() -> Vec<usize> {
    names().iter().enumerate().filter(|(_, n)| n.is_some()).map(|(i, _)| i).collect()
}

enum Object {
    PlugIn,
    Device(usize),
    /// (slot, is_input)
    Stream(usize, bool),
}

fn slot_of_device(id: ObjectID) -> Option<usize> {
    if id < FIRST_DEVICE_ID || (id - FIRST_DEVICE_ID) % 10 != 0 {
        return None;
    }
    let slot = ((id - FIRST_DEVICE_ID) / 10) as usize;
    (slot < MAX_DEVICES).then_some(slot)
}

fn classify(id: ObjectID) -> Option<Object> {
    if id == PLUGIN_ID {
        return Some(Object::PlugIn);
    }
    if id < FIRST_DEVICE_ID {
        return None;
    }
    let slot = ((id - FIRST_DEVICE_ID) / 10) as usize;
    if slot >= MAX_DEVICES || names()[slot].is_none() {
        return None;
    }
    match (id - FIRST_DEVICE_ID) % 10 {
        0 => Some(Object::Device(slot)),
        1 => Some(Object::Stream(slot, true)),
        2 => Some(Object::Stream(slot, false)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------------------

enum Value {
    Bytes(Vec<u8>),
    /// Returned to the host as a retained CFString.
    Str(String),
}

fn v_u32(v: u32) -> Value {
    Value::Bytes(v.to_ne_bytes().to_vec())
}
fn v_u32s(v: &[u32]) -> Value {
    Value::Bytes(v.iter().flat_map(|x| x.to_ne_bytes()).collect())
}
fn v_f64(v: f64) -> Value {
    Value::Bytes(v.to_ne_bytes().to_vec())
}
fn v_str(s: &str) -> Value {
    Value::Str(s.to_string())
}

/// AudioStreamBasicDescription: 48 kHz, stereo, interleaved native-endian packed Float32.
fn format_bytes() -> Vec<u8> {
    let mut b = Vec::with_capacity(40);
    b.extend(SAMPLE_RATE.to_ne_bytes());
    b.extend(fourcc(b"lpcm").to_ne_bytes());
    b.extend((1u32 | 8).to_ne_bytes()); // float | packed
    b.extend((4 * CHANNELS as u32).to_ne_bytes()); // bytes per packet
    b.extend(1u32.to_ne_bytes()); // frames per packet
    b.extend((4 * CHANNELS as u32).to_ne_bytes()); // bytes per frame
    b.extend((CHANNELS as u32).to_ne_bytes());
    b.extend(32u32.to_ne_bytes()); // bits per channel
    b.extend(0u32.to_ne_bytes()); // reserved
    b
}

/// AudioStreamRangedDescription for the single supported format.
fn ranged_format_bytes() -> Vec<u8> {
    let mut b = format_bytes();
    b.extend(SAMPLE_RATE.to_ne_bytes());
    b.extend(SAMPLE_RATE.to_ne_bytes());
    b
}

unsafe fn qualifier_string(size: u32, data: *const c_void) -> Option<String> {
    if size as usize != size_of::<*const CFString>() || data.is_null() {
        return None;
    }
    let s = *(data as *const *const CFString);
    if s.is_null() { None } else { Some((*s).to_string()) }
}

unsafe fn get_property(
    id: ObjectID,
    addr: &AudioObjectPropertyAddress,
    qual_size: u32,
    qual: *const c_void,
) -> Result<Value, OSStatus> {
    let obj = classify(id).ok_or(ERR_BAD_OBJECT)?;
    let sel = addr.mSelector;
    let scope = addr.mScope;
    Ok(match obj {
        Object::PlugIn => match sel {
            P_BASE_CLASS => v_u32(CLASS_OBJECT),
            P_CLASS => v_u32(CLASS_PLUGIN),
            P_OWNER => v_u32(0),
            P_MANUFACTURER => v_str("MacMixer"),
            P_OWNED_OBJECTS | P_PLUGIN_DEVICE_LIST => {
                v_u32s(&active_slots().into_iter().map(device_id).collect::<Vec<_>>())
            }
            P_PLUGIN_BOX_LIST => v_u32s(&[]),
            P_PLUGIN_UID_TO_BOX => v_u32(0),
            P_PLUGIN_UID_TO_DEVICE => {
                let uid = qualifier_string(qual_size, qual);
                let found = active_slots().into_iter().find(|&s| uid.as_deref() == Some(device_uid(s).as_str()));
                v_u32(found.map_or(0, device_id))
            }
            P_PLUGIN_RESOURCE_BUNDLE => v_str(""),
            P_CUSTOM_INFO_LIST => v_u32s(&[PROP_DEVICE_NAMES, CUSTOM_TYPE_CFSTRING, CUSTOM_TYPE_NONE]),
            PROP_DEVICE_NAMES => {
                let n = names();
                Value::Str(n.iter().map(|x| x.as_deref().unwrap_or("")).collect::<Vec<_>>().join("\n"))
            }
            _ => return Err(ERR_UNKNOWN_PROPERTY),
        },
        Object::Device(slot) => match sel {
            P_BASE_CLASS => v_u32(CLASS_OBJECT),
            P_CLASS => v_u32(CLASS_DEVICE),
            P_OWNER => v_u32(PLUGIN_ID),
            P_NAME => Value::Str(names()[slot].clone().unwrap_or_default()),
            P_MANUFACTURER => v_str("MacMixer"),
            P_OWNED_OBJECTS | P_DEV_STREAMS => match scope {
                SCOPE_INPUT => v_u32s(&[input_stream_id(slot)]),
                SCOPE_OUTPUT => v_u32s(&[output_stream_id(slot)]),
                _ => v_u32s(&[input_stream_id(slot), output_stream_id(slot)]),
            },
            P_DEV_UID => Value::Str(device_uid(slot)),
            P_DEV_MODEL_UID => v_str("MacMixer_Model"),
            P_DEV_TRANSPORT => v_u32(TRANSPORT_VIRTUAL),
            P_DEV_RELATED => v_u32s(&[id]),
            P_DEV_CLOCK_DOMAIN => v_u32(0),
            P_DEV_ALIVE => v_u32(1),
            P_DEV_RUNNING => v_u32((CLOCKS[slot].io_count.load(Ordering::Relaxed) > 0) as u32),
            P_DEV_CAN_BE_DEFAULT => v_u32(1),
            P_DEV_CAN_BE_SYSTEM_DEFAULT => v_u32(0),
            P_LATENCY | P_DEV_SAFETY_OFFSET => v_u32(0),
            P_CONTROL_LIST => v_u32s(&[]),
            P_DEV_SAMPLE_RATE => v_f64(SAMPLE_RATE),
            P_DEV_AVAILABLE_RATES => {
                let mut b = SAMPLE_RATE.to_ne_bytes().to_vec();
                b.extend(SAMPLE_RATE.to_ne_bytes());
                Value::Bytes(b)
            }
            P_DEV_HIDDEN => v_u32(0),
            P_DEV_STEREO_CHANNELS => v_u32s(&[1, 2]),
            // AudioChannelLayout { tag = stereo, bitmap = 0, descriptions = 0 }
            P_DEV_CHANNEL_LAYOUT => v_u32s(&[LAYOUT_TAG_STEREO, 0, 0]),
            P_DEV_ZERO_TS_PERIOD => v_u32(RING as u32),
            P_DEV_CLOCK_STABLE => v_u32(1),
            _ => return Err(ERR_UNKNOWN_PROPERTY),
        },
        Object::Stream(slot, is_input) => match sel {
            P_BASE_CLASS => v_u32(CLASS_OBJECT),
            P_CLASS => v_u32(CLASS_STREAM),
            P_OWNER => v_u32(device_id(slot)),
            P_OWNED_OBJECTS => v_u32s(&[]),
            P_STREAM_ACTIVE => v_u32(1),
            P_STREAM_DIRECTION => v_u32(is_input as u32),
            P_STREAM_TERMINAL => v_u32(if is_input { TERMINAL_MICROPHONE } else { TERMINAL_SPEAKER }),
            P_STREAM_START_CHANNEL => v_u32(1),
            P_LATENCY => v_u32(0),
            P_STREAM_VIRTUAL_FORMAT | P_STREAM_PHYSICAL_FORMAT => Value::Bytes(format_bytes()),
            P_STREAM_AVAILABLE_VIRTUAL | P_STREAM_AVAILABLE_PHYSICAL => Value::Bytes(ranged_format_bytes()),
            _ => return Err(ERR_UNKNOWN_PROPERTY),
        },
    })
}

fn notify(object: ObjectID, selectors: &[u32]) {
    let host = HOST.load(Ordering::Acquire);
    if host.is_null() {
        return;
    }
    let addrs: Vec<AudioObjectPropertyAddress> = selectors
        .iter()
        .map(|&s| AudioObjectPropertyAddress { mSelector: s, mScope: fourcc(b"glob"), mElement: 0 })
        .collect();
    unsafe { ((*host).PropertiesChanged)(host, object, addrs.len() as u32, addrs.as_ptr()) };
}

/// Apply a new device list ("name per line") and tell the host what changed.
fn set_device_names(text: &str) {
    let mut new: [Option<String>; MAX_DEVICES] = [const { None }; MAX_DEVICES];
    for (slot, line) in text.split('\n').take(MAX_DEVICES).enumerate() {
        let line = line.trim();
        if !line.is_empty() {
            new[slot] = Some(line.to_string());
        }
    }
    let mut list_changed = false;
    let mut renamed = Vec::new();
    {
        let mut cur = names();
        for slot in 0..MAX_DEVICES {
            match (&cur[slot], &new[slot]) {
                (Some(a), Some(b)) if a != b => renamed.push(slot),
                (Some(_), None) | (None, Some(_)) => list_changed = true,
                _ => {}
            }
        }
        *cur = new;
    }
    // The lock is released before calling back into the host.
    if list_changed {
        notify(PLUGIN_ID, &[P_PLUGIN_DEVICE_LIST, P_OWNED_OBJECTS]);
    }
    for slot in renamed {
        notify(device_id(slot), &[P_NAME]);
    }
}

// ---------------------------------------------------------------------------------------
// Driver interface
// ---------------------------------------------------------------------------------------

type Ref = *mut c_void;
type Addr = *const AudioObjectPropertyAddress;

#[repr(C)]
pub struct IOCycleInfo {
    pub mIOCycleCounter: u64,
    pub mNominalIOBufferFrameSize: u32,
    pub mCurrentTime: AudioTimeStamp,
    pub mInputTime: AudioTimeStamp,
    pub mOutputTime: AudioTimeStamp,
    pub mMainHostTicksPerFrame: f64,
    pub mDeviceHostTicksPerFrame: f64,
}

#[repr(C)]
pub struct DriverInterface {
    _reserved: *mut c_void,
    pub QueryInterface: unsafe extern "C" fn(Ref, CFUUIDBytes, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "C" fn(Ref) -> u32,
    pub Release: unsafe extern "C" fn(Ref) -> u32,
    pub Initialize: unsafe extern "C" fn(Ref, *const HostInterface) -> OSStatus,
    pub CreateDevice: unsafe extern "C" fn(Ref, *const c_void, *const c_void, *mut ObjectID) -> OSStatus,
    pub DestroyDevice: unsafe extern "C" fn(Ref, ObjectID) -> OSStatus,
    pub AddDeviceClient: unsafe extern "C" fn(Ref, ObjectID, *const c_void) -> OSStatus,
    pub RemoveDeviceClient: unsafe extern "C" fn(Ref, ObjectID, *const c_void) -> OSStatus,
    pub PerformDeviceConfigurationChange: unsafe extern "C" fn(Ref, ObjectID, u64, *mut c_void) -> OSStatus,
    pub AbortDeviceConfigurationChange: unsafe extern "C" fn(Ref, ObjectID, u64, *mut c_void) -> OSStatus,
    pub HasProperty: unsafe extern "C" fn(Ref, ObjectID, libc::pid_t, Addr) -> u8,
    pub IsPropertySettable: unsafe extern "C" fn(Ref, ObjectID, libc::pid_t, Addr, *mut u8) -> OSStatus,
    pub GetPropertyDataSize:
        unsafe extern "C" fn(Ref, ObjectID, libc::pid_t, Addr, u32, *const c_void, *mut u32) -> OSStatus,
    pub GetPropertyData: unsafe extern "C" fn(
        Ref,
        ObjectID,
        libc::pid_t,
        Addr,
        u32,
        *const c_void,
        u32,
        *mut u32,
        *mut c_void,
    ) -> OSStatus,
    pub SetPropertyData:
        unsafe extern "C" fn(Ref, ObjectID, libc::pid_t, Addr, u32, *const c_void, u32, *const c_void) -> OSStatus,
    pub StartIO: unsafe extern "C" fn(Ref, ObjectID, u32) -> OSStatus,
    pub StopIO: unsafe extern "C" fn(Ref, ObjectID, u32) -> OSStatus,
    pub GetZeroTimeStamp: unsafe extern "C" fn(Ref, ObjectID, u32, *mut f64, *mut u64, *mut u64) -> OSStatus,
    pub WillDoIOOperation: unsafe extern "C" fn(Ref, ObjectID, u32, u32, *mut u8, *mut u8) -> OSStatus,
    pub BeginIOOperation: unsafe extern "C" fn(Ref, ObjectID, u32, u32, u32, *const IOCycleInfo) -> OSStatus,
    pub DoIOOperation: unsafe extern "C" fn(
        Ref,
        ObjectID,
        ObjectID,
        u32,
        u32,
        u32,
        *const IOCycleInfo,
        *mut c_void,
        *mut c_void,
    ) -> OSStatus,
    pub EndIOOperation: unsafe extern "C" fn(Ref, ObjectID, u32, u32, u32, *const IOCycleInfo) -> OSStatus,
}
unsafe impl Sync for DriverInterface {}

pub static INTERFACE: DriverInterface = DriverInterface {
    _reserved: std::ptr::null_mut(),
    QueryInterface: query_interface,
    AddRef: add_ref,
    Release: release,
    Initialize: initialize,
    CreateDevice: create_device,
    DestroyDevice: destroy_device,
    AddDeviceClient: device_client,
    RemoveDeviceClient: device_client,
    PerformDeviceConfigurationChange: configuration_change,
    AbortDeviceConfigurationChange: configuration_change,
    HasProperty: has_property,
    IsPropertySettable: is_property_settable,
    GetPropertyDataSize: get_property_data_size,
    GetPropertyData: get_property_data,
    SetPropertyData: set_property_data,
    StartIO: start_io,
    StopIO: stop_io,
    GetZeroTimeStamp: get_zero_time_stamp,
    WillDoIOOperation: will_do_io_operation,
    BeginIOOperation: begin_end_io_operation,
    DoIOOperation: do_io_operation,
    EndIOOperation: begin_end_io_operation,
};

/// The driver reference handed to the host: a pointer to a pointer to the interface.
#[repr(transparent)]
struct InterfacePtr(#[allow(dead_code)] *const DriverInterface);
unsafe impl Sync for InterfacePtr {}
static DRIVER: InterfacePtr = InterfacePtr(&INTERFACE);

pub fn driver_ref() -> *mut c_void {
    &DRIVER as *const InterfacePtr as *mut c_void
}

const fn uuid(b: [u8; 16]) -> CFUUIDBytes {
    CFUUIDBytes {
        byte0: b[0],
        byte1: b[1],
        byte2: b[2],
        byte3: b[3],
        byte4: b[4],
        byte5: b[5],
        byte6: b[6],
        byte7: b[7],
        byte8: b[8],
        byte9: b[9],
        byte10: b[10],
        byte11: b[11],
        byte12: b[12],
        byte13: b[13],
        byte14: b[14],
        byte15: b[15],
    }
}

/// kAudioServerPlugInTypeUUID
const UUID_PLUGIN_TYPE: CFUUIDBytes =
    uuid([0x44, 0x3A, 0xBA, 0xB8, 0xE7, 0xB3, 0x49, 0x1A, 0xB9, 0x85, 0xBE, 0xB9, 0x18, 0x70, 0x30, 0xDB]);
/// kAudioServerPlugInDriverInterfaceUUID
const UUID_DRIVER_INTERFACE: CFUUIDBytes =
    uuid([0xEE, 0xA5, 0x77, 0x3D, 0xCC, 0x43, 0x49, 0xF1, 0x8E, 0x00, 0x8F, 0x96, 0xE7, 0xD2, 0x3B, 0x17]);
/// IUnknownUUID
const UUID_IUNKNOWN: CFUUIDBytes =
    uuid([0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

unsafe extern "C" {
    fn CFUUIDGetUUIDBytes(uuid: *const c_void) -> CFUUIDBytes;
}

/// CFPlugIn factory function, named in the bundle's Info.plist.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MacMixerDriver_Create(_allocator: *const c_void, requested_type: *const c_void) -> *mut c_void {
    if requested_type.is_null() || CFUUIDGetUUIDBytes(requested_type) != UUID_PLUGIN_TYPE {
        return std::ptr::null_mut();
    }
    driver_ref()
}

unsafe extern "C" fn query_interface(_d: Ref, iid: CFUUIDBytes, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return ERR_ILLEGAL;
    }
    if iid == UUID_IUNKNOWN || iid == UUID_DRIVER_INTERFACE {
        *out = driver_ref();
        0
    } else {
        *out = std::ptr::null_mut();
        0x8000_0004u32 as i32 // E_NOINTERFACE
    }
}

// The driver is a static singleton: reference counting has nothing to free.
unsafe extern "C" fn add_ref(_d: Ref) -> u32 {
    1
}
unsafe extern "C" fn release(_d: Ref) -> u32 {
    1
}

unsafe extern "C" fn initialize(_d: Ref, host: *const HostInterface) -> OSStatus {
    HOST.store(host as *mut HostInterface, Ordering::Release);
    rings();
    host_ticks_per_frame();
    0
}

unsafe extern "C" fn create_device(_d: Ref, _desc: *const c_void, _client: *const c_void, _out: *mut ObjectID) -> OSStatus {
    ERR_UNSUPPORTED
}
unsafe extern "C" fn destroy_device(_d: Ref, _device: ObjectID) -> OSStatus {
    ERR_UNSUPPORTED
}
unsafe extern "C" fn device_client(_d: Ref, _device: ObjectID, _client: *const c_void) -> OSStatus {
    0
}
unsafe extern "C" fn configuration_change(_d: Ref, _device: ObjectID, _action: u64, _info: *mut c_void) -> OSStatus {
    0
}

unsafe extern "C" fn has_property(_d: Ref, id: ObjectID, _pid: libc::pid_t, addr: Addr) -> u8 {
    if addr.is_null() {
        return 0;
    }
    get_property(id, &*addr, 0, std::ptr::null()).is_ok() as u8
}

unsafe extern "C" fn is_property_settable(_d: Ref, id: ObjectID, _pid: libc::pid_t, addr: Addr, out: *mut u8) -> OSStatus {
    if addr.is_null() || out.is_null() {
        return ERR_ILLEGAL;
    }
    if let Err(e) = get_property(id, &*addr, 0, std::ptr::null()) {
        return e;
    }
    *out = (id == PLUGIN_ID && (*addr).mSelector == PROP_DEVICE_NAMES) as u8;
    0
}

unsafe extern "C" fn get_property_data_size(
    _d: Ref,
    id: ObjectID,
    _pid: libc::pid_t,
    addr: Addr,
    qual_size: u32,
    qual: *const c_void,
    out: *mut u32,
) -> OSStatus {
    if addr.is_null() || out.is_null() {
        return ERR_ILLEGAL;
    }
    match get_property(id, &*addr, qual_size, qual) {
        Ok(Value::Bytes(b)) => *out = b.len() as u32,
        Ok(Value::Str(_)) => *out = size_of::<*const CFString>() as u32,
        Err(e) => return e,
    }
    0
}

unsafe extern "C" fn get_property_data(
    _d: Ref,
    id: ObjectID,
    _pid: libc::pid_t,
    addr: Addr,
    qual_size: u32,
    qual: *const c_void,
    in_size: u32,
    out_size: *mut u32,
    out: *mut c_void,
) -> OSStatus {
    if addr.is_null() || out_size.is_null() || out.is_null() {
        return ERR_ILLEGAL;
    }
    match get_property(id, &*addr, qual_size, qual) {
        Ok(Value::Bytes(b)) => {
            // Lists may be read partially; everything else must fit.
            let n = b.len().min(in_size as usize);
            std::ptr::copy_nonoverlapping(b.as_ptr(), out as *mut u8, n);
            *out_size = n as u32;
        }
        Ok(Value::Str(s)) => {
            if (in_size as usize) < size_of::<*const CFString>() {
                return ERR_BAD_SIZE;
            }
            // Ownership of the string passes to the host.
            let cf = CFString::from_str(&s);
            *(out as *mut *const CFString) = CFRetained::into_raw(cf).as_ptr();
            *out_size = size_of::<*const CFString>() as u32;
        }
        Err(e) => return e,
    }
    0
}

unsafe extern "C" fn set_property_data(
    _d: Ref,
    id: ObjectID,
    _pid: libc::pid_t,
    addr: Addr,
    _qual_size: u32,
    _qual: *const c_void,
    in_size: u32,
    data: *const c_void,
) -> OSStatus {
    if addr.is_null() {
        return ERR_ILLEGAL;
    }
    if classify(id).is_none() {
        return ERR_BAD_OBJECT;
    }
    if id == PLUGIN_ID && (*addr).mSelector == PROP_DEVICE_NAMES {
        if in_size as usize != size_of::<*const CFString>() || data.is_null() {
            return ERR_BAD_SIZE;
        }
        let s = *(data as *const *const CFString);
        let text = if s.is_null() { String::new() } else { (*s).to_string() };
        set_device_names(&text);
        return 0;
    }
    match get_property(id, &*addr, 0, std::ptr::null()) {
        Ok(_) => ERR_UNSUPPORTED,
        Err(e) => e,
    }
}

unsafe extern "C" fn start_io(_d: Ref, device: ObjectID, _client: u32) -> OSStatus {
    let Some(slot) = slot_of_device(device) else { return ERR_BAD_OBJECT };
    let c = &CLOCKS[slot];
    if c.io_count.fetch_add(1, Ordering::AcqRel) == 0 {
        c.periods.store(0, Ordering::Relaxed);
        c.anchor_host_time.store(mach_absolute_time(), Ordering::Relaxed);
        // Start from silence. No IO thread is running for this device yet.
        (*rings()[slot].0.get()).fill(0.0);
    }
    0
}

unsafe extern "C" fn stop_io(_d: Ref, device: ObjectID, _client: u32) -> OSStatus {
    let Some(slot) = slot_of_device(device) else { return ERR_BAD_OBJECT };
    let c = &CLOCKS[slot];
    let _ = c.io_count.try_update(Ordering::AcqRel, Ordering::Relaxed, |n| n.checked_sub(1));
    0
}

unsafe extern "C" fn get_zero_time_stamp(
    _d: Ref,
    device: ObjectID,
    _client: u32,
    out_sample_time: *mut f64,
    out_host_time: *mut u64,
    out_seed: *mut u64,
) -> OSStatus {
    let Some(slot) = slot_of_device(device) else { return ERR_BAD_OBJECT };
    let c = &CLOCKS[slot];
    let ticks_per_period = host_ticks_per_frame() * RING as f64;
    let anchor = c.anchor_host_time.load(Ordering::Relaxed);
    let mut periods = c.periods.load(Ordering::Relaxed);
    let now = mach_absolute_time();
    // Advance to the most recent period boundary that has already passed.
    while anchor + ((periods + 1) as f64 * ticks_per_period) as u64 <= now {
        periods += 1;
    }
    c.periods.store(periods, Ordering::Relaxed);
    *out_sample_time = (periods * RING as u64) as f64;
    *out_host_time = anchor + (periods as f64 * ticks_per_period) as u64;
    *out_seed = 1;
    0
}

unsafe extern "C" fn will_do_io_operation(
    _d: Ref,
    _device: ObjectID,
    _client: u32,
    operation: u32,
    out_will_do: *mut u8,
    out_in_place: *mut u8,
) -> OSStatus {
    let will = operation == OP_READ_INPUT || operation == OP_WRITE_MIX;
    if !out_will_do.is_null() {
        *out_will_do = will as u8;
    }
    if !out_in_place.is_null() {
        *out_in_place = 1;
    }
    0
}

unsafe extern "C" fn begin_end_io_operation(
    _d: Ref,
    _device: ObjectID,
    _client: u32,
    _operation: u32,
    _frames: u32,
    _info: *const IOCycleInfo,
) -> OSStatus {
    0
}

unsafe extern "C" fn do_io_operation(
    _d: Ref,
    device: ObjectID,
    _stream: ObjectID,
    _client: u32,
    operation: u32,
    frames: u32,
    info: *const IOCycleInfo,
    main_buffer: *mut c_void,
    _secondary: *mut c_void,
) -> OSStatus {
    let Some(slot) = slot_of_device(device) else { return ERR_BAD_OBJECT };
    if info.is_null() || main_buffer.is_null() {
        return 0;
    }
    let ring = &mut *rings()[slot].0.get();
    let buf = std::slice::from_raw_parts_mut(main_buffer as *mut f32, frames as usize * CHANNELS);
    match operation {
        OP_WRITE_MIX => {
            let start = ((*info).mOutputTime.mSampleTime as i64).rem_euclid(RING as i64) as usize;
            for (f, frame) in buf.chunks_exact(CHANNELS).enumerate() {
                let i = ((start + f) % RING) * CHANNELS;
                ring[i..i + CHANNELS].copy_from_slice(frame);
            }
        }
        OP_READ_INPUT => {
            let start = ((*info).mInputTime.mSampleTime as i64).rem_euclid(RING as i64) as usize;
            for (f, frame) in buf.chunks_exact_mut(CHANNELS).enumerate() {
                let i = ((start + f) % RING) * CHANNELS;
                frame.copy_from_slice(&ring[i..i + CHANNELS]);
                // Consumed: clear it so stale audio never loops if the writer stops.
                ring[i..i + CHANNELS].fill(0.0);
            }
        }
        _ => {}
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn changed(_h: *const HostInterface, _o: ObjectID, _n: u32, _a: Addr) -> OSStatus {
        NOTIFICATIONS.fetch_add(1, Ordering::Relaxed);
        0
    }

    fn addr(sel: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress { mSelector: sel, mScope: fourcc(b"glob"), mElement: 0 }
    }

    unsafe fn get_bytes(id: ObjectID, sel: u32) -> Result<Vec<u8>, OSStatus> {
        let d = driver_ref();
        let a = addr(sel);
        let mut size = 0u32;
        let st = (INTERFACE.GetPropertyDataSize)(d, id, 0, &a, 0, std::ptr::null(), &mut size);
        if st != 0 {
            return Err(st);
        }
        let mut buf = vec![0u8; size as usize];
        let mut out = 0u32;
        let st = (INTERFACE.GetPropertyData)(d, id, 0, &a, 0, std::ptr::null(), size, &mut out, buf.as_mut_ptr().cast());
        if st != 0 {
            return Err(st);
        }
        buf.truncate(out as usize);
        Ok(buf)
    }

    unsafe fn get_u32s(id: ObjectID, sel: u32) -> Vec<u32> {
        get_bytes(id, sel).unwrap().chunks_exact(4).map(|c| u32::from_ne_bytes(c.try_into().unwrap())).collect()
    }

    unsafe fn get_string(id: ObjectID, sel: u32) -> String {
        let b = get_bytes(id, sel).unwrap();
        let p = usize::from_ne_bytes(b.try_into().unwrap()) as *const CFString;
        let s = CFRetained::from_raw(std::ptr::NonNull::new(p as *mut CFString).unwrap());
        s.to_string()
    }

    unsafe fn set_names(text: &str) -> OSStatus {
        let s = CFString::from_str(text);
        let p: *const CFString = &*s;
        let a = addr(PROP_DEVICE_NAMES);
        (INTERFACE.SetPropertyData)(driver_ref(), PLUGIN_ID, 0, &a, 0, std::ptr::null(), 8, (&p as *const *const CFString).cast())
    }

    fn cycle(input_time: f64, output_time: f64) -> IOCycleInfo {
        let mut info: IOCycleInfo = unsafe { std::mem::zeroed() };
        info.mInputTime.mSampleTime = input_time;
        info.mOutputTime.mSampleTime = output_time;
        info
    }

    // One test: the driver state is global, so the steps must run in order.
    #[test]
    fn driver_end_to_end() {
        unsafe {
            let d = driver_ref();
            let host = HostInterface {
                PropertiesChanged: changed,
                CopyFromStorage: std::ptr::null(),
                WriteToStorage: std::ptr::null(),
                DeleteFromStorage: std::ptr::null(),
                RequestDeviceConfigurationChange: std::ptr::null(),
            };

            // COM plumbing.
            let mut iface: *mut c_void = std::ptr::null_mut();
            assert_eq!((INTERFACE.QueryInterface)(d, UUID_DRIVER_INTERFACE, &mut iface), 0);
            assert_eq!(iface, d);
            assert_ne!((INTERFACE.QueryInterface)(d, UUID_PLUGIN_TYPE, &mut iface), 0);
            assert_eq!((INTERFACE.Initialize)(d, &host), 0);

            // No devices at first.
            assert!(get_u32s(PLUGIN_ID, P_PLUGIN_DEVICE_LIST).is_empty());
            assert_eq!(get_bytes(device_id(0), P_NAME), Err(ERR_BAD_OBJECT));

            // Create slots 0 and 2.
            assert_eq!(set_names("MacMixer: Main\n\nMacMixer: Stream"), 0);
            assert_eq!(NOTIFICATIONS.load(Ordering::Relaxed), 1);
            assert_eq!(get_u32s(PLUGIN_ID, P_PLUGIN_DEVICE_LIST), vec![device_id(0), device_id(2)]);
            assert_eq!(get_string(device_id(2), P_NAME), "MacMixer: Stream");
            assert_eq!(get_string(device_id(2), P_DEV_UID), "MacMixer_UID_2");
            assert_eq!(get_string(PLUGIN_ID, PROP_DEVICE_NAMES).lines().next(), Some("MacMixer: Main"));
            assert_eq!(get_bytes(device_id(1), P_NAME), Err(ERR_BAD_OBJECT));

            // UID lookup.
            let uid = CFString::from_str("MacMixer_UID_2");
            let p: *const CFString = &*uid;
            let a = addr(P_PLUGIN_UID_TO_DEVICE);
            let (mut found, mut out) = (0u32, 0u32);
            let st = (INTERFACE.GetPropertyData)(
                d,
                PLUGIN_ID,
                0,
                &a,
                8,
                (&p as *const *const CFString).cast(),
                4,
                &mut out,
                (&mut found as *mut u32).cast(),
            );
            assert_eq!((st, found), (0, device_id(2)));

            // Device and stream description.
            assert_eq!(get_u32s(device_id(0), P_DEV_STREAMS), vec![input_stream_id(0), output_stream_id(0)]);
            assert_eq!(get_u32s(input_stream_id(0), P_STREAM_DIRECTION), vec![1]);
            assert_eq!(get_u32s(output_stream_id(0), P_STREAM_DIRECTION), vec![0]);
            let fmt = get_bytes(input_stream_id(0), P_STREAM_VIRTUAL_FORMAT).unwrap();
            assert_eq!(fmt.len(), 40);
            assert_eq!(f64::from_ne_bytes(fmt[..8].try_into().unwrap()), SAMPLE_RATE);
            assert_eq!(get_bytes(input_stream_id(0), P_STREAM_AVAILABLE_VIRTUAL).unwrap().len(), 56);
            assert_eq!(get_bytes(device_id(0), fourcc(b"xxxx")), Err(ERR_UNKNOWN_PROPERTY));
            let mut settable = 9u8;
            assert_eq!((INTERFACE.IsPropertySettable)(d, PLUGIN_ID, 0, &addr(PROP_DEVICE_NAMES), &mut settable), 0);
            assert_eq!(settable, 1);
            assert_eq!((INTERFACE.IsPropertySettable)(d, device_id(0), 0, &addr(P_DEV_SAMPLE_RATE), &mut settable), 0);
            assert_eq!(settable, 0);

            // Rename only: one more notification, same device list.
            assert_eq!(set_names("MacMixer: Main\n\nMacMixer: Live"), 0);
            assert_eq!(NOTIFICATIONS.load(Ordering::Relaxed), 2);
            assert_eq!(get_string(device_id(2), P_NAME), "MacMixer: Live");

            // Clock.
            assert_eq!(get_u32s(device_id(0), P_DEV_RUNNING), vec![0]);
            assert_eq!((INTERFACE.StartIO)(d, device_id(0), 1), 0);
            assert_eq!(get_u32s(device_id(0), P_DEV_RUNNING), vec![1]);
            let (mut st, mut ht, mut seed) = (-1.0f64, 0u64, 0u64);
            assert_eq!((INTERFACE.GetZeroTimeStamp)(d, device_id(0), 1, &mut st, &mut ht, &mut seed), 0);
            assert_eq!((st, seed), (0.0, 1));
            assert!(ht > 0);

            // Loopback: what is written at a sample time is read back at that time, once.
            // The range wraps around the end of the ring.
            let t = (RING * 3 - 100) as f64;
            let frames = 256u32;
            let mut out_buf: Vec<f32> = (0..frames * 2).map(|i| i as f32 + 1.0).collect();
            let info = cycle(0.0, t);
            assert_eq!(
                (INTERFACE.DoIOOperation)(d, device_id(0), output_stream_id(0), 1, OP_WRITE_MIX, frames, &info, out_buf.as_mut_ptr().cast(), std::ptr::null_mut()),
                0
            );
            let mut in_buf = vec![0.0f32; frames as usize * 2];
            let info = cycle(t, 0.0);
            (INTERFACE.DoIOOperation)(d, device_id(0), input_stream_id(0), 1, OP_READ_INPUT, frames, &info, in_buf.as_mut_ptr().cast(), std::ptr::null_mut());
            assert_eq!(in_buf, out_buf);
            (INTERFACE.DoIOOperation)(d, device_id(0), input_stream_id(0), 1, OP_READ_INPUT, frames, &info, in_buf.as_mut_ptr().cast(), std::ptr::null_mut());
            assert!(in_buf.iter().all(|&x| x == 0.0));

            // Devices are independent.
            let info = cycle(0.0, 0.0);
            (INTERFACE.DoIOOperation)(d, device_id(0), output_stream_id(0), 1, OP_WRITE_MIX, frames, &info, out_buf.as_mut_ptr().cast(), std::ptr::null_mut());
            (INTERFACE.DoIOOperation)(d, device_id(2), input_stream_id(2), 1, OP_READ_INPUT, frames, &info, in_buf.as_mut_ptr().cast(), std::ptr::null_mut());
            assert!(in_buf.iter().all(|&x| x == 0.0));

            assert_eq!((INTERFACE.StopIO)(d, device_id(0), 1), 0);
            assert_eq!(get_u32s(device_id(0), P_DEV_RUNNING), vec![0]);

            // Remove everything.
            assert_eq!(set_names(""), 0);
            assert!(get_u32s(PLUGIN_ID, P_PLUGIN_DEVICE_LIST).is_empty());
        }
    }
}
