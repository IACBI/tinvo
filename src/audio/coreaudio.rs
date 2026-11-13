//! macOS audio: output through an output audio unit on the HAL, input
//! through an AudioQueue.
//!
//! Output renders on the HAL's own I/O thread, straight into the device's
//! buffer at its sample rate and period (which low-latency mode shortens).
//! Without a chosen device it uses the default output unit, which follows
//! the system default; with one, the HAL output unit on that device. Input
//! uses an AudioQueue, whose extra buffering does not matter for recording.
//! TINVO's audio thread only owns the unit or queue and waits for the stop
//! request. Devices are the HAL's devices, identified by their UID.

use super::{
    AudioCallback, CaptureCallback, Device, InputInfo, StreamConfig, StreamHealth, StreamInfo, spawn_audio_thread,
};
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const SAMPLE_RATE: f64 = 48_000.0;
const BUFFER_COUNT: usize = 3;
/// Frames per input buffer (about 43 ms): recording does not need it short.
const CAPTURE_BUFFER_FRAMES: u32 = 2048;
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 8;

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioQueueBuffer {
    audio_data_bytes_capacity: u32,
    audio_data: *mut c_void,
    audio_data_byte_size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_description_count: u32,
}

type AudioQueueRef = *mut c_void;
/// (user data, queue, buffer, start time, packet description count, packet descriptions).
type InputCallback =
    unsafe extern "C" fn(*mut c_void, AudioQueueRef, *mut AudioQueueBuffer, *const c_void, u32, *const c_void);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewInput(
        format: *const AudioStreamBasicDescription,
        callback: InputCallback,
        user_data: *mut c_void,
        run_loop: *mut c_void,
        run_loop_mode: *const c_void,
        flags: u32,
        out_queue: *mut AudioQueueRef,
    ) -> i32;
    fn AudioQueueAllocateBuffer(queue: AudioQueueRef, bytes: u32, out: *mut *mut AudioQueueBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(
        queue: AudioQueueRef,
        buffer: *mut AudioQueueBuffer,
        num_packet_descs: u32,
        packet_descs: *const c_void,
    ) -> i32;
    fn AudioQueueStart(queue: AudioQueueRef, start_time: *const c_void) -> i32;
    fn AudioQueueStop(queue: AudioQueueRef, immediate: u8) -> i32;
    fn AudioQueueDispose(queue: AudioQueueRef, immediate: u8) -> i32;
    fn AudioQueueSetProperty(queue: AudioQueueRef, id: u32, data: *const c_void, size: u32) -> i32;
}

pub(crate) type CFStringRef = *const c_void;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringGetLength(s: CFStringRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(s: CFStringRef, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFRelease(cf: *const c_void);
}

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> i32;
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
}

const SYSTEM_OBJECT: u32 = 1;
const fn fourcc(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*b)
}
const PROP_DEVICES: u32 = fourcc(b"dev#");
const PROP_DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
const PROP_DEFAULT_INPUT: u32 = fourcc(b"dIn ");
const PROP_STREAMS: u32 = fourcc(b"stm#");
const PROP_UID: u32 = fourcc(b"uid ");
const PROP_NAME: u32 = fourcc(b"lnam");
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");
const SCOPE_INPUT: u32 = fourcc(b"inpt");
const QUEUE_CURRENT_DEVICE: u32 = fourcc(b"aqcd");
const PROP_NOMINAL_RATE: u32 = fourcc(b"nsrt");

fn address(selector: u32, scope: u32) -> PropertyAddress {
    PropertyAddress { selector, scope, element: 0 }
}

/// Copies and releases a CFString the system handed us.
pub(crate) unsafe fn take_cf_string(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: `s` is a live CFString we own; released once below.
    unsafe {
        let cap = CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), K_CF_STRING_ENCODING_UTF8) + 1;
        let mut buf = vec![0 as c_char; cap.max(1) as usize];
        let ok = CFStringGetCString(s, buf.as_mut_ptr(), cap, K_CF_STRING_ENCODING_UTF8);
        CFRelease(s);
        (ok != 0).then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

fn string_property(device: u32, selector: u32) -> Option<String> {
    let mut s: CFStringRef = ptr::null();
    let mut size = size_of::<CFStringRef>() as u32;
    // SAFETY: the property is a CFStringRef the caller must release.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address(selector, SCOPE_GLOBAL),
            0,
            ptr::null(),
            &mut size,
            (&raw mut s).cast(),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: a CFString we now own.
    unsafe { take_cf_string(s) }
}

/// Whether `device` has streams in `scope` (output or input).
fn has_streams(device: u32, scope: u32) -> bool {
    let mut size = 0u32;
    // SAFETY: valid address and out-pointer.
    let status =
        unsafe { AudioObjectGetPropertyDataSize(device, &address(PROP_STREAMS, scope), 0, ptr::null(), &mut size) };
    status == 0 && size > 0
}

fn default_device() -> Option<u32> {
    default_for(PROP_DEFAULT_OUTPUT)
}

/// The default device `selector` names (`dOut` or `dIn `).
fn default_for(selector: u32) -> Option<u32> {
    let mut id = 0u32;
    let mut size = 4u32;
    // SAFETY: the property is one AudioObjectID.
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &address(selector, SCOPE_GLOBAL),
            0,
            ptr::null(),
            &mut size,
            (&raw mut id).cast(),
        )
    };
    (status == 0 && id != 0).then_some(id)
}

/// Lists the devices with output streams.
pub fn devices() -> Result<Vec<Device>, String> {
    list(SCOPE_OUTPUT, PROP_DEFAULT_OUTPUT)
}

/// Lists the devices with input streams.
pub fn input_devices() -> Result<Vec<Device>, String> {
    list(SCOPE_INPUT, PROP_DEFAULT_INPUT)
}

fn list(scope: u32, default_selector: u32) -> Result<Vec<Device>, String> {
    let addr = address(PROP_DEVICES, SCOPE_GLOBAL);
    let mut size = 0u32;
    // SAFETY: valid address and out-pointer.
    let status = unsafe { AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &addr, 0, ptr::null(), &mut size) };
    if status != 0 {
        return Err(format!("cannot list audio devices (OSStatus {status})"));
    }
    let mut ids = vec![0u32; size as usize / 4];
    // SAFETY: `ids` holds `size` bytes of AudioObjectIDs.
    let status =
        unsafe { AudioObjectGetPropertyData(SYSTEM_OBJECT, &addr, 0, ptr::null(), &mut size, ids.as_mut_ptr().cast()) };
    if status != 0 {
        return Err(format!("cannot list audio devices (OSStatus {status})"));
    }
    ids.truncate(size as usize / 4);
    let default = default_for(default_selector);
    Ok(ids
        .into_iter()
        .filter(|&d| has_streams(d, scope))
        .filter_map(|d| {
            let id = string_property(d, PROP_UID)?;
            let name = string_property(d, PROP_NAME).unwrap_or_else(|| id.clone());
            Some(Device { id, name, is_default: Some(d) == default })
        })
        .collect())
}

/// A new CFString holding `s` (null if it cannot be made); the caller
/// releases it with [`release`].
pub(crate) fn new_cf_string(s: &str) -> CFStringRef {
    let Ok(c) = CString::new(s) else { return ptr::null() };
    // SAFETY: a NUL-terminated UTF-8 string.
    unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8) }
}

/// Releases a Core Foundation object we own.
pub(crate) unsafe fn release(cf: *const c_void) {
    // SAFETY: the caller owns one reference to `cf`.
    unsafe { CFRelease(cf) }
}

/// Points the queue at the device with `uid`.
fn set_queue_device(queue: AudioQueueRef, uid: &str) -> Result<(), String> {
    let c = CString::new(uid).map_err(|_| format!("bad device id '{uid}'"))?;
    // SAFETY: a NUL-terminated UTF-8 string; the CFString is released below.
    let s = unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8) };
    if s.is_null() {
        return Err(format!("bad device id '{uid}'"));
    }
    // SAFETY: the property value is a CFStringRef; the queue retains it.
    let status = unsafe {
        let status =
            AudioQueueSetProperty(queue, QUEUE_CURRENT_DEVICE, (&raw const s).cast(), size_of::<CFStringRef>() as u32);
        CFRelease(s);
        status
    };
    if status != 0 {
        return Err(format!("opening the chosen output device (is it still connected?) (OSStatus {status})"));
    }
    Ok(())
}

const AU_TYPE_OUTPUT: u32 = fourcc(b"auou");
const AU_SUBTYPE_HAL_OUTPUT: u32 = fourcc(b"ahal");
const AU_SUBTYPE_DEFAULT_OUTPUT: u32 = fourcc(b"def ");
const AU_MANUFACTURER_APPLE: u32 = fourcc(b"appl");
const AU_SCOPE_GLOBAL: u32 = 0;
const AU_SCOPE_INPUT: u32 = 1;
const AU_SCOPE_OUTPUT: u32 = 2;
const AU_PROPERTY_STREAM_FORMAT: u32 = 8;
const AU_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
const AU_PROPERTY_CURRENT_DEVICE: u32 = 2000;
const PROP_BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
const PROP_BUFFER_FRAME_SIZE_RANGE: u32 = fourcc(b"fsz#");
const PROP_IS_ALIVE: u32 = fourcc(b"livn");
const PROP_TRANSLATE_UID: u32 = fourcc(b"uidd");
/// The period asked for in low-latency mode (about 1.3 ms at 48 kHz),
/// raised to the device's minimum where that is larger.
const LOW_LATENCY_FRAMES: u32 = 64;

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

type AudioUnit = *mut c_void;
/// (refcon, action flags, time stamp, bus, frames, buffers).
type RenderCallback = unsafe extern "C" fn(*mut c_void, *mut u32, *const c_void, u32, u32, *mut AudioBufferList) -> i32;
type ListenerProc = unsafe extern "C" fn(u32, u32, *const PropertyAddress, *mut c_void) -> i32;

#[repr(C)]
struct RenderCallbackStruct {
    proc_: RenderCallback,
    refcon: *mut c_void,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(after: *mut c_void, desc: *const AudioComponentDescription) -> *mut c_void;
    fn AudioComponentInstanceNew(component: *mut c_void, out: *mut AudioUnit) -> i32;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> i32;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> i32;
    fn AudioUnitGetProperty(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn AudioUnitInitialize(unit: AudioUnit) -> i32;
    fn AudioUnitUninitialize(unit: AudioUnit) -> i32;
    fn AudioOutputUnitStart(unit: AudioUnit) -> i32;
    fn AudioOutputUnitStop(unit: AudioUnit) -> i32;
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectSetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> i32;
    fn AudioObjectAddPropertyListener(
        object: u32,
        address: *const PropertyAddress,
        listener: ListenerProc,
        client: *mut c_void,
    ) -> i32;
    fn AudioObjectRemovePropertyListener(
        object: u32,
        address: *const PropertyAddress,
        listener: ListenerProc,
        client: *mut c_void,
    ) -> i32;
}

/// The HAL device with `uid`, if one is connected.
fn device_for_uid(uid: &str) -> Option<u32> {
    let s = new_cf_string(uid);
    if s.is_null() {
        return None;
    }
    let mut id = 0u32;
    let mut size = 4u32;
    // SAFETY: the qualifier is one CFStringRef, the data one AudioObjectID;
    // the string is released once below.
    let status = unsafe {
        let status = AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &address(PROP_TRANSLATE_UID, SCOPE_GLOBAL),
            size_of::<CFStringRef>() as u32,
            (&raw const s).cast(),
            &mut size,
            (&raw mut id).cast(),
        );
        release(s);
        status
    };
    (status == 0 && id != 0).then_some(id)
}

/// The sample rate `device` runs at, if it reports a usable one.
fn nominal_rate(device: u32) -> Option<f64> {
    let mut rate = 0f64;
    let mut size = size_of::<f64>() as u32;
    // SAFETY: the property is one Float64.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address(PROP_NOMINAL_RATE, SCOPE_GLOBAL),
            0,
            ptr::null(),
            &mut size,
            (&raw mut rate).cast(),
        )
    };
    (status == 0 && rate.is_finite() && rate >= 8_000.0).then_some(rate)
}

/// A `u32` property of `device` in the global scope.
fn u32_property(device: u32, selector: u32) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    // SAFETY: the property is one UInt32.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address(selector, SCOPE_GLOBAL),
            0,
            ptr::null(),
            &mut size,
            (&raw mut v).cast(),
        )
    };
    (status == 0).then_some(v)
}

/// Sets this process's period on `device` (the HAL keeps one per process).
fn set_period(device: u32, frames: u32) -> bool {
    // SAFETY: the property is one UInt32.
    let status = unsafe {
        AudioObjectSetPropertyData(
            device,
            &address(PROP_BUFFER_FRAME_SIZE, SCOPE_GLOBAL),
            0,
            ptr::null(),
            4,
            (&raw const frames).cast(),
        )
    };
    status == 0
}

/// Asks `device` for its smallest period, at least [`LOW_LATENCY_FRAMES`].
/// Returns the period it had before, if the period is now smaller.
fn request_low_latency(device: u32) -> Option<u32> {
    let mut range = [0f64; 2];
    let mut size = size_of::<[f64; 2]>() as u32;
    // SAFETY: the property is an AudioValueRange (two Float64s).
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address(PROP_BUFFER_FRAME_SIZE_RANGE, SCOPE_GLOBAL),
            0,
            ptr::null(),
            &mut size,
            range.as_mut_ptr().cast(),
        )
    };
    let before = u32_property(device, PROP_BUFFER_FRAME_SIZE)?;
    if status != 0 {
        return None;
    }
    let want = LOW_LATENCY_FRAMES.max(range[0].ceil() as u32);
    if want >= before || !set_period(device, want) {
        return None;
    }
    if u32_property(device, PROP_BUFFER_FRAME_SIZE).is_some_and(|now| now < before) {
        Some(before)
    } else {
        set_period(device, before);
        None
    }
}

/// Marks the stream dead when the HAL reports its device gone.
unsafe extern "C" fn device_changed(
    device: u32,
    _count: u32,
    _addresses: *const PropertyAddress,
    dead: *mut c_void,
) -> i32 {
    if u32_property(device, PROP_IS_ALIVE) == Some(0) {
        // SAFETY: `dead` is the flag `start_unit` registered, which is never
        // freed.
        unsafe { &*(dead as *const AtomicBool) }.store(true, Ordering::Release);
    }
    0
}

struct State<C> {
    callback: C,
}

unsafe extern "C" fn render<C: AudioCallback>(
    user: *mut c_void,
    _flags: *mut u32,
    _time: *const c_void,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: `user` is the `State<C>` boxed in `open`, alive until the unit
    // is stopped and disposed; the unit renders on one thread.
    let state = unsafe { &mut *(user as *mut State<C>) };
    if data.is_null() {
        return 0;
    }
    // SAFETY: the unit hands us a list of `count` buffers for this cycle.
    let buffers = unsafe {
        let list = &mut *data;
        std::slice::from_raw_parts_mut(list.buffers.as_mut_ptr(), list.count as usize)
    };
    let bytes = frames as usize * 8;
    match buffers {
        // The format set in `open`: one buffer of interleaved stereo f32.
        [b] if b.channels == 2 && b.data_byte_size as usize >= bytes && !b.data.is_null() => {
            // SAFETY: the buffer holds `frames` interleaved stereo frames.
            let out = unsafe { std::slice::from_raw_parts_mut(b.data as *mut f32, frames as usize * 2) };
            state.callback.process(out);
            for s in out.iter_mut() {
                *s = super::sanitize(*s);
            }
        }
        // Anything else is not what was negotiated; play silence rather
        // than guess at the layout.
        _ => {
            for b in buffers.iter_mut().filter(|b| !b.data.is_null()) {
                // SAFETY: each buffer holds `data_byte_size` bytes.
                unsafe { ptr::write_bytes(b.data as *mut u8, 0, b.data_byte_size as usize) };
            }
        }
    }
    0
}

/// Everything `open` set up, torn down in reverse on drop.
struct Unit {
    unit: AudioUnit,
    initialized: bool,
    started: bool,
    listener: Option<(u32, *mut c_void)>,
    /// The device and period to put back: the period setting outlives the
    /// stream, so a later stream without low latency would inherit it.
    restore: Option<(u32, u32)>,
    closed: bool,
}

impl Unit {
    /// Stops and disposes the unit. Returns whether the render callback is
    /// certainly finished: AudioOutputUnitStop called from a thread other
    /// than the I/O thread returns only once the HAL has stopped calling
    /// it, unless the stop itself fails.
    fn close(&mut self) -> bool {
        if self.closed {
            return true;
        }
        self.closed = true;
        // SAFETY: each step undoes one that `open` completed.
        unsafe {
            let quiet = !self.started || AudioOutputUnitStop(self.unit) == 0;
            if self.initialized {
                AudioUnitUninitialize(self.unit);
            }
            AudioComponentInstanceDispose(self.unit);
            if let Some((device, dead)) = self.listener {
                AudioObjectRemovePropertyListener(device, &address(PROP_IS_ALIVE, SCOPE_GLOBAL), device_changed, dead);
            }
            if let Some((device, frames)) = self.restore {
                set_period(device, frames);
            }
            quiet
        }
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        self.close();
    }
}

fn check(status: i32, what: &str) -> Result<(), String> {
    if status == 0 { Ok(()) } else { Err(format!("{what} failed (OSStatus {status})")) }
}

/// Opens an output unit: the default output unit (which follows the system
/// default) without a device, the HAL output unit on the device otherwise.
fn new_unit(device: Option<u32>) -> Result<Unit, String> {
    let desc = AudioComponentDescription {
        component_type: AU_TYPE_OUTPUT,
        component_sub_type: if device.is_some() { AU_SUBTYPE_HAL_OUTPUT } else { AU_SUBTYPE_DEFAULT_OUTPUT },
        component_manufacturer: AU_MANUFACTURER_APPLE,
        component_flags: 0,
        component_flags_mask: 0,
    };
    // SAFETY: a valid description; null starts the search at the beginning.
    let component = unsafe { AudioComponentFindNext(ptr::null_mut(), &desc) };
    if component.is_null() {
        return Err("no CoreAudio output unit found".into());
    }
    let mut unit: AudioUnit = ptr::null_mut();
    // SAFETY: a component from AudioComponentFindNext and a valid out-pointer.
    check(unsafe { AudioComponentInstanceNew(component, &mut unit) }, "AudioComponentInstanceNew")?;
    let unit = Unit { unit, initialized: false, started: false, listener: None, restore: None, closed: false };
    if let Some(id) = device {
        // SAFETY: the property is one AudioObjectID.
        let status = unsafe {
            AudioUnitSetProperty(unit.unit, AU_PROPERTY_CURRENT_DEVICE, AU_SCOPE_GLOBAL, 0, (&raw const id).cast(), 4)
        };
        check(status, "opening the chosen output device")?;
    }
    Ok(unit)
}

/// Sets up and starts `unit` rendering through `state`; `dead` is raised if
/// the chosen device disappears.
fn start_unit<C: AudioCallback>(
    unit: &mut Unit,
    state: *mut State<C>,
    chosen: Option<u32>,
    dead: &'static AtomicBool,
) -> Result<(), String> {
    let callback = RenderCallbackStruct { proc_: render::<C>, refcon: state.cast() };
    // SAFETY: the property is one AURenderCallbackStruct; `state` outlives
    // the unit.
    let status = unsafe {
        AudioUnitSetProperty(
            unit.unit,
            AU_PROPERTY_SET_RENDER_CALLBACK,
            AU_SCOPE_INPUT,
            0,
            (&raw const callback).cast(),
            size_of::<RenderCallbackStruct>() as u32,
        )
    };
    check(status, "setting the render callback")?;
    if let Some(id) = chosen {
        let flag = (dead as *const AtomicBool).cast_mut().cast();
        // SAFETY: `dead` is never freed, so a notification racing with the
        // listener's removal still finds it.
        let status =
            unsafe { AudioObjectAddPropertyListener(id, &address(PROP_IS_ALIVE, SCOPE_GLOBAL), device_changed, flag) };
        if status == 0 {
            unit.listener = Some((id, flag));
        }
    }
    // SAFETY: a configured unit.
    check(unsafe { AudioUnitInitialize(unit.unit) }, "AudioUnitInitialize")?;
    unit.initialized = true;
    // SAFETY: an initialized unit.
    check(unsafe { AudioOutputUnitStart(unit.unit) }, "AudioOutputUnitStart")?;
    unit.started = true;
    Ok(())
}

fn float_stereo(sample_rate: f64) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        sample_rate,
        format_id: K_AUDIO_FORMAT_LINEAR_PCM,
        format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
        bytes_per_packet: 8,
        frames_per_packet: 1,
        bytes_per_frame: 8,
        channels_per_frame: 2,
        bits_per_channel: 32,
        reserved: 0,
    }
}

/// Plays through an output audio unit, which renders on the HAL's I/O
/// thread with the device's own period.
pub fn open<F, C>(
    config: StreamConfig,
    factory: F,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
) -> Result<(StreamInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32) -> C + Send + 'static,
    C: AudioCallback,
{
    spawn_audio_thread(move |report| {
        let chosen = match &config.device {
            Some(uid) => match device_for_uid(uid) {
                Some(id) => Some(id),
                None => return report(Err("opening the chosen output device (is it still connected?)".into())),
            },
            None => None,
        };
        let mut unit = match new_unit(chosen) {
            Ok(u) => u,
            Err(e) => return report(Err(e)),
        };
        let device = chosen.or_else(default_device);
        // The device side of the unit carries the hardware's format; feeding
        // the unit at the hardware's rate means it never resamples.
        let mut hardware = float_stereo(0.0);
        let mut size = size_of::<AudioStreamBasicDescription>() as u32;
        // SAFETY: the property is one AudioStreamBasicDescription.
        let status = unsafe {
            AudioUnitGetProperty(
                unit.unit,
                AU_PROPERTY_STREAM_FORMAT,
                AU_SCOPE_OUTPUT,
                0,
                (&raw mut hardware).cast(),
                &mut size,
            )
        };
        let rate = if status == 0 && hardware.sample_rate >= 8_000.0 { hardware.sample_rate } else { SAMPLE_RATE };
        let format = float_stereo(rate);
        // SAFETY: the property is one AudioStreamBasicDescription.
        let status = unsafe {
            AudioUnitSetProperty(
                unit.unit,
                AU_PROPERTY_STREAM_FORMAT,
                AU_SCOPE_INPUT,
                0,
                (&raw const format).cast(),
                size_of::<AudioStreamBasicDescription>() as u32,
            )
        };
        if let Err(e) = check(status, "setting the output format") {
            return report(Err(e));
        }
        if config.low_latency {
            unit.restore = device.and_then(|d| request_low_latency(d).map(|before| (d, before)));
        }
        let low_latency = unit.restore.is_some();
        let state = Box::into_raw(Box::new(State { callback: factory(rate as u32) }));
        // One byte per stream opened, kept for good: the HAL documents no
        // point after which a removed listener can no longer be running.
        let dead: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        match start_unit(&mut unit, state, chosen, dead) {
            Ok(()) => {
                let period = device.and_then(|d| u32_property(d, PROP_BUFFER_FRAME_SIZE)).unwrap_or(0);
                let name = match &config.device {
                    Some(uid) => devices().ok().and_then(|ds| ds.into_iter().find(|d| &d.id == uid)).map(|d| d.name),
                    None => device.and_then(|d| string_property(d, PROP_NAME)),
                };
                report(Ok(StreamInfo {
                    backend: "CoreAudio",
                    device: name.unwrap_or_default(),
                    sample_rate: rate as u32,
                    device_channels: hardware.channels_per_frame.max(1) as u16,
                    buffer_frames: period,
                    period_frames: period,
                    low_latency,
                    format: "F32".into(),
                }));
                while !stop.load(Ordering::Acquire) {
                    if dead.load(Ordering::Acquire) {
                        health.fail("audio device lost (it was disconnected)".into());
                        break;
                    }
                    std::thread::park_timeout(Duration::from_millis(50));
                }
            }
            Err(e) => report(Err(e)),
        }
        if unit.close() {
            // SAFETY: the unit is stopped and disposed, so no callback can run.
            drop(unsafe { Box::from_raw(state) });
        }
        // Otherwise the state is leaked on purpose: a callback that may
        // still run must not find it freed.
    })
}

struct InputState<C> {
    callback: C,
    stopping: AtomicBool,
    health: Arc<StreamHealth>,
}

unsafe extern "C" fn captured<C: CaptureCallback>(
    user: *mut c_void,
    queue: AudioQueueRef,
    buffer: *mut AudioQueueBuffer,
    _start: *const c_void,
    _packets: u32,
    _descs: *const c_void,
) {
    // SAFETY: `user` is the `InputState<C>` boxed in `open_input`, alive
    // until the queue is disposed; AudioQueue serializes callbacks.
    let state = unsafe { &mut *(user as *mut InputState<C>) };
    // SAFETY: AudioQueue hands us a filled buffer it owns until re-enqueued.
    let buf = unsafe { &mut *buffer };
    let samples = (buf.audio_data_byte_size / 4) as usize & !1;
    // SAFETY: the buffer holds `audio_data_byte_size` bytes of stereo f32.
    let data = unsafe { std::slice::from_raw_parts(buf.audio_data as *const f32, samples) };
    state.callback.capture(data);
    if !state.stopping.load(Ordering::Acquire) {
        // SAFETY: re-enqueueing the buffer we were handed.
        let status = unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, ptr::null()) };
        if status != 0 && !state.stopping.swap(true, Ordering::AcqRel) {
            // Error path only: the queue is dead, so taking a lock is acceptable.
            state.health.fail(format!("input device lost (AudioQueueEnqueueBuffer OSStatus {status})"));
        }
    }
}

/// Captures stereo float at the device's own rate (the queue converts only
/// its sample format and channels; 48 kHz if the rate cannot be read) from
/// the device with UID `device`, else the default input. macOS asks
/// the user for microphone access the first time; until it is granted the
/// queue delivers silence.
pub fn open_input<F, C>(
    device: Option<String>,
    factory: F,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
) -> Result<(InputInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32, u16) -> C + Send + 'static,
    C: CaptureCallback,
{
    spawn_audio_thread(move |report| {
        let id = match &device {
            Some(uid) => device_for_uid(uid),
            None => default_for(PROP_DEFAULT_INPUT),
        };
        let rate = id.and_then(nominal_rate).unwrap_or(SAMPLE_RATE);
        let state = Box::into_raw(Box::new(InputState {
            callback: factory(rate as u32, 2),
            stopping: AtomicBool::new(false),
            health,
        }));
        let format = float_stereo(rate);
        let mut queue: AudioQueueRef = ptr::null_mut();
        // SAFETY: valid format, callback and out-pointer; `state` outlives the queue.
        let status = unsafe {
            AudioQueueNewInput(&format, captured::<C>, state.cast(), ptr::null_mut(), ptr::null(), 0, &mut queue)
        };
        if status != 0 {
            // SAFETY: the queue was not created, so nothing else owns `state`.
            drop(unsafe { Box::from_raw(state) });
            return report(Err(format!("AudioQueueNewInput failed (OSStatus {status})")));
        }
        let cleanup = |queue: AudioQueueRef| {
            // SAFETY: stopping synchronously guarantees no callback runs
            // after AudioQueueStop returns, so `state` can be freed.
            unsafe {
                (*state).stopping.store(true, Ordering::Release);
                AudioQueueStop(queue, 1);
                AudioQueueDispose(queue, 1);
                drop(Box::from_raw(state));
            }
        };
        if let Some(uid) = &device {
            if let Err(e) = set_queue_device(queue, uid) {
                cleanup(queue);
                return report(Err(e));
            }
        }
        let name = match &device {
            Some(uid) => input_devices().ok().and_then(|ds| ds.into_iter().find(|d| &d.id == uid)).map(|d| d.name),
            None => default_for(PROP_DEFAULT_INPUT).and_then(|d| string_property(d, PROP_NAME)),
        };
        for _ in 0..BUFFER_COUNT {
            let mut buf: *mut AudioQueueBuffer = ptr::null_mut();
            // SAFETY: valid queue and out-pointer.
            let s = unsafe { AudioQueueAllocateBuffer(queue, CAPTURE_BUFFER_FRAMES * 8, &mut buf) };
            // SAFETY: handing a freshly allocated, empty buffer to the queue.
            let s = if s == 0 { unsafe { AudioQueueEnqueueBuffer(queue, buf, 0, ptr::null()) } } else { s };
            if s != 0 {
                cleanup(queue);
                return report(Err(format!("preparing the input buffers failed (OSStatus {s})")));
            }
        }
        // SAFETY: valid queue.
        let s = unsafe { AudioQueueStart(queue, ptr::null()) };
        if s != 0 {
            cleanup(queue);
            return report(Err(format!("AudioQueueStart failed (OSStatus {s})")));
        }
        report(Ok(InputInfo {
            backend: "CoreAudio",
            device: name.unwrap_or_default(),
            sample_rate: rate as u32,
            device_channels: 2,
            channels: 2,
            format: "F32".into(),
        }));
        while !stop.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(50));
        }
        cleanup(queue);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{open_input, open_with};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    /// Counts the frames it renders and plays a ramp: exactly representable
    /// values in ±0.5, repeating every 4096 frames, right the negative of left.
    struct Ramp(Arc<AtomicU64>);

    fn ramp(n: u64) -> f32 {
        ((n % 4096) as f32 - 2048.0) / 4096.0
    }

    impl AudioCallback for Ramp {
        fn process(&mut self, out: &mut [f32]) {
            let n = self.0.fetch_add(out.len() as u64 / 2, Ordering::Relaxed);
            for (i, frame) in out.chunks_exact_mut(2).enumerate() {
                frame[0] = ramp(n + i as u64);
                frame[1] = -ramp(n + i as u64);
            }
        }
    }

    fn require(var: &str, why: &str) {
        if std::env::var_os(var).is_some() {
            panic!("{var} is set but {why}");
        }
        eprintln!("{why}; skipping");
    }

    /// Opens the default output and every listed device, with and without
    /// low latency, and checks the unit really calls back at the device rate.
    #[test]
    fn output_units_render_on_every_device() {
        let devices = devices().unwrap_or_default();
        if devices.is_empty() {
            return require("TINVO_REQUIRE_AUDIO", "there is no output device");
        }
        let configs = std::iter::once(None).chain(devices.iter().map(|d| Some(d.id.clone())));
        for device in configs {
            let mut normal = None;
            for low_latency in [false, true, false] {
                let frames = Arc::new(AtomicU64::new(0));
                let counter = frames.clone();
                let config = StreamConfig { device: device.clone(), low_latency };
                let stream = open_with(&config, move |_| Ramp(counter)).unwrap_or_else(|e| panic!("{config:?}: {e}"));
                let info = stream.info().clone();
                assert!(info.sample_rate >= 8_000 && info.period_frames > 0, "{info:?}");
                if let Some(uid) = &device {
                    assert_eq!(Some(&info.device), devices.iter().find(|d| &d.id == uid).map(|d| &d.name));
                }
                let begin = Instant::now();
                let want = u64::from(info.sample_rate) / 4;
                while frames.load(Ordering::Relaxed) < want && begin.elapsed() < Duration::from_secs(3) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(frames.load(Ordering::Relaxed) >= want, "{info:?} rendered too little");
                assert_eq!(stream.error(), None);
                eprintln!("{info:?}");
                match (low_latency, normal) {
                    (false, None) => normal = Some(info.period_frames),
                    (false, Some(p)) => assert_eq!(info.period_frames, p, "low latency left behind: {info:?}"),
                    (true, Some(p)) => assert_eq!(info.low_latency, info.period_frames < p, "{info:?}"),
                    (true, None) => unreachable!(),
                }
            }
        }
    }

    /// Plays the ramp into BlackHole's output and records it from BlackHole's
    /// input; at 48 kHz (no resampling either way) every sample must come
    /// back unchanged and in order.
    #[test]
    fn samples_loop_back_through_a_loopback_device() {
        let find = |list: Vec<Device>| list.into_iter().find(|d| d.name.contains("BlackHole"));
        let (Some(out), Some(inp)) = (find(devices().unwrap_or_default()), find(input_devices().unwrap_or_default()))
        else {
            return require("TINVO_REQUIRE_LOOPBACK", "no BlackHole loopback device");
        };
        let got = Arc::new(Mutex::new(Vec::<f32>::with_capacity(1 << 20)));
        let sink = got.clone();
        let input = open_input(Some(&inp.id), move |_, _| {
            move |samples: &[f32]| {
                let mut v = sink.lock().unwrap();
                if v.len() + samples.len() <= v.capacity() {
                    v.extend_from_slice(samples);
                }
            }
        })
        .unwrap();
        let config = StreamConfig { device: Some(out.id.clone()), low_latency: false };
        let output = open_with(&config, |_| Ramp(Arc::new(AtomicU64::new(0)))).unwrap();
        // Both ends run at the device's own rate, so nothing is resampled.
        assert_eq!(input.info().sample_rate, output.info().sample_rate);
        std::thread::sleep(Duration::from_millis(1500));
        drop(input);
        drop(output);
        let got = got.lock().unwrap();
        let first = got.chunks_exact(2).position(|f| f != [0.0, 0.0]).expect("nothing came back through BlackHole");
        let frames = &got[first * 2..];
        assert!(frames.len() >= 2 * 4800, "only {} frames came back", frames.len() / 2);
        let start = ((frames[0] * 4096.0) + 2048.0) as u64;
        // The tail may be the silence after the output stopped.
        let played = frames.chunks_exact(2).rposition(|f| f != [0.0, 0.0]).unwrap() + 1;
        for (i, f) in frames.chunks_exact(2).take(played).enumerate() {
            let want = ramp(start + i as u64);
            assert_eq!(f, [want, -want], "frame {i} after the first");
        }
    }
}
