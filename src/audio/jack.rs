//! Linux output and input through a JACK server: jackd, or PipeWire's own
//! JACK implementation, which puts TINVO straight into PipeWire's graph
//! instead of going through its ALSA plugin.
//!
//! libjack is loaded with `dlopen` the first time it is needed rather than
//! linked, so it stays optional: where it is not installed, TINVO starts and
//! plays through ALSA as before. JACK calls the process callback on its own
//! real-time thread; TINVO's audio thread only owns the client and waits for
//! the stop request, as with CoreAudio. The ports are `tinvo:out_L` and
//! `tinvo:out_R` (`tinvo-in:in_L` and `in_R` when recording). On start they
//! are connected to the first two physical ports, and any patchbay can
//! rewire them.

use super::{
    AudioCallback, CaptureCallback, Device, InputInfo, StreamConfig, StreamHealth, StreamInfo, spawn_audio_thread,
};
use std::ffi::{CStr, CString, c_char, c_int, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// The device id that opens JACK.
pub const DEVICE_ID: &str = "jack";

const LIBRARY: &CStr = c"libjack.so.0";
const RTLD_NOW: c_int = 2;
const JACK_NO_START_SERVER: c_int = 0x01;
const JACK_SERVER_FAILED: c_int = 0x10;
const PORT_IS_INPUT: c_ulong = 0x1;
const PORT_IS_OUTPUT: c_ulong = 0x2;
const PORT_IS_PHYSICAL: c_ulong = 0x4;
const AUDIO_TYPE: &CStr = c"32 bit float mono audio";
const EEXIST: c_int = 17;
/// Frames per call into TINVO's callbacks; longer JACK periods take
/// several calls, so the scratch buffer never has to grow.
const CHUNK: usize = 1024;

unsafe extern "C" {
    fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
}

type ProcessCallback = unsafe extern "C" fn(u32, *mut c_void) -> c_int;
type ShutdownCallback = unsafe extern "C" fn(*mut c_void);
type MessageFunction = unsafe extern "C" fn(*const c_char);

/// The libjack functions TINVO calls, as declared in `<jack/jack.h>`.
struct Lib {
    client_open: unsafe extern "C" fn(*const c_char, c_int, *mut c_int, ...) -> *mut c_void,
    client_close: unsafe extern "C" fn(*mut c_void) -> c_int,
    activate: unsafe extern "C" fn(*mut c_void) -> c_int,
    deactivate: unsafe extern "C" fn(*mut c_void) -> c_int,
    sample_rate: unsafe extern "C" fn(*mut c_void) -> u32,
    buffer_size: unsafe extern "C" fn(*mut c_void) -> u32,
    set_process_callback: unsafe extern "C" fn(*mut c_void, ProcessCallback, *mut c_void) -> c_int,
    on_shutdown: unsafe extern "C" fn(*mut c_void, ShutdownCallback, *mut c_void),
    port_register: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_ulong, c_ulong) -> *mut c_void,
    port_get_buffer: PortGetBuffer,
    port_name: unsafe extern "C" fn(*const c_void) -> *const c_char,
    get_ports: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_ulong) -> *mut *const c_char,
    connect: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    #[cfg_attr(not(test), allow(dead_code))]
    disconnect: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    free: unsafe extern "C" fn(*mut c_void),
}

type PortGetBuffer = unsafe extern "C" fn(*mut c_void, u32) -> *mut c_void;

/// libjack's own diagnostics go to stderr, which would scribble over the
/// editor; TINVO reports failures itself.
unsafe extern "C" fn quiet(_message: *const c_char) {}

/// Looks `name` up in the loaded library as a function of type `T`.
unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &CStr) -> Result<T, String> {
    // SAFETY: a handle from dlopen and a NUL-terminated name.
    let p = unsafe { dlsym(handle, name.as_ptr()) };
    if p.is_null() {
        return Err(format!("{} has no {}", LIBRARY.to_string_lossy(), name.to_string_lossy()));
    }
    assert_eq!(size_of::<T>(), size_of::<*mut c_void>());
    // SAFETY: T is a function pointer type matching the C declaration.
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

fn load() -> Result<Lib, String> {
    // SAFETY: a NUL-terminated library name; the handle is never closed, so
    // the function pointers stay valid for the life of the process.
    let handle = unsafe { dlopen(LIBRARY.as_ptr(), RTLD_NOW) };
    if handle.is_null() {
        // SAFETY: dlerror returns a NUL-terminated message or null.
        let why = unsafe { dlerror() };
        let why = if why.is_null() {
            String::new()
        } else {
            // SAFETY: just checked non-null.
            format!(": {}", unsafe { CStr::from_ptr(why) }.to_string_lossy())
        };
        return Err(format!("JACK is not installed ({} not found{why})", LIBRARY.to_string_lossy()));
    }
    // SAFETY: each symbol is looked up with the type <jack/jack.h> declares.
    unsafe {
        let set_error: unsafe extern "C" fn(MessageFunction) = symbol(handle, c"jack_set_error_function")?;
        let set_info: unsafe extern "C" fn(MessageFunction) = symbol(handle, c"jack_set_info_function")?;
        set_error(quiet);
        set_info(quiet);
        Ok(Lib {
            client_open: symbol(handle, c"jack_client_open")?,
            client_close: symbol(handle, c"jack_client_close")?,
            activate: symbol(handle, c"jack_activate")?,
            deactivate: symbol(handle, c"jack_deactivate")?,
            sample_rate: symbol(handle, c"jack_get_sample_rate")?,
            buffer_size: symbol(handle, c"jack_get_buffer_size")?,
            set_process_callback: symbol(handle, c"jack_set_process_callback")?,
            on_shutdown: symbol(handle, c"jack_on_shutdown")?,
            port_register: symbol(handle, c"jack_port_register")?,
            port_get_buffer: symbol(handle, c"jack_port_get_buffer")?,
            port_name: symbol(handle, c"jack_port_name")?,
            get_ports: symbol(handle, c"jack_get_ports")?,
            connect: symbol(handle, c"jack_connect")?,
            disconnect: symbol(handle, c"jack_disconnect")?,
            free: symbol(handle, c"jack_free")?,
        })
    }
}

fn lib() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// An open JACK client, closed on drop.
struct Client {
    lib: &'static Lib,
    ptr: *mut c_void,
}

impl Client {
    /// Connects to the running server; never starts one.
    fn open(name: &CStr) -> Result<Self, String> {
        let lib = lib()?;
        let mut status: c_int = 0;
        // SAFETY: a NUL-terminated name and a valid status out-pointer; no
        // variadic arguments follow because no option asks for one.
        let ptr = unsafe { (lib.client_open)(name.as_ptr(), JACK_NO_START_SERVER, &mut status) };
        if ptr.is_null() {
            return Err(if status & JACK_SERVER_FAILED != 0 {
                "no JACK server is running".into()
            } else {
                format!("cannot open a JACK client (status {status:#x})")
            });
        }
        Ok(Self { lib, ptr })
    }

    /// Full names of the audio ports with all of `flags`.
    fn ports(&self, flags: c_ulong) -> Vec<String> {
        // SAFETY: an open client; null pattern matches every name.
        let list = unsafe { (self.lib.get_ports)(self.ptr, ptr::null(), AUDIO_TYPE.as_ptr(), flags) };
        let mut out = Vec::new();
        if list.is_null() {
            return out;
        }
        for i in 0.. {
            // SAFETY: a NULL-terminated array of NUL-terminated names.
            let name = unsafe { *list.add(i) };
            if name.is_null() {
                break;
            }
            // SAFETY: a live NUL-terminated name.
            out.push(unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned());
        }
        // SAFETY: freeing the array jack_get_ports allocated, once.
        unsafe { (self.lib.free)(list.cast()) };
        out
    }

    fn register(&self, name: &CStr, flags: c_ulong) -> Result<*mut c_void, String> {
        // SAFETY: an open client and NUL-terminated names; buffer size 0
        // is required for the built-in audio type.
        let port = unsafe { (self.lib.port_register)(self.ptr, name.as_ptr(), AUDIO_TYPE.as_ptr(), flags, 0) };
        if port.is_null() {
            return Err(format!("cannot register the JACK port {}", name.to_string_lossy()));
        }
        Ok(port)
    }

    fn port_name(&self, port: *mut c_void) -> String {
        // SAFETY: a port of this client; the name lives as long as the port.
        unsafe { CStr::from_ptr((self.lib.port_name)(port)) }.to_string_lossy().into_owned()
    }

    fn connect(&self, from: &str, to: &str) -> Result<(), String> {
        let (f, t) = (CString::new(from).map_err(|e| e.to_string())?, CString::new(to).map_err(|e| e.to_string())?);
        // SAFETY: an active client and NUL-terminated port names.
        match unsafe { (self.lib.connect)(self.ptr, f.as_ptr(), t.as_ptr()) } {
            0 | EEXIST => Ok(()),
            e => Err(format!("cannot connect JACK port {from} to {to} (error {e})")),
        }
    }

    #[cfg(test)]
    fn disconnect(&self, from: &str, to: &str) {
        let (f, t) = (CString::new(from).unwrap(), CString::new(to).unwrap());
        // SAFETY: an active client and NUL-terminated port names.
        unsafe { (self.lib.disconnect)(self.ptr, f.as_ptr(), t.as_ptr()) };
    }

    /// Installs the callbacks and activates the client. `state` and `dead`
    /// must outlive the client.
    fn start(&self, process: ProcessCallback, state: *mut c_void, dead: &Arc<AtomicBool>) -> Result<(), String> {
        // SAFETY: an open, inactive client; the caller keeps `state` and
        // `dead` alive until the client is closed.
        unsafe {
            if (self.lib.set_process_callback)(self.ptr, process, state) != 0 {
                return Err("cannot set the JACK process callback".into());
            }
            (self.lib.on_shutdown)(self.ptr, on_shutdown, Arc::as_ptr(dead).cast_mut().cast());
            if (self.lib.activate)(self.ptr) != 0 {
                return Err("cannot activate the JACK client".into());
            }
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: an open client, closed once. Deactivating first means no
        // callback runs after this returns, so its state can be freed;
        // deactivating an inactive client is harmless.
        unsafe {
            (self.lib.deactivate)(self.ptr);
            (self.lib.client_close)(self.ptr);
        }
    }
}

unsafe extern "C" fn on_shutdown(arg: *mut c_void) {
    // SAFETY: `arg` is the `dead` flag `start` registered, alive until the
    // client is closed.
    unsafe { &*(arg as *const AtomicBool) }.store(true, Ordering::Release);
}

/// The device name for a stream connected to `ports`: JACK and the client
/// owning them (`system` for jackd's hardware, the card's name under
/// PipeWire).
fn describe(ports: &[String]) -> String {
    match ports.first().and_then(|p| p.split_once(':')) {
        Some((client, _)) => format!("JACK ({client})"),
        None => "JACK (no physical ports)".into(),
    }
}

/// Whether `port` (a full `client:port` name) is one of the monitor ports
/// PipeWire shows for each sink (`…:monitor_FL`). They carry what is being
/// played, so they are never taken for a capture source.
fn is_monitor(port: &str) -> bool {
    port.split_once(':').is_some_and(|(_, name)| name.starts_with("monitor_"))
}

impl Client {
    /// Physical playback ports; jackd and PipeWire list the default
    /// device's first.
    fn playback_ports(&self) -> Vec<String> {
        self.ports(PORT_IS_PHYSICAL | PORT_IS_INPUT)
    }

    /// Physical capture ports, without monitors.
    fn capture_ports(&self) -> Vec<String> {
        self.ports(PORT_IS_PHYSICAL | PORT_IS_OUTPUT).into_iter().filter(|p| !is_monitor(p)).collect()
    }
}

/// The JACK playback entry, when a server is running.
pub fn devices() -> Vec<Device> {
    listing(Client::playback_ports)
}

/// The JACK capture entry, when a server is running.
pub fn input_devices() -> Vec<Device> {
    listing(Client::capture_ports)
}

fn listing(ports: fn(&Client) -> Vec<String>) -> Vec<Device> {
    match Client::open(c"tinvo-list") {
        Ok(client) => vec![Device { id: DEVICE_ID.into(), name: describe(&ports(&client)), is_default: false }],
        Err(_) => Vec::new(),
    }
}

/// Waits on the audio thread until the stream is stopped or the server
/// goes away.
fn wait(stop: &AtomicBool, dead: &AtomicBool, health: &StreamHealth) {
    while !stop.load(Ordering::Acquire) {
        if dead.load(Ordering::Acquire) {
            health.fail("the JACK server stopped".into());
            return;
        }
        std::thread::park_timeout(Duration::from_millis(50));
    }
}

struct Output<C> {
    callback: C,
    ports: [*mut c_void; 2],
    port_get_buffer: PortGetBuffer,
    scratch: Box<[f32]>,
}

unsafe extern "C" fn render<C: AudioCallback>(frames: u32, arg: *mut c_void) -> c_int {
    // SAFETY: `arg` is the `Output<C>` boxed in `open_named`, alive until
    // the client is closed; JACK never runs one client's callback twice at once.
    let state = unsafe { &mut *(arg as *mut Output<C>) };
    let n = frames as usize;
    // SAFETY: JACK hands each output port a buffer of `frames` samples for
    // this cycle.
    let (left, right) = unsafe {
        (
            std::slice::from_raw_parts_mut((state.port_get_buffer)(state.ports[0], frames) as *mut f32, n),
            std::slice::from_raw_parts_mut((state.port_get_buffer)(state.ports[1], frames) as *mut f32, n),
        )
    };
    let mut done = 0;
    while done < n {
        let k = (n - done).min(CHUNK);
        let buf = &mut state.scratch[..k * 2];
        state.callback.process(buf);
        for i in 0..k {
            left[done + i] = super::sanitize(buf[2 * i]);
            right[done + i] = super::sanitize(buf[2 * i + 1]);
        }
        done += k;
    }
    0
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
    debug_assert_eq!(config.device.as_deref(), Some(DEVICE_ID));
    open_named(c"tinvo", factory, stop, health)
}

fn open_named<F, C>(
    name: &'static CStr,
    factory: F,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
) -> Result<(StreamInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32) -> C + Send + 'static,
    C: AudioCallback,
{
    spawn_audio_thread(move |report| {
        let client = match Client::open(name) {
            Ok(c) => c,
            Err(e) => return report(Err(e)),
        };
        let ports = match (client.register(c"out_L", PORT_IS_OUTPUT), client.register(c"out_R", PORT_IS_OUTPUT)) {
            (Ok(l), Ok(r)) => [l, r],
            (Err(e), _) | (_, Err(e)) => return report(Err(e)),
        };
        // SAFETY: an open client.
        let (rate, period) = unsafe { ((client.lib.sample_rate)(client.ptr), (client.lib.buffer_size)(client.ptr)) };
        let state = Box::into_raw(Box::new(Output {
            callback: factory(rate),
            ports,
            port_get_buffer: client.lib.port_get_buffer,
            scratch: vec![0.0; CHUNK * 2].into_boxed_slice(),
        }));
        let dead = Arc::new(AtomicBool::new(false));
        let targets = client.playback_ports();
        let started = client.start(render::<C>, state.cast(), &dead).and_then(|()| {
            for (port, target) in ports.iter().zip(&targets) {
                client.connect(&client.port_name(*port), target)?;
            }
            Ok(())
        });
        match started {
            Ok(()) => {
                report(Ok(StreamInfo {
                    backend: "JACK",
                    device: describe(&targets),
                    sample_rate: rate,
                    device_channels: 2,
                    buffer_frames: period,
                    period_frames: period,
                    low_latency: false,
                    format: "F32".into(),
                }));
                wait(&stop, &dead, &health);
            }
            Err(e) => report(Err(e)),
        }
        drop(client);
        // SAFETY: the client is closed, so no callback can still use it.
        drop(unsafe { Box::from_raw(state) });
    })
}

struct Input<C> {
    callback: C,
    ports: [*mut c_void; 2],
    port_get_buffer: PortGetBuffer,
    scratch: Box<[f32]>,
}

unsafe extern "C" fn capture<C: CaptureCallback>(frames: u32, arg: *mut c_void) -> c_int {
    // SAFETY: `arg` is the `Input<C>` boxed in `open_input_named`, alive
    // until the client is closed.
    let state = unsafe { &mut *(arg as *mut Input<C>) };
    let n = frames as usize;
    // SAFETY: JACK hands each input port a buffer of `frames` samples.
    let (left, right) = unsafe {
        (
            std::slice::from_raw_parts((state.port_get_buffer)(state.ports[0], frames) as *const f32, n),
            std::slice::from_raw_parts((state.port_get_buffer)(state.ports[1], frames) as *const f32, n),
        )
    };
    let mut done = 0;
    while done < n {
        let k = (n - done).min(CHUNK);
        let buf = &mut state.scratch[..k * 2];
        for i in 0..k {
            buf[2 * i] = left[done + i];
            buf[2 * i + 1] = right[done + i];
        }
        state.callback.capture(buf);
        done += k;
    }
    0
}

pub fn open_input<F, C>(
    factory: F,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
) -> Result<(InputInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32, u16) -> C + Send + 'static,
    C: CaptureCallback,
{
    open_input_named(c"tinvo-in", factory, stop, health)
}

fn open_input_named<F, C>(
    name: &'static CStr,
    factory: F,
    stop: Arc<AtomicBool>,
    health: Arc<StreamHealth>,
) -> Result<(InputInfo, JoinHandle<()>), String>
where
    F: FnOnce(u32, u16) -> C + Send + 'static,
    C: CaptureCallback,
{
    spawn_audio_thread(move |report| {
        let client = match Client::open(name) {
            Ok(c) => c,
            Err(e) => return report(Err(e)),
        };
        let ports = match (client.register(c"in_L", PORT_IS_INPUT), client.register(c"in_R", PORT_IS_INPUT)) {
            (Ok(l), Ok(r)) => [l, r],
            (Err(e), _) | (_, Err(e)) => return report(Err(e)),
        };
        // SAFETY: an open client.
        let rate = unsafe { (client.lib.sample_rate)(client.ptr) };
        let state = Box::into_raw(Box::new(Input {
            callback: factory(rate, 2),
            ports,
            port_get_buffer: client.lib.port_get_buffer,
            scratch: vec![0.0; CHUNK * 2].into_boxed_slice(),
        }));
        let dead = Arc::new(AtomicBool::new(false));
        let sources = client.capture_ports();
        let started = client.start(capture::<C>, state.cast(), &dead).and_then(|()| {
            for (source, port) in sources.iter().zip(&ports) {
                client.connect(source, &client.port_name(*port))?;
            }
            Ok(())
        });
        match started {
            Ok(()) => {
                report(Ok(InputInfo {
                    backend: "JACK",
                    device: describe(&sources),
                    sample_rate: rate,
                    device_channels: sources.len().min(2) as u16,
                    channels: 2,
                    format: "F32".into(),
                }));
                wait(&stop, &dead, &health);
            }
            Err(e) => report(Err(e)),
        }
        drop(client);
        // SAFETY: the client is closed, so no callback can still use it.
        drop(unsafe { Box::from_raw(state) });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{InputStream, Stream};
    use std::sync::Mutex;
    use std::time::Instant;

    /// Whether a JACK server is reachable. CI sets `TINVO_REQUIRE_JACK`
    /// where it runs one, so these tests cannot pass there by skipping.
    fn server() -> bool {
        match Client::open(c"tinvo-probe") {
            Ok(_) => true,
            Err(e) if std::env::var_os("TINVO_REQUIRE_JACK").is_some() => {
                panic!("TINVO_REQUIRE_JACK is set but JACK is unavailable: {e}")
            }
            Err(e) => {
                eprintln!("{e}; skipping");
                false
            }
        }
    }

    /// Exactly representable values in ±0.5, repeating every 4096 frames.
    fn ramp(n: u32) -> f32 {
        ((n % 4096) as f32 - 2048.0) / 4096.0
    }

    struct Ramp(u32);

    impl AudioCallback for Ramp {
        fn process(&mut self, out: &mut [f32]) {
            for frame in out.chunks_exact_mut(2) {
                frame[0] = ramp(self.0);
                frame[1] = -ramp(self.0);
                self.0 += 1;
            }
        }
    }

    #[test]
    fn monitor_ports_are_not_capture_sources() {
        assert!(is_monitor("Built-in Audio Analog Stereo:monitor_FL"));
        assert!(!is_monitor("Built-in Audio Analog Stereo:capture_FL"));
        assert!(!is_monitor("system:capture_1"));
        assert!(!is_monitor("monitor_FL"), "no client part: not a JACK port name");
        assert_eq!(describe(&["Built-in Audio:playback_FL".into()]), "JACK (Built-in Audio)");
        assert_eq!(describe(&[]), "JACK (no physical ports)");
    }

    #[test]
    fn the_jack_entry_is_listed_while_a_server_runs() {
        if !server() {
            return;
        }
        let out = crate::audio::output_devices().unwrap();
        assert_eq!(out.iter().filter(|d| d.id == DEVICE_ID).count(), 1, "{out:?}");
        assert!(crate::audio::input_devices().unwrap().iter().any(|d| d.id == DEVICE_ID));
    }

    /// Plays a ramp through a TINVO output client into a TINVO input client
    /// and checks every sample arrives unchanged and in order. Recording
    /// starts once the connections have settled: right after a connection
    /// JACK may reorder the graph, which shifts the path between the two
    /// clients by one period (seen as a jump of exactly one period).
    #[test]
    fn samples_go_through_jack_unchanged() {
        if !server() {
            return;
        }
        let health = Arc::new(StreamHealth::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (info, thread) = open_named(c"tinvo-test-out", |_| Ramp(0), stop.clone(), health.clone()).unwrap();
        let output = Stream { info, stop, health, thread: Some(thread) };
        assert_eq!(output.info().backend, "JACK");
        assert!(output.info().sample_rate > 0 && output.info().period_frames > 0);

        let got = Arc::new(Mutex::new(Vec::<f32>::with_capacity(1 << 20)));
        let sink = got.clone();
        let armed = Arc::new(AtomicBool::new(false));
        let recording = armed.clone();
        let health = Arc::new(StreamHealth::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (info, thread) = open_input_named(
            c"tinvo-test-in",
            move |_, channels| {
                assert_eq!(channels, 2);
                move |samples: &[f32]| {
                    if !recording.load(Ordering::Acquire) {
                        return;
                    }
                    let mut v = sink.lock().unwrap();
                    if v.len() + samples.len() <= v.capacity() {
                        v.extend_from_slice(samples);
                    }
                }
            },
            stop.clone(),
            health.clone(),
        )
        .unwrap();
        let input = InputStream { info, stop, health, thread: Some(thread) };
        assert_eq!(input.info().channels, 2);

        // Feed the input from the ramp alone.
        let patch = Client::open(c"tinvo-test-patch").unwrap();
        for source in patch.ports(PORT_IS_PHYSICAL | PORT_IS_OUTPUT) {
            patch.disconnect(&source, "tinvo-test-in:in_L");
            patch.disconnect(&source, "tinvo-test-in:in_R");
        }
        patch.connect("tinvo-test-out:out_L", "tinvo-test-in:in_L").unwrap();
        patch.connect("tinvo-test-out:out_R", "tinvo-test-in:in_R").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        armed.store(true, Ordering::Release);
        let begin = Instant::now();
        while got.lock().unwrap().len() < 2 * 9600 && begin.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(input);
        assert_eq!(output.error(), None);
        drop(output);

        let frames = got.lock().unwrap();
        assert!(frames.len() >= 2 * 9600, "only {} frames arrived", frames.len() / 2);
        let start = ((frames[0] * 4096.0) + 2048.0) as u32;
        for (i, f) in frames.chunks_exact(2).enumerate() {
            let want = ramp(start + i as u32);
            assert_eq!(f, [want, -want], "frame {i} after the first");
        }
    }
}
