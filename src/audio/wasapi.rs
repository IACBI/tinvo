//! Windows output through WASAPI in shared, event-driven mode.
//!
//! COM interfaces are called through hand-written vtable layouts. Only the
//! vtable prefix up to the last method used is declared; the order of
//! methods follows the Windows SDK headers (mmdeviceapi.h, audioclient.h).

use super::{AudioCallback, DeviceFormat, StreamHealth, StreamInfo, convert_stereo, spawn_audio_thread};
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

type Hresult = i32;
type Handle = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

const CLSID_MM_DEVICE_ENUMERATOR: Guid =
    Guid { d1: 0xBCDE_0395, d2: 0xE52F, d3: 0x467C, d4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E] };
const IID_IMM_DEVICE_ENUMERATOR: Guid =
    Guid { d1: 0xA956_64D2, d2: 0x9614, d3: 0x4F35, d4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6] };
const IID_IAUDIO_CLIENT: Guid =
    Guid { d1: 0x1CB9_AD4C, d2: 0xDBFA, d3: 0x4C32, d4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2] };
const IID_IAUDIO_RENDER_CLIENT: Guid =
    Guid { d1: 0xF294_ACFC, d2: 0x3146, d3: 0x4483, d4: [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2] };
const SUBTYPE_PCM: Guid = Guid { d1: 1, d2: 0, d3: 0x10, d4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71] };
const SUBTYPE_IEEE_FLOAT: Guid = Guid { d1: 3, d2: 0, d3: 0x10, d4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71] };

const COINIT_MULTITHREADED: u32 = 0;
const CLSCTX_ALL: u32 = 0x17;
const E_RENDER: i32 = 0;
const E_CONSOLE: i32 = 0;
const AUDCLNT_SHAREMODE_SHARED: i32 = 0;
const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
const WAIT_OBJECT_0: u32 = 0;
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// Requested device buffer: 20 ms in 100-ns units.
const BUFFER_DURATION: i64 = 200_000;

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> Hresult;
    fn CoUninitialize();
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut c_void,
        context: u32,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> Hresult;
    fn CoTaskMemFree(p: *mut c_void);
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(attributes: *mut c_void, manual_reset: i32, initial: i32, name: *const u16) -> Handle;
    fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
}

#[link(name = "avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(task: *const u16, index: *mut u32) -> Handle;
    fn AvRevertMmThreadCharacteristics(handle: Handle) -> i32;
}

#[repr(C)]
struct IUnknownVtbl {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct IMMDeviceEnumeratorVtbl {
    base: IUnknownVtbl,
    _enum_audio_endpoints: usize,
    get_default_audio_endpoint: unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> Hresult,
}

#[repr(C)]
struct IMMDeviceVtbl {
    base: IUnknownVtbl,
    activate: unsafe extern "system" fn(*mut c_void, *const Guid, u32, *mut c_void, *mut *mut c_void) -> Hresult,
}

#[repr(C)]
struct IAudioClientVtbl {
    base: IUnknownVtbl,
    initialize: unsafe extern "system" fn(*mut c_void, i32, u32, i64, i64, *const WaveFormatEx, *const Guid) -> Hresult,
    get_buffer_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult,
    _get_stream_latency: usize,
    get_current_padding: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult,
    _is_format_supported: usize,
    get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut WaveFormatEx) -> Hresult,
    _get_device_period: usize,
    start: unsafe extern "system" fn(*mut c_void) -> Hresult,
    stop: unsafe extern "system" fn(*mut c_void) -> Hresult,
    _reset: usize,
    set_event_handle: unsafe extern "system" fn(*mut c_void, Handle) -> Hresult,
    get_service: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult,
}

#[repr(C)]
struct IAudioRenderClientVtbl {
    base: IUnknownVtbl,
    get_buffer: unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> Hresult,
    release_buffer: unsafe extern "system" fn(*mut c_void, u32, u32) -> Hresult,
}

/// WAVEFORMATEX is declared with 1-byte packing in mmreg.h.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct WaveFormatEx {
    tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    cb_size: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct WaveFormatExtensible {
    format: WaveFormatEx,
    valid_bits: u16,
    channel_mask: u32,
    sub_format: Guid,
}

/// Owned COM interface pointer, released on drop.
struct Com(*mut c_void);

impl Com {
    /// # Safety
    /// `T` must be a prefix of this object's real vtable layout.
    unsafe fn vtbl<T>(&self) -> &T {
        // SAFETY: a COM object pointer points at its vtable pointer.
        unsafe { &**(self.0 as *const *const T) }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: every COM vtable begins with IUnknown.
            unsafe { (self.vtbl::<IUnknownVtbl>().release)(self.0) };
        }
    }
}

fn check(hr: Hresult, what: &str) -> Result<(), String> {
    if hr < 0 { Err(format!("{what} failed (HRESULT 0x{:08X})", hr as u32)) } else { Ok(()) }
}

struct Event(Handle);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

struct ComApartment(bool);

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balanced with a successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

struct MmcssTask(Handle);

impl Drop for MmcssTask {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: handle returned by AvSetMmThreadCharacteristicsW.
            unsafe { AvRevertMmThreadCharacteristics(self.0) };
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
        // SAFETY: all COM calls follow the documented WASAPI contracts; the
        // objects live (and are released) on this thread.
        if let Err(e) = unsafe { run(factory, &stop, report) } {
            health.fail(e);
        }
    })
}

/// Opens the device and runs the render loop until `stop` is set.
///
/// Errors before the stream is running are passed to `report`; errors after
/// that are returned so the caller can record them.
unsafe fn run<F, C>(factory: F, stop: &AtomicBool, report: &dyn Fn(Result<StreamInfo, String>)) -> Result<(), String>
where
    F: FnOnce(u32) -> C,
    C: AudioCallback,
{
    let mut reported = false;
    let result = unsafe { run_inner(factory, stop, report, &mut reported) };
    match result {
        Err(e) if !reported => {
            report(Err(e));
            Ok(())
        }
        other => other,
    }
}

unsafe fn run_inner<F, C>(
    factory: F,
    stop: &AtomicBool,
    report: &dyn Fn(Result<StreamInfo, String>),
    reported: &mut bool,
) -> Result<(), String>
where
    F: FnOnce(u32) -> C,
    C: AudioCallback,
{
    // SAFETY (whole function): raw COM/Win32 calls with valid arguments; every
    // out-pointer refers to a live local.
    unsafe {
        let hr = CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED);
        check(hr, "CoInitializeEx")?;
        let _apartment = ComApartment(true);

        let mut p = ptr::null_mut();
        check(
            CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut p,
            ),
            "creating the device enumerator",
        )?;
        let enumerator = Com(p);

        let mut p = ptr::null_mut();
        check(
            (enumerator.vtbl::<IMMDeviceEnumeratorVtbl>().get_default_audio_endpoint)(
                enumerator.0,
                E_RENDER,
                E_CONSOLE,
                &mut p,
            ),
            "finding the default output device",
        )?;
        let device = Com(p);

        let mut p = ptr::null_mut();
        check(
            (device.vtbl::<IMMDeviceVtbl>().activate)(
                device.0,
                &IID_IAUDIO_CLIENT,
                CLSCTX_ALL,
                ptr::null_mut(),
                &mut p,
            ),
            "activating the audio client",
        )?;
        let client = Com(p);
        let ac = client.vtbl::<IAudioClientVtbl>();

        let mut mix: *mut WaveFormatEx = ptr::null_mut();
        check((ac.get_mix_format)(client.0, &mut mix), "querying the mix format")?;
        let wf = ptr::read_unaligned(mix);
        let (tag, bits) = (wf.tag, wf.bits_per_sample);
        let effective_tag = if tag == WAVE_FORMAT_EXTENSIBLE && wf.cb_size >= 22 {
            let ext = ptr::read_unaligned(mix as *const WaveFormatExtensible);
            let sub = ext.sub_format;
            if sub == SUBTYPE_IEEE_FLOAT {
                WAVE_FORMAT_IEEE_FLOAT
            } else if sub == SUBTYPE_PCM {
                WAVE_FORMAT_PCM
            } else {
                0
            }
        } else {
            tag
        };
        let format = match (effective_tag, bits) {
            (WAVE_FORMAT_IEEE_FLOAT, 32) => Some(DeviceFormat::F32),
            (WAVE_FORMAT_PCM, 16) => Some(DeviceFormat::I16),
            (WAVE_FORMAT_PCM, 24) => Some(DeviceFormat::I24),
            (WAVE_FORMAT_PCM, 32) => Some(DeviceFormat::I32),
            _ => None,
        };
        let sample_rate = wf.samples_per_sec;
        let channels = wf.channels;
        let hr = (ac.initialize)(
            client.0,
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            BUFFER_DURATION,
            0,
            mix,
            ptr::null(),
        );
        CoTaskMemFree(mix as *mut c_void);
        let format = format.ok_or_else(|| format!("unsupported device format (tag {effective_tag}, {bits} bits)"))?;
        check(hr, "initializing the audio client")?;
        if channels == 0 {
            return Err("device reports zero channels".into());
        }

        let event = Event(CreateEventW(ptr::null_mut(), 0, 0, ptr::null()));
        if event.0.is_null() {
            return Err("CreateEventW failed".into());
        }
        check((ac.set_event_handle)(client.0, event.0), "setting the event handle")?;

        let mut buffer_frames = 0u32;
        check((ac.get_buffer_size)(client.0, &mut buffer_frames), "querying the buffer size")?;

        let mut p = ptr::null_mut();
        check((ac.get_service)(client.0, &IID_IAUDIO_RENDER_CLIENT, &mut p), "getting the render client")?;
        let render = Com(p);
        let rc = render.vtbl::<IAudioRenderClientVtbl>();

        // Allocation happens here, before the stream starts.
        let mut callback = factory(sample_rate);
        let mut scratch = vec![0.0f32; buffer_frames as usize * 2];
        let ch = usize::from(channels);
        let width = format.bytes();

        // Pre-fill with silence so the first period does not underrun.
        let mut data: *mut u8 = ptr::null_mut();
        check((rc.get_buffer)(render.0, buffer_frames, &mut data), "pre-filling the buffer")?;
        check((rc.release_buffer)(render.0, buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT), "pre-filling the buffer")?;

        let task_name: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
        let mut task_index = 0u32;
        let _mmcss = MmcssTask(AvSetMmThreadCharacteristicsW(task_name.as_ptr(), &mut task_index));

        check((ac.start)(client.0), "starting the stream")?;
        *reported = true;
        report(Ok(StreamInfo {
            backend: "WASAPI",
            sample_rate,
            device_channels: channels,
            buffer_frames,
            format: format!("{format:?}"),
        }));

        let mut result = Ok(());
        while !stop.load(Ordering::Acquire) {
            if WaitForSingleObject(event.0, 2000) != WAIT_OBJECT_0 {
                result = Err("audio device stopped responding".to_string());
                break;
            }
            let mut padding = 0u32;
            if let Err(e) = check((ac.get_current_padding)(client.0, &mut padding), "GetCurrentPadding") {
                result = Err(format!("audio device lost: {e}"));
                break;
            }
            let frames = buffer_frames.saturating_sub(padding) as usize;
            if frames == 0 {
                continue;
            }
            let mut data: *mut u8 = ptr::null_mut();
            if let Err(e) = check((rc.get_buffer)(render.0, frames as u32, &mut data), "GetBuffer") {
                result = Err(format!("audio device lost: {e}"));
                break;
            }
            let src = &mut scratch[..frames * 2];
            callback.process(src);
            let dst = std::slice::from_raw_parts_mut(data, frames * ch * width);
            convert_stereo(src, dst, ch, format);
            if let Err(e) = check((rc.release_buffer)(render.0, frames as u32, 0), "ReleaseBuffer") {
                result = Err(format!("audio device lost: {e}"));
                break;
            }
        }
        (ac.stop)(client.0);
        result
    }
}
