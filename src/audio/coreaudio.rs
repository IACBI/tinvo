//! macOS output through AudioToolbox's AudioQueue.
//!
//! The queue calls back on its own internal thread; TINVO's audio thread only
//! owns the queue's lifetime and waits for the stop request.

use super::{AudioCallback, StreamHealth, StreamInfo, spawn_audio_thread};
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const SAMPLE_RATE: f64 = 48_000.0;
const BUFFER_FRAMES: u32 = 512;
const BUFFER_COUNT: usize = 3;
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

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
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
    factory: F,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
) -> Result<(StreamInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32) -> C + Send + 'static,
    C: AudioCallback,
{
    spawn_audio_thread(move |report| {
        let state = Box::into_raw(Box::new(State { callback: factory(SAMPLE_RATE as u32), stopping: AtomicBool::new(false), health }));
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
            sample_rate: SAMPLE_RATE as u32,
            device_channels: 2,
            buffer_frames: BUFFER_FRAMES,
            format: "F32".into(),
        }));
        while !stop.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(50));
        }
        cleanup(queue);
    })
}
