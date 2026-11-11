//! Windows output through WASAPI in shared, event-driven mode.
//!
//! COM interfaces are called through hand-written vtable layouts. Only the
//! vtable prefix up to the last method used is declared; the order of
//! methods follows the Windows SDK headers (mmdeviceapi.h, audioclient.h,
//! propsys.h).
//!
//! In low-latency mode the stream asks `IAudioClient3` (Windows 10 and
//! later) for the engine's smallest shared-mode period; if the interface or
//! that period is unavailable it falls back to the regular 20 ms buffer.

use super::{
    AudioCallback, Device, DeviceFormat, StreamConfig, StreamHealth, StreamInfo, convert_stereo, spawn_audio_thread,
};
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
const IID_IAUDIO_CLIENT3: Guid =
    Guid { d1: 0x7ED4_EE07, d2: 0x8E67, d3: 0x4CD4, d4: [0x8C, 0x1A, 0x2B, 0x7A, 0x59, 0x87, 0xAD, 0x42] };
/// PKEY_Device_FriendlyName (functiondiscoverykeys_devpkey.h).
const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = PropertyKey {
    fmtid: Guid { d1: 0xA45C_254E, d2: 0xDF1C, d3: 0x4EFD, d4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0] },
    pid: 14,
};
const IID_IAUDIO_RENDER_CLIENT: Guid =
    Guid { d1: 0xF294_ACFC, d2: 0x3146, d3: 0x4483, d4: [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2] };
const SUBTYPE_PCM: Guid = Guid { d1: 1, d2: 0, d3: 0x10, d4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71] };
const SUBTYPE_IEEE_FLOAT: Guid = Guid { d1: 3, d2: 0, d3: 0x10, d4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71] };

const COINIT_MULTITHREADED: u32 = 0;
/// CoInitializeEx on a thread already in another apartment: COM is usable,
/// but this call must not be balanced by CoUninitialize.
const RPC_E_CHANGED_MODE: Hresult = 0x8001_0106_u32 as Hresult;
const DEVICE_STATE_ACTIVE: u32 = 1;
const STGM_READ: u32 = 0;
const VT_LPWSTR: u16 = 31;
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
    fn PropVariantClear(pv: *mut PropVariant) -> Hresult;
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
    enum_audio_endpoints: unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void) -> Hresult,
    get_default_audio_endpoint: unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> Hresult,
    get_device: unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> Hresult,
}

#[repr(C)]
struct IMMDeviceCollectionVtbl {
    base: IUnknownVtbl,
    get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult,
    item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> Hresult,
}

#[repr(C)]
struct IMMDeviceVtbl {
    base: IUnknownVtbl,
    activate: unsafe extern "system" fn(*mut c_void, *const Guid, u32, *mut c_void, *mut *mut c_void) -> Hresult,
    open_property_store: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> Hresult,
    get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> Hresult,
}

#[repr(C)]
struct IPropertyStoreVtbl {
    base: IUnknownVtbl,
    _get_count: usize,
    _get_at: usize,
    get_value: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> Hresult,
}

#[repr(C)]
struct PropertyKey {
    fmtid: Guid,
    pid: u32,
}

/// PROPVARIANT: a type tag, three reserved words and a 16-byte (8 on 32-bit
/// Windows) union, of which only the string pointer is read here.
#[repr(C)]
struct PropVariant {
    vt: u16,
    _reserved: [u16; 3],
    data: [usize; 2],
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
    get_device_period: unsafe extern "system" fn(*mut c_void, *mut i64, *mut i64) -> Hresult,
    start: unsafe extern "system" fn(*mut c_void) -> Hresult,
    stop: unsafe extern "system" fn(*mut c_void) -> Hresult,
    _reset: usize,
    set_event_handle: unsafe extern "system" fn(*mut c_void, Handle) -> Hresult,
    get_service: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult,
}

/// IAudioClient, then IAudioClient2's three methods, then IAudioClient3's.
#[repr(C)]
struct IAudioClient3Vtbl {
    client: IAudioClientVtbl,
    _is_offload_capable: usize,
    _set_client_properties: usize,
    _get_buffer_size_limits: usize,
    get_shared_mode_engine_period:
        unsafe extern "system" fn(*mut c_void, *const WaveFormatEx, *mut u32, *mut u32, *mut u32, *mut u32) -> Hresult,
    _get_current_shared_mode_engine_period: usize,
    initialize_shared_audio_stream:
        unsafe extern "system" fn(*mut c_void, u32, u32, *const WaveFormatEx, *const Guid) -> Hresult,
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

impl ComApartment {
    /// Joins the multithreaded apartment on this thread for as long as the
    /// guard lives.
    fn enter() -> Result<Self, String> {
        // SAFETY: plain call; balanced by the guard's drop when it succeeds.
        let hr = unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        check(hr, "CoInitializeEx")?;
        Ok(Self(true))
    }
}

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

/// NUL-terminated UTF-16.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Reads a NUL-terminated UTF-16 string.
///
/// # Safety
/// `p` must point at a NUL-terminated UTF-16 string.
unsafe fn from_wide(p: *const u16) -> String {
    // SAFETY: the caller guarantees a terminator, so every read is in bounds.
    unsafe {
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}

fn enumerator() -> Result<Com, String> {
    let mut p = ptr::null_mut();
    // SAFETY: valid GUIDs and out-pointer; COM is initialized by the caller.
    let hr = unsafe {
        CoCreateInstance(&CLSID_MM_DEVICE_ENUMERATOR, ptr::null_mut(), CLSCTX_ALL, &IID_IMM_DEVICE_ENUMERATOR, &mut p)
    };
    check(hr, "creating the device enumerator")?;
    Ok(Com(p))
}

/// The system's default render device.
fn default_device(enumerator: &Com) -> Result<Com, String> {
    let mut p = ptr::null_mut();
    // SAFETY: valid enumerator and out-pointer.
    let hr = unsafe {
        (enumerator.vtbl::<IMMDeviceEnumeratorVtbl>().get_default_audio_endpoint)(
            enumerator.0,
            E_RENDER,
            E_CONSOLE,
            &mut p,
        )
    };
    check(hr, "finding the default output device")?;
    Ok(Com(p))
}

/// The endpoint id of `device`.
fn device_id(device: &Com) -> Result<String, String> {
    let mut p: *mut u16 = ptr::null_mut();
    // SAFETY: valid device and out-pointer; the string is freed with
    // CoTaskMemFree as documented.
    unsafe {
        check((device.vtbl::<IMMDeviceVtbl>().get_id)(device.0, &mut p), "reading a device id")?;
        let id = from_wide(p);
        CoTaskMemFree(p.cast());
        Ok(id)
    }
}

/// The friendly name of `device` ("Speakers (Realtek(R) Audio)").
fn device_name(device: &Com) -> Result<String, String> {
    let mut p = ptr::null_mut();
    // SAFETY: valid device, key and out-pointers; the PROPVARIANT is cleared
    // after its string has been copied.
    unsafe {
        check((device.vtbl::<IMMDeviceVtbl>().open_property_store)(device.0, STGM_READ, &mut p), "opening properties")?;
        let store = Com(p);
        let mut v = PropVariant { vt: 0, _reserved: [0; 3], data: [0; 2] };
        check(
            (store.vtbl::<IPropertyStoreVtbl>().get_value)(store.0, &PKEY_DEVICE_FRIENDLY_NAME, &mut v),
            "reading a device name",
        )?;
        let name = if v.vt == VT_LPWSTR && v.data[0] != 0 { from_wide(v.data[0] as *const u16) } else { String::new() };
        PropVariantClear(&mut v);
        Ok(name)
    }
}

/// Lists the active render devices.
pub fn devices() -> Result<Vec<Device>, String> {
    let _apartment = ComApartment::enter()?;
    let enumerator = enumerator()?;
    let default = default_device(&enumerator).and_then(|d| device_id(&d)).ok();
    let mut p = ptr::null_mut();
    // SAFETY: valid enumerator and out-pointer.
    let hr = unsafe {
        (enumerator.vtbl::<IMMDeviceEnumeratorVtbl>().enum_audio_endpoints)(
            enumerator.0,
            E_RENDER,
            DEVICE_STATE_ACTIVE,
            &mut p,
        )
    };
    check(hr, "listing output devices")?;
    let collection = Com(p);
    let vt = unsafe { collection.vtbl::<IMMDeviceCollectionVtbl>() };
    let mut count = 0u32;
    // SAFETY: valid collection and out-pointer.
    check(unsafe { (vt.get_count)(collection.0, &mut count) }, "counting output devices")?;
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut p = ptr::null_mut();
        // SAFETY: `i` is below the count; valid out-pointer.
        check(unsafe { (vt.item)(collection.0, i, &mut p) }, "reading an output device")?;
        let device = Com(p);
        let id = device_id(&device)?;
        let name = device_name(&device)?;
        out.push(Device { is_default: default.as_deref() == Some(id.as_str()), id, name });
    }
    Ok(out)
}

/// Id of the current default render device.
pub fn default_id() -> Option<String> {
    let _apartment = ComApartment::enter().ok()?;
    default_device(&enumerator().ok()?).and_then(|d| device_id(&d)).ok()
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
        // SAFETY: all COM calls follow the documented WASAPI contracts; the
        // objects live (and are released) on this thread.
        if let Err(e) = unsafe { run(&config, factory, &stop, report) } {
            health.fail(e);
        }
    })
}

/// Opens the device and runs the render loop until `stop` is set.
///
/// Errors before the stream is running are passed to `report`; errors after
/// that are returned so the caller can record them.
unsafe fn run<F, C>(
    config: &StreamConfig,
    factory: F,
    stop: &AtomicBool,
    report: &dyn Fn(Result<StreamInfo, String>),
) -> Result<(), String>
where
    F: FnOnce(u32) -> C,
    C: AudioCallback,
{
    let mut reported = false;
    let result = unsafe { run_inner(config, factory, stop, report, &mut reported) };
    match result {
        Err(e) if !reported => {
            report(Err(e));
            Ok(())
        }
        other => other,
    }
}

/// Activates an audio client of interface `iid` on `device`.
unsafe fn activate(device: &Com, iid: &Guid) -> Result<Com, String> {
    let mut p = ptr::null_mut();
    // SAFETY: valid device, GUID and out-pointer.
    let hr = unsafe { (device.vtbl::<IMMDeviceVtbl>().activate)(device.0, iid, CLSCTX_ALL, ptr::null_mut(), &mut p) };
    check(hr, "activating the audio client")?;
    Ok(Com(p))
}

/// Initializes an `IAudioClient3` at the engine's smallest period for
/// `format`. Returns the period in frames.
unsafe fn init_low_latency(client: &Com, format: *const WaveFormatEx) -> Result<u32, String> {
    // SAFETY: `client` was activated as IAudioClient3, whose vtable
    // `IAudioClient3Vtbl` describes; valid out-pointers.
    unsafe {
        let vt = client.vtbl::<IAudioClient3Vtbl>();
        let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
        check(
            (vt.get_shared_mode_engine_period)(client.0, format, &mut default, &mut fundamental, &mut min, &mut max),
            "querying engine periods",
        )?;
        // Drivers without small-buffer support report min == default; such a
        // stream would be no faster, so it is not called low latency.
        if min == 0 || min >= default {
            return Err("the driver offers no period below its default".into());
        }
        check(
            (vt.initialize_shared_audio_stream)(client.0, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, min, format, ptr::null()),
            "initializing a low-latency stream",
        )?;
        Ok(min)
    }
}

unsafe fn run_inner<F, C>(
    config: &StreamConfig,
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
        let _apartment = ComApartment::enter()?;
        let enumerator = enumerator()?;
        let device = match &config.device {
            None => default_device(&enumerator)?,
            Some(id) => {
                let mut p = ptr::null_mut();
                let w = wide(id);
                let hr = (enumerator.vtbl::<IMMDeviceEnumeratorVtbl>().get_device)(enumerator.0, w.as_ptr(), &mut p);
                check(hr, "opening the chosen output device (is it still connected?)")?;
                Com(p)
            }
        };
        let device_label = device_name(&device).unwrap_or_default();

        let mut client = activate(&device, &IID_IAUDIO_CLIENT)?;
        let mut mix: *mut WaveFormatEx = ptr::null_mut();
        check((client.vtbl::<IAudioClientVtbl>().get_mix_format)(client.0, &mut mix), "querying the mix format")?;
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
        // A low-latency stream needs its own client; a client whose
        // initialization failed cannot be initialized again.
        let mut low_period = None;
        if config.low_latency && format.is_some() {
            if let Ok(c3) = activate(&device, &IID_IAUDIO_CLIENT3) {
                if let Ok(period) = init_low_latency(&c3, mix) {
                    client = c3;
                    low_period = Some(period);
                }
            }
        }
        let hr = if low_period.is_some() {
            0
        } else {
            (client.vtbl::<IAudioClientVtbl>().initialize)(
                client.0,
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_DURATION,
                0,
                mix,
                ptr::null(),
            )
        };
        CoTaskMemFree(mix as *mut c_void);
        let format = format.ok_or_else(|| format!("unsupported device format (tag {effective_tag}, {bits} bits)"))?;
        check(hr, "initializing the audio client")?;
        let ac = client.vtbl::<IAudioClientVtbl>();
        let period_frames = match low_period {
            Some(p) => p,
            None => {
                let (mut default, mut min) = (0i64, 0i64);
                check((ac.get_device_period)(client.0, &mut default, &mut min), "querying the device period")?;
                (default as f64 * f64::from(sample_rate) / 1e7).round() as u32
            }
        };
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
            device: device_label,
            sample_rate,
            device_channels: channels,
            buffer_frames,
            period_frames,
            low_latency: low_period.is_some(),
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
