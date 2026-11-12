//! Native real-time audio output.
//!
//! | Platform | API                         | Linked library        |
//! |----------|-----------------------------|-----------------------|
//! | Windows  | WASAPI, shared/event mode   | ole32, avrt (system)  |
//! | Linux    | ALSA PCM; JACK (or PipeWire's JACK) | libasound; libjack loaded at run time |
//! | macOS    | AudioToolbox AudioQueue     | AudioToolbox.framework|
//!
//! Each backend runs a dedicated audio thread, asks a factory for the
//! callback once the device sample rate is known, and converts the callback's
//! interleaved stereo `f32` to whatever the device needs. Backends also list
//! the output devices and open one by its id (otherwise the system default).

#[cfg(target_os = "linux")]
pub(crate) mod alsa;
#[cfg(target_os = "macos")]
pub(crate) mod coreaudio;
#[cfg(target_os = "linux")]
mod jack;
#[cfg(windows)]
mod wasapi;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Produces audio on the real-time thread. Implementations must not block,
/// lock or allocate in `process`.
pub trait AudioCallback: Send + 'static {
    /// Fills `out` with interleaved stereo samples.
    fn process(&mut self, out: &mut [f32]);
}

/// Facts about an opened output stream.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    pub backend: &'static str,
    /// Name of the device playing, for display.
    pub device: String,
    pub sample_rate: u32,
    pub device_channels: u16,
    /// Device buffer size in frames.
    pub buffer_frames: u32,
    /// Frames rendered per callback (the device period).
    pub period_frames: u32,
    /// Whether the low-latency period asked for is in use.
    pub low_latency: bool,
    /// Native sample format, for display.
    pub format: String,
}

/// An output device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// Stable identifier to open it by.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether it is the system's default output right now.
    pub is_default: bool,
}

/// How to open a stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamConfig {
    /// Device id; `None` plays on the system default.
    pub device: Option<String>,
    /// Ask for a shorter period: the smallest WASAPI `IAudioClient3` offers,
    /// or 64 frames on CoreAudio. ALSA and JACK ignore it (JACK's period is
    /// the server's).
    pub low_latency: bool,
}

/// Lists the output devices that can be opened now.
pub fn output_devices() -> Result<Vec<Device>, String> {
    platform_devices()
}

/// Lists the input (capture) devices that can be opened now.
pub fn input_devices() -> Result<Vec<Device>, String> {
    platform_input_devices()
}

/// Facts about an opened input stream.
#[derive(Clone, Debug, PartialEq)]
pub struct InputInfo {
    pub backend: &'static str,
    /// Name of the device capturing, for display.
    pub device: String,
    pub sample_rate: u32,
    /// Channels the device delivers.
    pub device_channels: u16,
    /// Channels passed to the callback: the first one or two.
    pub channels: u16,
    /// Native sample format, for display.
    pub format: String,
}

/// Receives captured audio on the capture thread: interleaved `f32` with
/// [`InputInfo::channels`] per frame. It must not block or allocate, or the
/// device overruns.
pub trait CaptureCallback: Send + 'static {
    fn capture(&mut self, samples: &[f32]);
}

impl<F: FnMut(&[f32]) + Send + 'static> CaptureCallback for F {
    fn capture(&mut self, samples: &[f32]) {
        self(samples);
    }
}

/// A running input stream. Dropping it stops the capture thread.
pub struct InputStream {
    info: InputInfo,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
    thread: Option<JoinHandle<()>>,
}

impl InputStream {
    pub fn info(&self) -> &InputInfo {
        &self.info
    }

    /// The error that stopped capture, if any (e.g. device unplugged).
    pub fn error(&self) -> Option<String> {
        self.health.error()
    }
}

impl Drop for InputStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Opens the input device with id `device` (`None`: the system default).
/// `factory` runs on the capture thread with the sample rate and channel
/// count, before capture starts, and returns the callback.
pub fn open_input<F, C>(device: Option<&str>, factory: F) -> Result<InputStream, String>
where
    F: FnOnce(u32, u16) -> C + Send + 'static,
    C: CaptureCallback,
{
    let stop = Arc::new(AtomicBool::new(false));
    let health = Arc::new(StreamHealth::default());
    let (info, thread) = platform_open_input(device.map(str::to_string), factory, stop.clone(), health.clone())?;
    Ok(InputStream { info, stop, health, thread: Some(thread) })
}

/// Id of the system's default output, where the backend can tell (WASAPI);
/// a change means a stream on the default device should be reopened. The
/// other backends follow default changes themselves and return `None`.
pub fn default_output_id() -> Option<String> {
    platform_default_id()
}

/// Picks the device `query` names: an exact id, else the one device whose
/// name contains `query` (ignoring case).
pub fn find_device<'a>(devices: &'a [Device], query: &str) -> Result<&'a Device, String> {
    if let Some(d) = devices.iter().find(|d| d.id == query) {
        return Ok(d);
    }
    let q = query.to_lowercase();
    let hits: Vec<&Device> = devices.iter().filter(|d| d.name.to_lowercase().contains(&q)).collect();
    match hits[..] {
        [d] => Ok(d),
        [] => Err(format!("no device matches '{query}' (tinvo devices and tinvo inputs list them)")),
        _ => Err(format!(
            "'{query}' matches {} devices: {}",
            hits.len(),
            hits.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Failure reporting from the audio thread.
#[derive(Debug, Default)]
pub struct StreamHealth {
    failed: AtomicBool,
    message: Mutex<String>,
}

impl StreamHealth {
    /// Records a fatal stream error. Called once, as the audio thread exits.
    pub(crate) fn fail(&self, message: String) {
        if let Ok(mut m) = self.message.lock() {
            *m = message;
        }
        self.failed.store(true, Ordering::Release);
    }

    pub fn error(&self) -> Option<String> {
        if self.failed.load(Ordering::Acquire) {
            Some(self.message.lock().map(|m| m.clone()).unwrap_or_default())
        } else {
            None
        }
    }
}

/// A running output stream. Dropping it stops the audio thread.
pub struct Stream {
    info: StreamInfo,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
    thread: Option<JoinHandle<()>>,
}

impl Stream {
    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// The error that stopped the stream, if any (e.g. device unplugged).
    pub fn error(&self) -> Option<String> {
        self.health.error()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Opens the default output device. `factory` runs on the audio thread with
/// the device sample rate and returns the callback to drive.
pub fn open<F, C>(factory: F) -> Result<Stream, String>
where
    F: FnOnce(u32) -> C + Send + 'static,
    C: AudioCallback,
{
    open_with(&StreamConfig::default(), factory)
}

/// Opens the output `config` asks for.
pub fn open_with<F, C>(config: &StreamConfig, factory: F) -> Result<Stream, String>
where
    F: FnOnce(u32) -> C + Send + 'static,
    C: AudioCallback,
{
    let stop = Arc::new(AtomicBool::new(false));
    let health = Arc::new(StreamHealth::default());
    let (info, thread) = platform_open(config.clone(), factory, stop.clone(), health.clone())?;
    Ok(Stream { info, stop, health, thread: Some(thread) })
}

#[cfg(windows)]
use wasapi::{
    default_id as platform_default_id, devices as platform_devices, input_devices as platform_input_devices,
    open as platform_open, open_input as platform_open_input,
};

/// Linux lists ALSA's PCMs and, while a server runs, JACK, under the id
/// `jack`. ALSA's own `jack` plugin PCM is left out: it needs the same
/// libjack, and the native path replaces it.
#[cfg(target_os = "linux")]
fn platform_devices() -> Result<Vec<Device>, String> {
    with_jack(alsa::devices(), jack::devices())
}

#[cfg(target_os = "linux")]
fn platform_input_devices() -> Result<Vec<Device>, String> {
    with_jack(alsa::input_devices(), jack::input_devices())
}

#[cfg(target_os = "linux")]
fn with_jack(alsa: Result<Vec<Device>, String>, jack: Vec<Device>) -> Result<Vec<Device>, String> {
    match alsa {
        Ok(mut devices) => {
            devices.retain(|d| d.id != jack::DEVICE_ID);
            devices.extend(jack);
            Ok(devices)
        }
        Err(e) if jack.is_empty() => Err(e),
        Err(_) => Ok(jack),
    }
}

#[cfg(target_os = "linux")]
fn platform_open<F, C>(
    config: StreamConfig,
    factory: F,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
) -> Result<(StreamInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32) -> C + Send + 'static,
    C: AudioCallback,
{
    if config.device.as_deref() == Some(jack::DEVICE_ID) {
        jack::open(config, factory, stop, health)
    } else {
        alsa::open(config, factory, stop, health)
    }
}

#[cfg(target_os = "linux")]
fn platform_open_input<F, C>(
    device: Option<String>,
    factory: F,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
) -> Result<(InputInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32, u16) -> C + Send + 'static,
    C: CaptureCallback,
{
    if device.as_deref() == Some(jack::DEVICE_ID) {
        jack::open_input(factory, stop, health)
    } else {
        alsa::open_input(device, factory, stop, health)
    }
}

#[cfg(target_os = "macos")]
use coreaudio::{
    devices as platform_devices, input_devices as platform_input_devices, open as platform_open,
    open_input as platform_open_input,
};

#[cfg(not(windows))]
fn platform_default_id() -> Option<String> {
    None
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_devices() -> Result<Vec<Device>, String> {
    Err("no audio backend for this platform".into())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_input_devices() -> Result<Vec<Device>, String> {
    Err("no audio backend for this platform".into())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_open_input<F, C>(
    _device: Option<String>,
    _factory: F,
    _stop: Arc<AtomicBool>,
    _health: Arc<StreamHealth>,
) -> Result<(InputInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32, u16) -> C + Send + 'static,
    C: CaptureCallback,
{
    Err("no audio backend for this platform".into())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_open<F, C>(
    _config: StreamConfig,
    _factory: F,
    _stop: Arc<AtomicBool>,
    _health: Arc<StreamHealth>,
) -> Result<(StreamInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32) -> C + Send + 'static,
    C: AudioCallback,
{
    Err("no audio backend for this platform".into())
}

/// Spawns the audio thread and waits until it reports that the device is
/// open (or failed to open).
#[allow(dead_code)] // unused only on platforms without a backend
fn spawn_audio_thread<T, B>(body: B) -> Result<(T, JoinHandle<()>), String>
where
    T: Send + 'static,
    B: FnOnce(&dyn Fn(Result<T, String>)) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("tinvo-audio".into())
        .spawn(move || {
            let report = |r: Result<T, String>| {
                let _ = tx.send(r);
            };
            body(&report);
        })
        .map_err(|e| format!("cannot spawn audio thread: {e}"))?;
    match rx.recv() {
        Ok(Ok(info)) => Ok((info, thread)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err("audio thread exited during start-up".into())
        }
    }
}

/// Guards the device against non-finite or wildly out-of-range samples.
#[inline]
pub fn sanitize(x: f32) -> f32 {
    if x.is_finite() { x.clamp(-1.0, 1.0) } else { 0.0 }
}

/// Native device sample encodings the backends convert to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceFormat {
    F32,
    I16,
    /// Packed 3-byte little-endian.
    I24,
    I32,
}

impl DeviceFormat {
    pub fn bytes(self) -> usize {
        match self {
            DeviceFormat::F32 | DeviceFormat::I32 => 4,
            DeviceFormat::I16 => 2,
            DeviceFormat::I24 => 3,
        }
    }
}

/// Reads `frames` frames of `channels` interleaved channels in `format`
/// from `src`, writing the first `keep` channels of each as `f32` to `dst`.
pub fn convert_from_device(src: &[u8], channels: usize, format: DeviceFormat, keep: usize, dst: &mut [f32]) {
    let width = format.bytes();
    let frames = dst.len() / keep;
    debug_assert!(keep <= channels && src.len() >= frames * channels * width);
    for f in 0..frames {
        for c in 0..keep {
            let at = (f * channels + c) * width;
            let b = &src[at..at + width];
            dst[f * keep + c] = match format {
                DeviceFormat::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                DeviceFormat::I16 => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
                // Sign-extend the 3 bytes through the top of an i32.
                DeviceFormat::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
                DeviceFormat::I32 => (f64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])) / 2_147_483_648.0) as f32,
            };
        }
    }
}

/// Writes interleaved stereo `src` into `dst` laid out as `channels`
/// interleaved channels in `format`. Mono devices get the average of L and R;
/// channels beyond the first two are silent.
pub fn convert_stereo(src: &[f32], dst: &mut [u8], channels: usize, format: DeviceFormat) {
    let frames = src.len() / 2;
    let width = format.bytes();
    debug_assert!(dst.len() >= frames * channels * width);
    for f in 0..frames {
        let (l, r) = (sanitize(src[2 * f]), sanitize(src[2 * f + 1]));
        for c in 0..channels {
            let v = match (channels, c) {
                (1, _) => 0.5 * (l + r),
                (_, 0) => l,
                (_, 1) => r,
                _ => 0.0,
            };
            let at = (f * channels + c) * width;
            let out = &mut dst[at..at + width];
            match format {
                DeviceFormat::F32 => out.copy_from_slice(&v.to_le_bytes()),
                DeviceFormat::I16 => {
                    out.copy_from_slice(&((v * 32_767.0).round() as i16).to_le_bytes());
                }
                DeviceFormat::I24 => {
                    out.copy_from_slice(&((v * 8_388_607.0).round() as i32).to_le_bytes()[..3]);
                }
                DeviceFormat::I32 => {
                    let q = (f64::from(v) * 2_147_483_647.0).round() as i32;
                    out.copy_from_slice(&q.to_le_bytes());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the platform's device listing (with or without devices);
    /// a bad FFI declaration would crash here rather than in the app.
    #[test]
    fn listing_devices_is_consistent() {
        match output_devices() {
            Ok(devices) => {
                assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
                for d in &devices {
                    assert!(!d.id.is_empty() && !d.name.is_empty(), "{d:?}");
                }
                if let Some(id) = default_output_id() {
                    assert!(devices.iter().any(|d| d.id == id && d.is_default), "default {id} not listed");
                }
            }
            Err(e) => assert!(!e.is_empty()),
        }
    }

    #[test]
    fn devices_are_found_by_id_or_unique_name() {
        let d = |id: &str, name: &str| Device { id: id.into(), name: name.into(), is_default: false };
        let list = [d("{a}", "Speakers (Realtek Audio)"), d("{b}", "Headphones (USB)"), d("{c}", "Speakers (HDMI)")];
        assert_eq!(find_device(&list, "{b}").unwrap().id, "{b}");
        assert_eq!(find_device(&list, "usb").unwrap().id, "{b}");
        assert_eq!(find_device(&list, "realtek").unwrap().id, "{a}");
        let e = find_device(&list, "speakers").unwrap_err();
        assert!(e.contains("matches 2 devices"), "{e}");
        assert!(find_device(&list, "bluetooth").unwrap_err().contains("no device matches"));
    }

    #[test]
    fn device_input_converts_to_f32_and_keeps_the_first_channels() {
        // Two frames of three channels in each format; keep two channels.
        let values = [0.5f32, -0.25, 0.75, -1.0, 0.125, 0.0];
        for format in [DeviceFormat::F32, DeviceFormat::I16, DeviceFormat::I24, DeviceFormat::I32] {
            let mut bytes = vec![0u8; values.len() * format.bytes()];
            for (i, &v) in values.iter().enumerate() {
                let w = format.bytes();
                let out = &mut bytes[i * w..(i + 1) * w];
                match format {
                    DeviceFormat::F32 => out.copy_from_slice(&v.to_le_bytes()),
                    DeviceFormat::I16 => out.copy_from_slice(&((v * 32_768.0) as i16).to_le_bytes()),
                    DeviceFormat::I24 => out.copy_from_slice(&((v * 8_388_608.0) as i32).to_le_bytes()[..3]),
                    DeviceFormat::I32 => out.copy_from_slice(
                        &((f64::from(v) * 2_147_483_648.0).max(-2_147_483_648.0) as i32).to_le_bytes(),
                    ),
                }
            }
            let mut dst = [9.0f32; 4];
            convert_from_device(&bytes, 3, format, 2, &mut dst);
            assert_eq!(dst, [0.5, -0.25, -1.0, 0.125], "{format:?}");
        }
        // Full-scale negative 24-bit sign-extends; mono keeps one channel.
        let mut dst = [0.0f32; 2];
        convert_from_device(&[0x00, 0x00, 0x80, 0xFF, 0xFF, 0x7F], 1, DeviceFormat::I24, 1, &mut dst);
        assert_eq!(dst[0], -1.0);
        assert!((dst[1] - 8_388_607.0 / 8_388_608.0).abs() < 1e-9);
    }

    /// Exercises the platform's input listing (with or without devices).
    #[test]
    fn listing_input_devices_is_consistent() {
        match input_devices() {
            Ok(devices) => {
                assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
                for d in &devices {
                    assert!(!d.id.is_empty() && !d.name.is_empty(), "{d:?}");
                }
            }
            Err(e) => assert!(!e.is_empty()),
        }
    }

    #[test]
    fn sanitize_guards_output() {
        assert_eq!(sanitize(f32::NAN), 0.0);
        assert_eq!(sanitize(f32::INFINITY), 0.0);
        assert_eq!(sanitize(3.0), 1.0);
        assert_eq!(sanitize(-0.25), -0.25);
    }

    #[test]
    fn converts_channel_layouts_and_formats() {
        let src = [0.5f32, -0.5, 1.0, 0.0];
        let mut mono = [0u8; 2 * 4];
        convert_stereo(&src, &mut mono, 1, DeviceFormat::F32);
        assert_eq!(f32::from_le_bytes(mono[0..4].try_into().unwrap()), 0.0);
        assert_eq!(f32::from_le_bytes(mono[4..8].try_into().unwrap()), 0.5);

        let mut quad = [0u8; 2 * 4 * 2];
        convert_stereo(&src, &mut quad, 4, DeviceFormat::I16);
        let s = |i: usize| i16::from_le_bytes([quad[2 * i], quad[2 * i + 1]]);
        assert_eq!([s(0), s(1), s(2), s(3)], [16_384, -16_384, 0, 0]);
        assert_eq!(s(4), 32_767);

        let mut p24 = [0u8; 2 * 3 * 2];
        convert_stereo(&src, &mut p24, 2, DeviceFormat::I24);
        let v = i32::from_le_bytes([0, p24[3], p24[4], p24[5]]) >> 8;
        assert_eq!(v, -4_194_304);

        let mut i32s = [0u8; 2 * 4 * 2];
        convert_stereo(&[f32::NAN, -1.0, 0.0, 0.0], &mut i32s, 2, DeviceFormat::I32);
        assert_eq!(i32::from_le_bytes(i32s[0..4].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(i32s[4..8].try_into().unwrap()), -2_147_483_647);
    }
}
