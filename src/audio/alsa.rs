//! Linux output through the ALSA PCM API (libasound).
//!
//! Opens the chosen PCM, else `default` (which routes through
//! PipeWire/PulseAudio when present), as interleaved stereo float at 48 kHz,
//! letting the ALSA plug layer convert if the hardware differs. Devices are
//! the PCM names ALSA's hints list for output.

use super::{
    AudioCallback, CaptureCallback, Device, InputInfo, StreamConfig, StreamHealth, StreamInfo, spawn_audio_thread,
};
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_STREAM_CAPTURE: c_int = 1;
/// Capture latency; recording does not need it short.
const CAPTURE_LATENCY_US: c_uint = 100_000;
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
    fn snd_pcm_readi(pcm: *mut c_void, buffer: *mut c_void, frames: c_ulong) -> c_long;
    fn snd_pcm_recover(pcm: *mut c_void, err: c_int, silent: c_int) -> c_int;
    fn snd_pcm_drop(pcm: *mut c_void) -> c_int;
    fn snd_pcm_close(pcm: *mut c_void) -> c_int;
    fn snd_strerror(err: c_int) -> *const c_char;
    fn snd_device_name_hint(card: c_int, iface: *const c_char, hints: *mut *mut *mut c_void) -> c_int;
    fn snd_device_name_get_hint(hint: *const c_void, id: *const c_char) -> *mut c_char;
    fn snd_device_name_free_hint(hints: *mut *mut c_void) -> c_int;
}

unsafe extern "C" {
    fn free(p: *mut c_void);
}

const DEFAULT_PCM: &str = "default";

/// Takes ownership of a string libasound allocated.
unsafe fn take_string(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: a NUL-terminated string from snd_device_name_get_hint, freed here once.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { free(p.cast()) };
    Some(s)
}

/// One entry of ALSA's device hints: its name, description and direction
/// (`None` means both).
pub(crate) struct Hint {
    pub name: String,
    pub desc: Option<String>,
    pub ioid: Option<String>,
}

/// ALSA's device hints for interface `iface` (`pcm`, `rawmidi`) on every card.
pub(crate) fn hints(iface: &CStr) -> Result<Vec<Hint>, String> {
    let mut hints: *mut *mut c_void = ptr::null_mut();
    // SAFETY: valid out-pointer; card -1 asks for all cards.
    let r = unsafe { snd_device_name_hint(-1, iface.as_ptr(), &mut hints) };
    if r < 0 {
        return Err(alsa_error("cannot list ALSA devices", r));
    }
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        // SAFETY: `hints` is a NULL-terminated array from snd_device_name_hint.
        let hint = unsafe { *hints.add(i) };
        if hint.is_null() {
            break;
        }
        i += 1;
        // SAFETY: `hint` is a live entry; each returned string is ours to free.
        let (name, desc, ioid) = unsafe {
            (
                take_string(snd_device_name_get_hint(hint, c"NAME".as_ptr())),
                take_string(snd_device_name_get_hint(hint, c"DESC".as_ptr())),
                take_string(snd_device_name_get_hint(hint, c"IOID".as_ptr())),
            )
        };
        if let Some(name) = name {
            out.push(Hint { name, desc: desc.map(|d| d.replace('\n', ", ")), ioid });
        }
    }
    // SAFETY: freeing the array snd_device_name_hint allocated, once.
    unsafe { snd_device_name_free_hint(hints) };
    Ok(out)
}

/// Lists the PCMs ALSA offers for capture.
pub fn input_devices() -> Result<Vec<Device>, String> {
    Ok(hints(c"pcm")?
        .into_iter()
        .filter(|h| h.ioid.as_deref() != Some("Output") && h.name != "null")
        .map(|h| {
            let label = h.desc.unwrap_or_else(|| h.name.clone());
            Device { is_default: h.name == DEFAULT_PCM, name: format!("{label} ({})", h.name), id: h.name }
        })
        .collect())
}

/// Captures stereo float at 48 kHz from the chosen PCM (the plug layer
/// converts mono inputs and other rates).
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
        let name = device.unwrap_or_else(|| DEFAULT_PCM.into());
        // SAFETY: libasound calls with valid handles and buffers.
        let (pcm, _, period) = match unsafe { open_pcm(&name, SND_PCM_STREAM_CAPTURE, CAPTURE_LATENCY_US) } {
            Ok(v) => v,
            Err(e) => return report(Err(e)),
        };
        let mut callback = factory(SAMPLE_RATE, 2);
        let mut buf = vec![0.0f32; period * 2];
        report(Ok(InputInfo {
            backend: "ALSA",
            device: name,
            sample_rate: SAMPLE_RATE,
            device_channels: 2,
            channels: 2,
            format: "F32".into(),
        }));
        while !stop.load(Ordering::Acquire) {
            // SAFETY: `buf` holds `period` interleaved stereo frames.
            let n = unsafe { snd_pcm_readi(pcm.0, buf.as_mut_ptr().cast(), period as c_ulong) };
            if n > 0 {
                callback.capture(&buf[..n as usize * 2]);
                continue;
            }
            // SAFETY: recovering a valid handle from an overrun or suspend.
            let r = unsafe { snd_pcm_recover(pcm.0, n as c_int, 1) };
            if r < 0 {
                health.fail(alsa_error("input device lost", r));
                return;
            }
        }
    })
}

/// Lists the PCMs ALSA offers for playback.
pub fn devices() -> Result<Vec<Device>, String> {
    Ok(hints(c"pcm")?
        .into_iter()
        .filter(|h| h.ioid.as_deref() != Some("Input") && h.name != "null")
        .map(|h| {
            let label = h.desc.unwrap_or_else(|| h.name.clone());
            Device { is_default: h.name == DEFAULT_PCM, name: format!("{label} ({})", h.name), id: h.name }
        })
        .collect())
}

pub(crate) fn alsa_error(what: &str, err: c_int) -> String {
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
        let name = config.device.unwrap_or_else(|| DEFAULT_PCM.into());
        // SAFETY: libasound calls with valid handles and buffers.
        let (pcm, buffer, period) = match unsafe { open_pcm(&name, SND_PCM_STREAM_PLAYBACK, LATENCY_US) } {
            Ok(v) => v,
            Err(e) => return report(Err(e)),
        };
        let mut callback = factory(SAMPLE_RATE);
        let mut buf = vec![0.0f32; period * 2];
        report(Ok(StreamInfo {
            backend: "ALSA",
            device: name,
            sample_rate: SAMPLE_RATE,
            device_channels: 2,
            buffer_frames: buffer as u32,
            period_frames: period as u32,
            low_latency: false,
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
                let n =
                    unsafe { snd_pcm_writei(pcm.0, buf[offset * 2..].as_ptr().cast(), (period - offset) as c_ulong) };
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

unsafe fn open_pcm(name: &str, stream: c_int, latency_us: c_uint) -> Result<(Pcm, usize, usize), String> {
    let mut handle = ptr::null_mut();
    let c_name = CString::new(name).map_err(|_| format!("bad ALSA device name '{name}'"))?;
    // SAFETY: valid out-pointer and NUL-terminated device name.
    let r = unsafe { snd_pcm_open(&mut handle, c_name.as_ptr(), stream, 0) };
    if r < 0 {
        return Err(alsa_error(&format!("cannot open ALSA device '{name}'"), r));
    }
    let pcm = Pcm(handle);
    // SAFETY: `pcm` is an open handle.
    let r = unsafe {
        snd_pcm_set_params(pcm.0, SND_PCM_FORMAT_FLOAT_LE, SND_PCM_ACCESS_RW_INTERLEAVED, 2, SAMPLE_RATE, 1, latency_us)
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
    Ok((pcm, buffer as usize, period as usize))
}
