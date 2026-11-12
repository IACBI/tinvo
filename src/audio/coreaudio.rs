//! macOS output through AudioToolbox's AudioQueue.
//!
//! The queue calls back on its own internal thread; TINVO's audio thread only
//! owns the queue's lifetime and waits for the stop request. Devices are the
//! HAL's devices that have output streams, identified by their UID; the queue
//! plays on the system default unless one is set.

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
const BUFFER_FRAMES: u32 = 512;
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
type OutputCallback = unsafe extern "C" fn(*mut c_void, AudioQueueRef, *mut AudioQueueBuffer);
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
    fn AudioQueueNewOutput(
        format: *const AudioStreamBasicDescription,
        callback: OutputCallback,
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

struct State<C> {
    callback: C,
    stopping: AtomicBool,
    health: Arc<StreamHealth>,
}

unsafe extern "C" fn render<C: AudioCallback>(user: *mut c_void, queue: AudioQueueRef, buffer: *mut AudioQueueBuffer) {
    // SAFETY: `user` is the `State<C>` boxed in `open`, alive until the
    // queue is disposed; AudioQueue serializes callbacks.
    let state = unsafe { &mut *(user as *mut State<C>) };
    // SAFETY: AudioQueue hands us a valid buffer it owns until re-enqueued.
    let buf = unsafe { &mut *buffer };
    let frames = (buf.audio_data_bytes_capacity / 8) as usize;
    // SAFETY: capacity was allocated for `frames` stereo f32 frames.
    let out = unsafe { std::slice::from_raw_parts_mut(buf.audio_data as *mut f32, frames * 2) };
    state.callback.process(out);
    for s in out.iter_mut() {
        *s = super::sanitize(*s);
    }
    buf.audio_data_byte_size = (frames * 8) as u32;
    if !state.stopping.load(Ordering::Acquire) {
        // SAFETY: re-enqueueing the buffer we were handed.
        let status = unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, ptr::null()) };
        if status != 0 && !state.stopping.swap(true, Ordering::AcqRel) {
            // Error path only: the queue is dead, so taking a lock is acceptable.
            state.health.fail(format!("audio device lost (AudioQueueEnqueueBuffer OSStatus {status})"));
        }
    }
}

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
        let state = Box::into_raw(Box::new(State {
            callback: factory(SAMPLE_RATE as u32),
            stopping: AtomicBool::new(false),
            health,
        }));
        let format = AudioStreamBasicDescription {
            sample_rate: SAMPLE_RATE,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            bytes_per_packet: 8,
            frames_per_packet: 1,
            bytes_per_frame: 8,
            channels_per_frame: 2,
            bits_per_channel: 32,
            reserved: 0,
        };
        let mut queue: AudioQueueRef = ptr::null_mut();
        // SAFETY: valid format, callback and out-pointer; `state` outlives the queue.
        let status = unsafe {
            AudioQueueNewOutput(&format, render::<C>, state.cast(), ptr::null_mut(), ptr::null(), 0, &mut queue)
        };
        if status != 0 {
            // SAFETY: the queue was not created, so nothing else owns `state`.
            drop(unsafe { Box::from_raw(state) });
            return report(Err(format!("AudioQueueNewOutput failed (OSStatus {status})")));
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
        if let Some(uid) = &config.device {
            if let Err(e) = set_queue_device(queue, uid) {
                cleanup(queue);
                return report(Err(e));
            }
        }
        let device = match &config.device {
            Some(uid) => devices().ok().and_then(|ds| ds.into_iter().find(|d| &d.id == uid)).map(|d| d.name),
            None => default_device().and_then(|d| string_property(d, PROP_NAME)),
        };
        for _ in 0..BUFFER_COUNT {
            let mut buf: *mut AudioQueueBuffer = ptr::null_mut();
            // SAFETY: valid queue and out-pointer.
            let s = unsafe { AudioQueueAllocateBuffer(queue, BUFFER_FRAMES * 8, &mut buf) };
            if s != 0 {
                cleanup(queue);
                return report(Err(format!("AudioQueueAllocateBuffer failed (OSStatus {s})")));
            }
            // Prime the queue with real audio before starting.
            // SAFETY: invoking our own callback with a freshly allocated buffer.
            unsafe { render::<C>(state.cast(), queue, buf) };
        }
        // SAFETY: valid queue.
        let s = unsafe { AudioQueueStart(queue, ptr::null()) };
        if s != 0 {
            cleanup(queue);
            return report(Err(format!("AudioQueueStart failed (OSStatus {s})")));
        }
        report(Ok(StreamInfo {
            backend: "CoreAudio",
            device: device.unwrap_or_default(),
            sample_rate: SAMPLE_RATE as u32,
            device_channels: 2,
            buffer_frames: BUFFER_FRAMES * BUFFER_COUNT as u32,
            period_frames: BUFFER_FRAMES,
            low_latency: false,
            format: "F32".into(),
        }));
        while !stop.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(50));
        }
        cleanup(queue);
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

/// Captures stereo float at 48 kHz (the queue converts the device's format)
/// from the device with UID `device`, else the default input. macOS asks
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
        let state = Box::into_raw(Box::new(InputState {
            callback: factory(SAMPLE_RATE as u32, 2),
            stopping: AtomicBool::new(false),
            health,
        }));
        let format = AudioStreamBasicDescription {
            sample_rate: SAMPLE_RATE,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            bytes_per_packet: 8,
            frames_per_packet: 1,
            bytes_per_frame: 8,
            channels_per_frame: 2,
            bits_per_channel: 32,
            reserved: 0,
        };
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
            sample_rate: SAMPLE_RATE as u32,
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
