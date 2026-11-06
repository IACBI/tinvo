//! Linux output through the ALSA PCM API (libasound).
//!
//! Opens the `default` PCM (which routes through PipeWire/PulseAudio when
//! present) as interleaved stereo float at 48 kHz, letting the ALSA plug
//! layer convert if the hardware differs.

use super::{AudioCallback, StreamHealth, StreamInfo, spawn_audio_thread};
use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_FORMAT_FLOAT_LE: c_int = 14;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;
const SAMPLE_RATE: c_uint = 48_000;
const LATENCY_US: c_uint = 20_000;

#[link(name = "asound")]
unsafe extern "C" {
    fn snd_pcm_open(pcm: *mut *mut c_void, name: *const c_char, stream: c_int, mode: c_int) -> c_int;
    fn snd_pcm_set_params(
        pcm: *mut c_void,
        format: c_int,
        access: c_int,
        channels: c_uint,
        rate: c_uint,
        soft_resample: c_int,
        latency_us: c_uint,
    ) -> c_int;
    fn snd_pcm_get_params(pcm: *mut c_void, buffer_size: *mut c_ulong, period_size: *mut c_ulong) -> c_int;
    fn snd_pcm_writei(pcm: *mut c_void, buffer: *const c_void, frames: c_ulong) -> c_long;
    fn snd_pcm_recover(pcm: *mut c_void, err: c_int, silent: c_int) -> c_int;
    fn snd_pcm_drop(pcm: *mut c_void) -> c_int;
    fn snd_pcm_close(pcm: *mut c_void) -> c_int;
    fn snd_strerror(err: c_int) -> *const c_char;
}

fn alsa_error(what: &str, err: c_int) -> String {
    // SAFETY: snd_strerror returns a static NUL-terminated string.
    let msg = unsafe { CStr::from_ptr(snd_strerror(err)) }.to_string_lossy();
    format!("{what}: {msg}")
}

struct Pcm(*mut c_void);

impl Drop for Pcm {
    fn drop(&mut self) {
        // SAFETY: handle from snd_pcm_open, closed exactly once.
        unsafe {
            snd_pcm_drop(self.0);
            snd_pcm_close(self.0);
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
        // SAFETY: libasound calls with valid handles and buffers.
        let pcm = match unsafe { open_pcm() } {
            Ok(v) => v,
            Err(e) => return report(Err(e)),
        };
        let (pcm, period) = pcm;
        let mut callback = factory(SAMPLE_RATE);
        let mut buf = vec![0.0f32; period * 2];
        report(Ok(StreamInfo {
            backend: "ALSA",
            sample_rate: SAMPLE_RATE,
            device_channels: 2,
            buffer_frames: period as u32,
            format: "F32".into(),
        }));
        while !stop.load(Ordering::Acquire) {
            callback.process(&mut buf);
            for s in buf.iter_mut() {
                *s = super::sanitize(*s);
            }
            let mut offset = 0usize;
            while offset < period {
                // SAFETY: `buf` holds `period` interleaved stereo frames.
                let n = unsafe {
                    snd_pcm_writei(pcm.0, buf[offset * 2..].as_ptr().cast(), (period - offset) as c_ulong)
                };
                if n >= 0 {
                    offset += n as usize;
                    continue;
                }
                // SAFETY: recovering a valid handle from an xrun or suspend.
                let r = unsafe { snd_pcm_recover(pcm.0, n as c_int, 1) };
                if r < 0 {
                    health.fail(alsa_error("audio device lost", r));
                    return;
                }
            }
        }
    })
}

unsafe fn open_pcm() -> Result<(Pcm, usize), String> {
    let mut handle = ptr::null_mut();
    let name = c"default";
    // SAFETY: valid out-pointer and NUL-terminated device name.
    let r = unsafe { snd_pcm_open(&mut handle, name.as_ptr(), SND_PCM_STREAM_PLAYBACK, 0) };
    if r < 0 {
        return Err(alsa_error("cannot open ALSA device 'default'", r));
    }
    let pcm = Pcm(handle);
    // SAFETY: `pcm` is an open handle.
    let r = unsafe {
        snd_pcm_set_params(pcm.0, SND_PCM_FORMAT_FLOAT_LE, SND_PCM_ACCESS_RW_INTERLEAVED, 2, SAMPLE_RATE, 1, LATENCY_US)
    };
    if r < 0 {
        return Err(alsa_error("cannot configure ALSA device", r));
    }
    let (mut buffer, mut period) = (0 as c_ulong, 0 as c_ulong);
    // SAFETY: valid out-pointers.
    let r = unsafe { snd_pcm_get_params(pcm.0, &mut buffer, &mut period) };
    if r < 0 || period == 0 {
        return Err(alsa_error("cannot query ALSA buffer size", r));
    }
    Ok((pcm, period as usize))
}
