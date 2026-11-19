//! Plugins played in this process: the host side of CLAP.
//!
//! A [`PluginHost`] lives on the thread that CLAP calls main for the
//! instances it creates: the program's main thread, as CLAP asks on Windows
//! and macOS (the editor's UI thread, or `tinvo render`'s). It loads plugin
//! files, creates, initialises and activates [`Instance`]s there, and
//! services what the plugins ask of it between frames ([`PluginHost::poll`]).
//! An instance is then moved to whichever thread plays CLAP's audio thread
//! (the audio callback, or the rendering thread itself), which processes it
//! and stops it; it comes back to its host's thread to be deactivated and
//! destroyed, which dropping it does.
//!
//! The rules CLAP sets, and how they are kept:
//! - main-thread functions (`init`, `activate`, `deactivate`, `destroy`,
//!   `on_main_thread`, the port queries) run only on the thread whose host
//!   created the instance, which is what the host's `clap.thread-check`
//!   reports as main for it: the host is not `Send`, [`Instance::restart`]
//!   refuses another thread, and an instance dropped on another thread is
//!   leaked rather than destroyed there;
//! - audio-thread functions (`start_processing`, `process`, `reset`,
//!   `stop_processing`) run inside an [`AudioRole`], which is what
//!   `clap.thread-check` reports as audio, on one thread at a time, since
//!   they take `&mut Instance`;
//! - a plugin file is initialised once while any instance uses it, and
//!   deinitialised only after the last of them is destroyed (dropping an
//!   instance, polling and dropping a host let go of files nothing uses, on
//!   whichever host's thread does so first, which CLAP allows);
//! - a thread that will process calls [`prepare_audio_thread`] before its
//!   first real-time call: on macOS a thread's first use of thread-local
//!   storage may allocate.
//!
//! Processing never allocates: the buffers and event lists are sized when
//! an instance is activated.

use super::ffi::*;
use super::library::Dso;
use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// Events an instance can be given between two process calls, and notes it
/// can hold: each held note keeps one of these for its release, so a
/// release always fits.
pub const MAX_EVENTS: usize = 512;
/// At most this many audio ports each way, and channels on one port.
const MAX_PORTS: u32 = 16;
const MAX_PORT_CHANNELS: u32 = 32;

/// The largest call an instance is activated for; longer ones are split.
pub const MAX_FRAMES: usize = 8192;

thread_local! {
    // Const-initialised and without destructors, so reading them allocates
    // nothing once the thread has used them (`prepare_audio_thread`).
    static AUDIO: Cell<bool> = const { Cell::new(false) };
    static KEY: Cell<u64> = const { Cell::new(0) };
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// A number for this thread, unique in the process (thread ids may be
/// reused); given the first time it is asked for, without allocating.
fn thread_key() -> u64 {
    KEY.with(|k| {
        if k.get() == 0 {
            k.set(NEXT_KEY.fetch_add(1, Ordering::Relaxed));
        }
        k.get()
    })
}

/// Whether this thread is playing CLAP's audio thread now.
pub fn is_audio_thread() -> bool {
    AUDIO.with(Cell::get)
}

/// Sets up this thread's thread-local storage for processing: called once
/// on a thread before it processes in real time, since on macOS a thread's
/// first use of it may allocate.
pub fn prepare_audio_thread() {
    AUDIO.with(|_| ());
    thread_key();
}

/// The current thread playing CLAP's audio thread, until dropped. Taken
/// around every audio-thread call into a plugin; the main thread takes it
/// too when it processes (rendering a file) or stops an instance the audio
/// thread left processing.
pub struct AudioRole {
    was: bool,
    _not_send: PhantomData<*const ()>,
}

impl AudioRole {
    pub fn enter() -> AudioRole {
        AudioRole { was: AUDIO.with(|a| a.replace(true)), _not_send: PhantomData }
    }
}

impl Drop for AudioRole {
    fn drop(&mut self) {
        AUDIO.with(|a| a.set(self.was));
    }
}

/// Every plugin file this process has initialised and not deinitialised.
/// A file's `init` and `deinit` both run with this held, so they never
/// overlap; and one is let go only when nothing else holds it, which no
/// one can start to without this lock.
static FILES: Mutex<Vec<Arc<Dso>>> = Mutex::new(Vec::new());

/// Deinitialises the files nothing uses: on the thread of the host or
/// instance that let go of one last.
fn let_go_of_unused_files() {
    let mut files = FILES.lock().unwrap_or_else(|e| e.into_inner());
    // An instance's hold on a file is taken with the lock held, so one held
    // here by nothing else stays that way; it is dropped with the lock held,
    // so it cannot be opened again while it is deinitialised.
    files.retain(|d| Arc::strong_count(d) > 1);
}

/// What a plugin is asked to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Played from notes: needs a note input and a mono or stereo main
    /// output; any audio input is fed silence.
    Instrument,
    /// Fed audio: needs a mono or stereo main input and output.
    Effect,
}

/// How an instance will be run, for plugins with `clap.render`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// In time with an audio device.
    Realtime,
    /// As fast as it goes, rendering a file: a plugin may use costlier
    /// algorithms, and one that needs real time is refused.
    Offline,
}

/// Something a plugin asked for that its owner has to do, from
/// [`PluginHost::poll`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// The instance with this token wants deactivating and activating
    /// again ([`Instance::restart`]).
    Restart(u64),
    /// The instance with this token could not start processing, or its
    /// processing failed, and it stays silent (or passes its input) until
    /// restarted.
    Failed(u64),
}

/// The state a plugin's host callbacks reach: one per instance, at a fixed
/// address from before `create_plugin` until after `destroy`.
#[repr(C)]
struct HostData {
    // First, so the `*const ClapHost` a plugin passes back is this.
    clap: ClapHost,
    token: u64,
    /// The thread its host is on: CLAP's main thread for this instance.
    owner: u64,
    /// The instance, set once it is initialised and cleared before it is
    /// destroyed, for `on_main_thread`.
    plugin: AtomicPtr<ClapPlugin>,
    callback: AtomicBool,
    restart: AtomicBool,
    process: AtomicBool,
    failed: AtomicBool,
    /// Its instance was dropped off its host's thread and leaked: the host
    /// stops serving it.
    abandoned: AtomicBool,
}

// SAFETY: the pointers in `clap` are to static strings and functions;
// everything else is atomic or never written after construction.
unsafe impl Send for HostData {}
// SAFETY: as above.
unsafe impl Sync for HostData {}

const HOST_VERSION: &CStr = match CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes()) {
    Ok(v) => v,
    Err(_) => panic!("the package version has a NUL in it"),
};

static THREAD_CHECK: ClapHostThreadCheck =
    ClapHostThreadCheck { is_main_thread: Some(host_is_main_thread), is_audio_thread: Some(host_is_audio_thread) };

/// The `HostData` behind a plugin's `host` pointer.
///
/// # Safety
/// `host` must be null or a pointer a plugin was given by this module.
unsafe fn host_data<'a>(host: *const ClapHost) -> Option<&'a HostData> {
    // SAFETY: `clap` is the first field of a `#[repr(C)]` `HostData`, and
    // the pointer was made from the whole `HostData` (`Arc::as_ptr`), which
    // outlives the plugin.
    unsafe { host.cast::<HostData>().as_ref() }
}

unsafe extern "C" fn host_get_extension(_host: *const ClapHost, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return null();
    }
    // SAFETY: a plugin passes a NUL-terminated id.
    if unsafe { CStr::from_ptr(id) } == CLAP_EXT_THREAD_CHECK { (&raw const THREAD_CHECK).cast() } else { null() }
}

unsafe extern "C" fn host_request_restart(host: *const ClapHost) {
    // SAFETY: the pointer this instance was given.
    if let Some(h) = unsafe { host_data(host) } {
        h.restart.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_request_process(host: *const ClapHost) {
    // SAFETY: as above.
    if let Some(h) = unsafe { host_data(host) } {
        h.process.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_request_callback(host: *const ClapHost) {
    // SAFETY: as above.
    if let Some(h) = unsafe { host_data(host) } {
        h.callback.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_is_main_thread(host: *const ClapHost) -> bool {
    // SAFETY: the pointer this instance was given.
    unsafe { host_data(host) }.is_some_and(|h| h.owner == thread_key())
}

unsafe extern "C" fn host_is_audio_thread(_host: *const ClapHost) -> bool {
    is_audio_thread()
}

/// Loads plugins and services their requests, on the thread that creates
/// it, which becomes CLAP's main thread for them.
pub struct PluginHost {
    live: Vec<Weak<HostData>>,
    next_token: u64,
    owner: u64,
    // Plugins' main-thread functions are called from here: never moved to
    // another thread.
    _main: PhantomData<*const ()>,
}

impl PluginHost {
    #[allow(clippy::new_without_default)]
    pub fn new() -> PluginHost {
        PluginHost { live: Vec::new(), next_token: 1, owner: thread_key(), _main: PhantomData }
    }

    /// Creates, initialises and activates the plugin `id` from the file at
    /// `path`, checking that it can be a `role`, for processing at
    /// `sample_rate` in calls of 1 to `max_frames` frames (at most
    /// [`MAX_FRAMES`]).
    ///
    /// # Safety
    /// The file's code runs in this process, and is trusted by this call
    /// (see [`Dso::open`]); no plugin file may be loaded in this process
    /// other than through a `PluginHost` or a scanner child. The host must
    /// be on the thread CLAP calls main, which on Windows and macOS is the
    /// program's main thread (the one its windows and timers would use).
    pub unsafe fn create(
        &mut self,
        path: &Path,
        id: &str,
        role: Role,
        mode: Mode,
        sample_rate: f64,
        max_frames: usize,
    ) -> Result<Instance, String> {
        // SAFETY: passed on from the caller.
        let made = unsafe { self.make(path, id, role, mode, sample_rate, max_frames) };
        if made.is_err() {
            // A file loaded for an instance that came to nothing.
            let_go_of_unused_files();
        }
        made
    }

    /// # Safety
    /// As [`PluginHost::create`].
    unsafe fn make(
        &mut self,
        path: &Path,
        id: &str,
        role: Role,
        mode: Mode,
        sample_rate: f64,
        max_frames: usize,
    ) -> Result<Instance, String> {
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            return Err(format!("cannot run at {sample_rate} Hz"));
        }
        let c_id = CString::new(id).map_err(|_| "has an id with a NUL in it".to_string())?;
        let dso = {
            let mut files = FILES.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: `files` is every file loaded through here, and the
            // caller trusts the file.
            let dso = unsafe { Dso::open_among(path, &files) }?;
            if !files.iter().any(|d| Arc::ptr_eq(d, &dso)) {
                files.push(dso.clone());
            }
            dso
        };
        let factory = dso.factory()?;
        // SAFETY: a factory the file returned, valid while `dso` lives.
        let Some(create_plugin) = (unsafe { (*factory).create_plugin }) else {
            return Err("has an incomplete plugin factory".into());
        };
        let token = self.next_token;
        self.next_token += 1;
        let host = Arc::new(HostData {
            clap: ClapHost {
                clap_version: CLAP_VERSION,
                host_data: null_mut(),
                name: c"TINVO".as_ptr(),
                vendor: c"A.C.B".as_ptr(),
                url: c"https://github.com/IACBI/tinvo".as_ptr(),
                version: HOST_VERSION.as_ptr(),
                get_extension: Some(host_get_extension),
                request_restart: Some(host_request_restart),
                request_process: Some(host_request_process),
                request_callback: Some(host_request_callback),
            },
            token,
            owner: self.owner,
            plugin: AtomicPtr::new(null_mut()),
            callback: AtomicBool::new(false),
            restart: AtomicBool::new(false),
            process: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
        });
        // SAFETY: the factory's own function, given a host that lives until
        // after `destroy` (the instance holds it) and a NUL-terminated id.
        let plugin = unsafe { create_plugin(factory, Arc::as_ptr(&host).cast(), c_id.as_ptr()) };
        if plugin.is_null() {
            return Err(format!("has no plugin '{id}' (or could not create it)"));
        }
        // SAFETY: a plugin the factory returned, valid until destroyed.
        let fns = unsafe { Fns::of(plugin) };
        let Some(fns) = fns else {
            // SAFETY: as above; `destroy` is the one function needed to
            // let go of it.
            match unsafe { (*plugin).destroy } {
                Some(destroy) => unsafe { destroy(plugin) },
                None => {
                    // Leaked, with what it may still use: its host data and
                    // its file, which then stays loaded.
                    std::mem::forget(host);
                    std::mem::forget(dso);
                }
            }
            return Err("is an incomplete plugin".into());
        };
        // SAFETY: init, first and once, on the main thread.
        if !unsafe { (fns.init)(plugin) } {
            // SAFETY: a plugin whose init failed is destroyed, as CLAP asks.
            unsafe { (fns.destroy)(plugin) };
            return Err("refused to start (its init failed)".into());
        }
        host.plugin.store(plugin.cast_mut(), Ordering::Release);
        self.live.push(Arc::downgrade(&host));
        // From here on dropping `instance` deactivates and destroys it.
        let mut instance = Instance {
            plugin,
            fns,
            role,
            layout: Layout::default(),
            buffers: Buffers::default(),
            events: Vec::with_capacity(MAX_EVENTS),
            held: Vec::with_capacity(MAX_EVENTS),
            next_note: 0,
            steady: 0,
            sample_rate,
            max_frames: max_frames.clamp(1, MAX_FRAMES),
            activated: false,
            started: false,
            failed: false,
            host: ManuallyDrop::new(host),
            dso: ManuallyDrop::new(dso),
        };
        if mode == Mode::Offline {
            instance.go_offline()?;
        }
        instance.activate()?;
        Ok(instance)
    }

    /// Services what plugins asked for since the last call: calls
    /// `on_main_thread` for those that asked, and returns what their owners
    /// have to do. Lets go of plugin files no instance uses. Called often
    /// (CLAP suggests within about 30 ms of a request).
    pub fn poll(&mut self) -> Vec<Request> {
        let mut requests = Vec::new();
        self.live.retain(|w| {
            let Some(h) = w.upgrade().filter(|h| !h.abandoned.load(Ordering::Acquire)) else { return false };
            // Taken before the call, so a request made during it is kept for
            // the next poll.
            if h.callback.swap(false, Ordering::AcqRel) {
                let plugin = h.plugin.load(Ordering::Acquire);
                if !plugin.is_null() {
                    // SAFETY: a live, initialised instance (cleared before
                    // `destroy`, which runs on this thread too); CLAP lets
                    // `on_main_thread` run while the audio thread processes.
                    if let Some(f) = unsafe { (*plugin).on_main_thread } {
                        unsafe { f(plugin) };
                    }
                }
            }
            if h.restart.swap(false, Ordering::AcqRel) {
                requests.push(Request::Restart(h.token));
            }
            if h.failed.swap(false, Ordering::AcqRel) {
                requests.push(Request::Failed(h.token));
            }
            true
        });
        let_go_of_unused_files();
        requests
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let_go_of_unused_files();
    }
}

/// A plugin's functions, checked present once.
#[derive(Clone, Copy)]
struct Fns {
    init: unsafe extern "C" fn(*const ClapPlugin) -> bool,
    destroy: unsafe extern "C" fn(*const ClapPlugin),
    activate: unsafe extern "C" fn(*const ClapPlugin, f64, u32, u32) -> bool,
    deactivate: unsafe extern "C" fn(*const ClapPlugin),
    reset: unsafe extern "C" fn(*const ClapPlugin),
    start_processing: unsafe extern "C" fn(*const ClapPlugin) -> bool,
    stop_processing: unsafe extern "C" fn(*const ClapPlugin),
    process: unsafe extern "C" fn(*const ClapPlugin, *const ClapProcess) -> ClapProcessStatus,
    get_extension: unsafe extern "C" fn(*const ClapPlugin, *const c_char) -> *const c_void,
}

impl Fns {
    /// # Safety
    /// `p` must point at a live `clap_plugin_t`.
    unsafe fn of(p: *const ClapPlugin) -> Option<Fns> {
        // SAFETY: by the caller's promise.
        let p = unsafe { &*p };
        // Called through the plugin itself, by `PluginHost::poll`.
        p.on_main_thread?;
        Some(Fns {
            init: p.init?,
            destroy: p.destroy?,
            activate: p.activate?,
            deactivate: p.deactivate?,
            reset: p.reset?,
            start_processing: p.start_processing?,
            stop_processing: p.stop_processing?,
            process: p.process?,
            get_extension: p.get_extension?,
        })
    }
}

/// The audio ports an instance has, as TINVO uses them: every port gets
/// buffers; the main input (port 0) is fed (or silent for an instrument),
/// the main output (port 0) is heard, the rest are silent or unheard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// Channels of each input port, then of each output port.
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
    /// The note input notes go to, and whether it takes CLAP's own note
    /// events (otherwise MIDI 1).
    pub notes: Option<(u16, bool)>,
}

/// One event in an input list: every kind starts with its header.
#[repr(C)]
#[derive(Clone, Copy)]
union Event {
    header: ClapEventHeader,
    note: ClapEventNote,
    midi: ClapEventMidi,
}

/// A note an instance holds, with what releases it.
#[derive(Clone, Copy, Debug)]
struct Held {
    id: i32,
    key: u8,
}

/// A note played on an instance, for releasing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteId(i32);

/// An instance's audio buffers: one plane of `max_frames` per channel of
/// every port, inputs first; reached only through `planes` once made, as
/// the plugin holds pointers into them.
#[derive(Default)]
struct Buffers {
    // Never resized or borrowed: `planes` point into it (a `Vec`'s buffer,
    // unlike a `Box`'s, stays valid for them when the `Vec` moves).
    _store: Vec<f32>,
    planes: Vec<*mut f32>,
    inputs: Vec<ClapAudioBuffer>,
    outputs: Vec<ClapAudioBuffer>,
    /// Planes before the first output's.
    input_planes: usize,
}

impl Buffers {
    fn new(layout: &Layout, max_frames: usize) -> Result<Buffers, String> {
        let input_planes: usize = layout.inputs.iter().map(|&c| c as usize).sum();
        let output_planes: usize = layout.outputs.iter().map(|&c| c as usize).sum();
        let room = || format!("has more audio channels than there is memory for at {max_frames} frames");
        let len = (input_planes + output_planes).checked_mul(max_frames).ok_or_else(room)?;
        let mut store = Vec::new();
        store.try_reserve_exact(len).map_err(|_| room())?;
        store.resize(len, 0.0f32);
        let base = store.as_mut_ptr();
        // SAFETY: each offset is within `store`, plane by plane.
        let mut planes: Vec<*mut f32> =
            (0..input_planes + output_planes).map(|i| unsafe { base.add(i * max_frames) }).collect();
        let first = planes.as_mut_ptr();
        let mut next = 0;
        let mut port = |channels: u32| {
            let buffer = ClapAudioBuffer {
                // SAFETY: within `planes`, which is never resized.
                data32: unsafe { first.add(next) },
                data64: null_mut(),
                channel_count: channels,
                latency: 0,
                constant_mask: 0,
            };
            next += channels as usize;
            buffer
        };
        let inputs = layout.inputs.iter().map(|&c| port(c)).collect();
        let outputs = layout.outputs.iter().map(|&c| port(c)).collect();
        Ok(Buffers { _store: store, planes, inputs, outputs, input_planes })
    }

    /// Plane `i`'s first `frames` samples.
    ///
    /// # Safety
    /// `i` is a plane, `frames` at most the `max_frames` it was made with,
    /// and the plugin is not running.
    #[allow(clippy::mut_from_ref)]
    unsafe fn plane(&self, i: usize, frames: usize) -> &mut [f32] {
        // SAFETY: by the caller's promise, a plane nothing else is using.
        unsafe { std::slice::from_raw_parts_mut(self.planes[i], frames) }
    }
}

/// A plugin instance: created and activated by a [`PluginHost`], processed
/// on CLAP's audio thread, deactivated and destroyed when dropped on its
/// host's thread.
pub struct Instance {
    plugin: *const ClapPlugin,
    fns: Fns,
    role: Role,
    layout: Layout,
    buffers: Buffers,
    events: Vec<Event>,
    held: Vec<Held>,
    next_note: i32,
    steady: i64,
    sample_rate: f64,
    max_frames: usize,
    activated: bool,
    started: bool,
    /// It could not start processing, or its processing failed: it is not
    /// called again until restarted.
    failed: bool,
    // Both outlive the plugin: let go of in `drop` after `destroy`.
    host: ManuallyDrop<Arc<HostData>>,
    dso: ManuallyDrop<Arc<Dso>>,
}

// SAFETY: an instance is used by one thread at a time (`&mut self` for
// everything that calls the plugin, but `extension`, which CLAP marks
// thread-safe); the pointers in it are to the plugin and to buffers it
// owns. Which thread may do what is kept as the module's docs say.
unsafe impl Send for Instance {}

impl Instance {
    /// The token its requests carry ([`PluginHost::poll`]).
    pub fn token(&self) -> u64 {
        self.host.token
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Whether it is activated and has not failed: whether
    /// [`Instance::process`] reaches the plugin.
    pub fn is_playing(&self) -> bool {
        self.activated && !self.failed
    }

    /// Whether this is the thread its host is on.
    fn on_host_thread(&self) -> bool {
        self.host.owner == thread_key()
    }

    /// The plugin itself, for calling its extensions; valid until the
    /// instance is dropped.
    pub fn as_ptr(&self) -> *const ClapPlugin {
        self.plugin
    }

    /// The plugin's extension `id`, or null.
    pub fn extension(&self, id: &CStr) -> *const c_void {
        // SAFETY: get_extension is thread-safe, on an initialised plugin.
        unsafe { (self.fns.get_extension)(self.plugin, id.as_ptr()) }
    }

    /// Whether the plugin asked to be processed since the last call (a
    /// bypassed or resting instance that is not processed should be).
    pub fn take_process_request(&self) -> bool {
        self.host.process.swap(false, Ordering::AcqRel)
    }

    /// Tells a plugin with `clap.render` that it renders offline.
    fn go_offline(&mut self) -> Result<(), String> {
        let ext = self.extension(CLAP_EXT_RENDER).cast::<ClapPluginRender>();
        if ext.is_null() {
            return Ok(());
        }
        // SAFETY: the plugin's own extension, valid until it is destroyed.
        let (hard, set) = unsafe { ((*ext).has_hard_realtime_requirement, (*ext).set) };
        // SAFETY: main-thread functions of a deactivated plugin.
        if hard.is_some_and(|f| unsafe { f(self.plugin) }) {
            return Err("can only run in real time, not render a file".into());
        }
        match set {
            Some(f) if unsafe { f(self.plugin, CLAP_RENDER_OFFLINE) } => Ok(()),
            _ => Err("refused to render offline".into()),
        }
    }

    /// Reads its ports, sizes its buffers and activates it.
    fn activate(&mut self) -> Result<(), String> {
        debug_assert!(!self.activated);
        self.layout = self.read_layout()?;
        self.buffers = Buffers::new(&self.layout, self.max_frames)?;
        // SAFETY: main thread, deactivated, with frame counts in CLAP's
        // range ([1, i32::MAX]; at most MAX_FRAMES, clamped in `make`).
        if !unsafe { (self.fns.activate)(self.plugin, self.sample_rate, 1, self.max_frames as u32) } {
            return Err(format!("could not start at {} Hz", self.sample_rate));
        }
        self.activated = true;
        self.failed = false;
        Ok(())
    }

    /// Its audio and note ports, checked against its role.
    fn read_layout(&self) -> Result<Layout, String> {
        let audio = self.extension(CLAP_EXT_AUDIO_PORTS).cast::<ClapPluginAudioPorts>();
        // Each port's channels, and whether port 0 says it is the main one.
        let read = |is_input: bool| -> Result<(Vec<u32>, bool), String> {
            if audio.is_null() {
                return Ok((Vec::new(), false));
            }
            // SAFETY: the plugin's own extension.
            let (Some(count), Some(get)) = (unsafe { ((*audio).count, (*audio).get) }) else {
                return Err("has an incomplete clap.audio-ports".into());
            };
            // SAFETY: main thread, deactivated.
            let n = unsafe { count(self.plugin, is_input) };
            if n > MAX_PORTS {
                return Err(format!("has more than {MAX_PORTS} audio ports one way"));
            }
            let mut ports = Vec::new();
            let mut main = false;
            for i in 0..n {
                let mut info = ClapAudioPortInfo {
                    id: CLAP_INVALID_ID,
                    name: [0; CLAP_NAME_SIZE],
                    flags: 0,
                    channel_count: 0,
                    port_type: null(),
                    in_place_pair: CLAP_INVALID_ID,
                };
                // SAFETY: an index below the count, and room for the info.
                if !unsafe { get(self.plugin, i, is_input, &mut info) } {
                    return Err("could not describe its audio ports".into());
                }
                if info.channel_count > MAX_PORT_CHANNELS {
                    return Err(format!("has an audio port of more than {MAX_PORT_CHANNELS} channels"));
                }
                main |= i == 0 && info.flags & CLAP_AUDIO_PORT_IS_MAIN != 0;
                ports.push(info.channel_count);
            }
            Ok((ports, main))
        };
        let (inputs, main_in) = read(true)?;
        let (outputs, main_out) = read(false)?;
        // A main port is port 0, flagged so; any other port 0 is an
        // auxiliary one, not to be heard (or fed) as the main.
        if !(main_out && matches!(outputs.first(), Some(1 | 2))) {
            return Err("has no mono or stereo main output".into());
        }
        if self.role == Role::Effect && !(main_in && matches!(inputs.first(), Some(1 | 2))) {
            return Err("has no mono or stereo main input".into());
        }
        let notes = match self.role {
            Role::Effect => None,
            Role::Instrument => Some(self.read_note_input()?.ok_or("takes no notes")?),
        };
        Ok(Layout { inputs, outputs, notes })
    }

    /// The first note input taking CLAP's notes or MIDI 1, and which.
    fn read_note_input(&self) -> Result<Option<(u16, bool)>, String> {
        let ext = self.extension(CLAP_EXT_NOTE_PORTS).cast::<ClapPluginNotePorts>();
        if ext.is_null() {
            return Ok(None);
        }
        // SAFETY: the plugin's own extension.
        let (Some(count), Some(get)) = (unsafe { ((*ext).count, (*ext).get) }) else {
            return Err("has an incomplete clap.note-ports".into());
        };
        // SAFETY: main thread, deactivated.
        let n = unsafe { count(self.plugin, true) }.min(MAX_PORTS);
        for i in 0..n {
            let mut info = ClapNotePortInfo {
                id: CLAP_INVALID_ID,
                supported_dialects: 0,
                preferred_dialect: 0,
                name: [0; CLAP_NAME_SIZE],
            };
            // SAFETY: an index below the count, and room for the info.
            if !unsafe { get(self.plugin, i, true, &mut info) } {
                return Err("could not describe its note ports".into());
            }
            if info.supported_dialects & CLAP_NOTE_DIALECT_CLAP != 0 {
                return Ok(Some((i as u16, true)));
            }
            if info.supported_dialects & CLAP_NOTE_DIALECT_MIDI != 0 {
                return Ok(Some((i as u16, false)));
            }
        }
        Ok(None)
    }

    /// Deactivates and activates it again, as a plugin's `request_restart`
    /// asks, and after a failure: on its host's thread, with the instance
    /// back from the audio thread. Its voices are reset (CLAP's `reset`),
    /// the notes it held forgotten and its ports read again. On an error it
    /// stays deactivated, and silent.
    pub fn restart(&mut self) -> Result<(), String> {
        if !self.on_host_thread() {
            return Err("can only be restarted on its host's thread".into());
        }
        self.stop();
        self.events.clear();
        self.held.clear();
        if self.activated {
            {
                let _role = AudioRole::enter();
                // SAFETY: audio thread (this one plays it, as nothing else
                // can hold the instance), active.
                unsafe { (self.fns.reset)(self.plugin) };
            }
            // SAFETY: main thread, active and not processing.
            unsafe { (self.fns.deactivate)(self.plugin) };
            self.activated = false;
        }
        self.activate()
    }

    /// Plays note `key` (0 to 127) at `velocity` (0 to 1), from the start
    /// of the next process call; `None` if its event list has no room left
    /// for the note and its release.
    pub fn note_on(&mut self, key: u8, velocity: f32) -> Option<NoteId> {
        let (_, clap) = self.layout.notes?;
        if !velocity.is_finite() {
            return None;
        }
        let key = key.min(127);
        if self.events.len() + self.held.len() + 2 > MAX_EVENTS {
            return None;
        }
        if !clap {
            // MIDI 1 has one note per key: one still held ends here, and its
            // own release is then dropped.
            if let Some(i) = self.held.iter().position(|h| h.key == key) {
                let old = self.held.swap_remove(i);
                self.push_note(CLAP_EVENT_NOTE_OFF, old, 0.0);
            }
        }
        let id = self.next_note;
        self.next_note = if id == i32::MAX { 0 } else { id + 1 };
        let held = Held { id, key };
        self.push_note(CLAP_EVENT_NOTE_ON, held, velocity);
        self.held.push(held);
        Some(NoteId(id))
    }

    /// Releases a note from [`Instance::note_on`], from the start of the
    /// next process call; one already released (or from before a restart)
    /// is passed over.
    pub fn note_off(&mut self, note: NoteId) {
        if let Some(i) = self.held.iter().position(|h| h.id == note.0) {
            let held = self.held.swap_remove(i);
            self.push_note(CLAP_EVENT_NOTE_OFF, held, 0.0);
        }
    }

    /// Releases every note it holds.
    pub fn release_all(&mut self) {
        while let Some(held) = self.held.pop() {
            self.push_note(CLAP_EVENT_NOTE_OFF, held, 0.0);
        }
    }

    /// Queues a note event, at the start of the next process call. Room is
    /// there: a note-on checked for it, and a release uses its note's.
    fn push_note(&mut self, kind: u16, held: Held, velocity: f32) {
        let Some((port, clap)) = self.layout.notes else { return };
        debug_assert!(self.events.len() < MAX_EVENTS);
        let event = if clap {
            Event {
                note: ClapEventNote {
                    header: ClapEventHeader {
                        size: size_of::<ClapEventNote>() as u32,
                        time: 0,
                        space_id: CLAP_CORE_EVENT_SPACE_ID,
                        kind,
                        flags: 0,
                    },
                    note_id: held.id,
                    port_index: port as i16,
                    channel: 0,
                    key: i16::from(held.key),
                    velocity: f64::from(velocity.clamp(0.0, 1.0)),
                },
            }
        } else {
            let (status, value) = if kind == CLAP_EVENT_NOTE_ON {
                (0x90, (velocity.clamp(0.0, 1.0) * 127.0).round().max(1.0) as u8)
            } else {
                (0x80, 0)
            };
            Event {
                midi: ClapEventMidi {
                    header: ClapEventHeader {
                        size: size_of::<ClapEventMidi>() as u32,
                        time: 0,
                        space_id: CLAP_CORE_EVENT_SPACE_ID,
                        kind: CLAP_EVENT_MIDI,
                        flags: 0,
                    },
                    port_index: port,
                    data: [status, held.key, value],
                },
            }
        };
        if self.events.len() < MAX_EVENTS {
            self.events.push(event);
        }
    }

    /// Processes `io`, interleaved stereo frames: an effect's input,
    /// replaced by its output; an instrument's output, written over it. On
    /// the thread playing CLAP's audio thread (taken here). Starts
    /// processing the first time. Returns whether the plugin processed all
    /// of it. Where it did not (not activated, failed to start, or an error
    /// from the plugin, from which it is not called again until restarted
    /// and which [`PluginHost::poll`] reports) an effect leaves `io` as it
    /// was and an instrument silences it; a call split at `max_frames`
    /// keeps what the parts before an error made. Never allocates.
    pub fn process(&mut self, io: &mut [f32]) -> bool {
        let frames = io.len() / 2;
        let _role = AudioRole::enter();
        if self.activated && !self.failed && !self.started {
            // SAFETY: audio thread, active and not processing.
            if unsafe { (self.fns.start_processing)(self.plugin) } {
                self.started = true;
            } else {
                self.give_up();
            }
        }
        let mut done = 0;
        while done < frames && self.activated && !self.failed {
            let n = (frames - done).min(self.max_frames);
            if self.process_chunk(&mut io[2 * done..2 * (done + n)]) {
                done += n;
            } else {
                self.give_up();
            }
        }
        if done < frames {
            // Events not taken stay queued: a restart clears them with the
            // notes they belong to.
            self.advance(frames - done);
            if self.role == Role::Instrument {
                io[2 * done..].fill(0.0);
            }
        }
        done == frames
    }

    /// Stops calling the plugin until it is restarted, and says so.
    fn give_up(&mut self) {
        self.failed = true;
        self.host.failed.store(true, Ordering::Release);
    }

    /// One process call of at most `max_frames` frames; on an error from
    /// the plugin `io` is left as it was.
    fn process_chunk(&mut self, io: &mut [f32]) -> bool {
        let frames = io.len() / 2;
        // A plugin may write the masks; each call starts them clear.
        for buffer in self.buffers.inputs.iter_mut().chain(self.buffers.outputs.iter_mut()) {
            buffer.constant_mask = 0;
        }
        let outputs = self.buffers.outputs.as_mut_ptr();
        let b = &self.buffers;
        let fed = if self.role == Role::Effect { self.layout.inputs[0] } else { 0 };
        // SAFETY: planes of these buffers, `frames` within their size, and
        // the plugin is not running.
        unsafe {
            match fed {
                2 => {
                    let (l, r) = (b.plane(0, frames), b.plane(1, frames));
                    for (i, f) in io.chunks_exact(2).enumerate() {
                        l[i] = f[0];
                        r[i] = f[1];
                    }
                }
                1 => {
                    let m = b.plane(0, frames);
                    for (i, f) in io.chunks_exact(2).enumerate() {
                        m[i] = 0.5 * (f[0] + f[1]);
                    }
                }
                _ => {}
            }
            for p in fed as usize..b.input_planes {
                b.plane(p, frames).fill(0.0);
            }
            for p in b.input_planes..b.planes.len() {
                b.plane(p, frames).fill(0.0);
            }
        }
        let in_events = ClapInputEvents {
            ctx: (&raw const self.events).cast_mut().cast(),
            size: Some(events_size),
            get: Some(events_get),
        };
        let out_events = ClapOutputEvents { ctx: null_mut(), try_push: Some(events_discard) };
        let process = ClapProcess {
            steady_time: self.steady,
            frames_count: frames as u32,
            transport: null(),
            audio_inputs: b.inputs.as_ptr(),
            audio_outputs: outputs,
            audio_inputs_count: b.inputs.len() as u32,
            audio_outputs_count: b.outputs.len() as u32,
            in_events: &in_events,
            out_events: &out_events,
        };
        // SAFETY: audio thread, processing; every pointer in `process` is
        // valid until it returns, and its counts are the plugin's own.
        let status = unsafe { (self.fns.process)(self.plugin, &process) };
        if status == CLAP_PROCESS_ERROR {
            // Its events stay queued for whatever follows: a restart clears
            // them, with the notes they belong to.
            return false;
        }
        self.events.clear();
        let out = b.input_planes;
        // SAFETY: as above, after the plugin returned.
        unsafe {
            let clean = |x: f32| if x.is_finite() { x } else { 0.0 };
            if self.layout.outputs[0] == 2 {
                let (l, r) = (b.plane(out, frames), b.plane(out + 1, frames));
                for (i, f) in io.chunks_exact_mut(2).enumerate() {
                    f[0] = clean(l[i]);
                    f[1] = clean(r[i]);
                }
            } else {
                let m = b.plane(out, frames);
                for (i, f) in io.chunks_exact_mut(2).enumerate() {
                    let x = clean(m[i]);
                    f[0] = x;
                    f[1] = x;
                }
            }
        }
        self.advance(frames);
        true
    }

    /// Moves its steady time on by `frames` not processed (CLAP's steady
    /// time counts every frame since it started).
    pub fn skip(&mut self, frames: usize) {
        self.advance(frames);
    }

    /// Steady time, `frames` on; past what it can count, unknown (-1) for
    /// good, as CLAP allows.
    fn advance(&mut self, frames: usize) {
        if self.steady >= 0 {
            self.steady = i64::try_from(frames).ok().and_then(|f| self.steady.checked_add(f)).unwrap_or(-1);
        }
    }

    /// Events waiting for the next process call.
    pub fn queued(&self) -> usize {
        self.events.len()
    }

    /// Stops processing, if it was: before it leaves the audio thread for
    /// good. On the thread playing CLAP's audio thread (taken here).
    pub fn stop(&mut self) {
        if self.started {
            let _role = AudioRole::enter();
            // SAFETY: audio thread, active and processing.
            unsafe { (self.fns.stop_processing)(self.plugin) };
            self.started = false;
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if !self.on_host_thread() {
            // Destroying it here would break CLAP's threading rules, and
            // letting go of its file could deinitialise the file under it:
            // both are leaked instead, and its host stops serving it (a
            // poll already under way may serve it once more, which the
            // leak keeps safe).
            self.host.abandoned.store(true, Ordering::Release);
            return;
        }
        // An instance the audio thread left processing (its stream ended
        // without stopping it) is stopped here, playing the audio thread.
        self.stop();
        // SAFETY: main thread; deactivated if it was active, then destroyed
        // once; the host data and the file outlive it.
        unsafe {
            if self.activated {
                (self.fns.deactivate)(self.plugin);
            }
            self.host.plugin.store(null_mut(), Ordering::Release);
            (self.fns.destroy)(self.plugin);
            ManuallyDrop::drop(&mut self.host);
            ManuallyDrop::drop(&mut self.dso);
        }
        let_go_of_unused_files();
    }
}

unsafe extern "C" fn events_size(list: *const ClapInputEvents) -> u32 {
    // SAFETY: `ctx` is the instance's event list, alive during `process`.
    unsafe { (*(*list).ctx.cast::<Vec<Event>>()).len() as u32 }
}

unsafe extern "C" fn events_get(list: *const ClapInputEvents, index: u32) -> *const ClapEventHeader {
    // SAFETY: as above.
    let events = unsafe { &*(*list).ctx.cast::<Vec<Event>>() };
    events.get(index as usize).map_or(null(), |e| (e as *const Event).cast())
}

/// Takes the plugin's output events, and lets them go: TINVO reads none
/// yet.
unsafe extern "C" fn events_discard(_list: *const ClapOutputEvents, _event: *const ClapEventHeader) -> bool {
    true
}
