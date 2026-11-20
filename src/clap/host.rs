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
//!   refuses another thread, and an instance dropped on another thread
//!   stops processing there and goes home to its host, whose next poll (or
//!   drop) destroys it;
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
//!   storage may allocate;
//! - the main-thread calls CLAP allows while the plugin processes (reading
//!   a parameter's value or text, saving its state) go through a
//!   [`PluginHandle`], which never touches the [`Instance`] the audio
//!   thread holds: it reaches the plugin through its host data, works only
//!   on its host's thread, and only while the plugin lives (its pointer is
//!   cleared there before `destroy`);
//! - parameter changes reach the plugin in its next process call, or in a
//!   `flush` on the audio thread when it is not processed meanwhile.
//!
//! Processing never allocates: the buffers and event lists are sized when
//! an instance is activated.

use super::PluginRef;
use super::ffi::*;
use super::library::Dso;
use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// The most parameters read of a plugin; any beyond are left out.
pub const MAX_PARAMS: usize = 4096;
/// Parameter changes an instance can be given between two process calls,
/// one a parameter: a change to one already waiting replaces it. Kept apart
/// from [`MAX_EVENTS`], so they never take a note's room.
pub const MAX_PARAM_EVENTS: usize = 256;
/// The largest state saved or loaded, in bytes.
pub const MAX_STATE: usize = 16 << 20;
/// The latest a plugin's output may come, in seconds: the engine makes up
/// for this much on any one path.
pub const MAX_LATENCY_SECONDS: f64 = 0.5;

/// [`MAX_LATENCY_SECONDS`] in frames at `sample_rate`.
pub fn max_latency(sample_rate: f64) -> usize {
    (MAX_LATENCY_SECONDS * sample_rate) as usize
}

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

/// The next instance's token: unique in the process, whichever host made it.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

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
    /// The parameters of the instance with this token changed as CLAP's
    /// rescan flags say: their values (as after loading a preset, or a
    /// change the plugin made itself), their text, their info (names and
    /// whether hidden, already read again for its handles) or all of them
    /// (read again at its next restart).
    Params(u64, u32),
    /// The instance with this token asked for what refers to parameter
    /// `id` to be cleared, as CLAP's clear flags say (all of it: the
    /// parameter is gone, or its id names another one from its next
    /// restart; its automation; its modulation).
    Cleared { token: u64, id: u32, flags: u32 },
    /// The state of the instance with this token changed and should be
    /// saved again: the plugin said so, or changed a parameter itself. The
    /// changes sent to it ([`Instance::set_param`]) are not reported: the
    /// sender knows of them.
    StateDirty(u64),
}

/// One of a plugin's parameters, as `clap.params` describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamInfo {
    /// Its stable id, which automation names it by.
    pub id: u32,
    /// CLAP's `clap_param_info_flags`.
    pub flags: u32,
    pub name: String,
    /// Where it sits in the plugin, `/` between levels; may be empty.
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
}

impl ParamInfo {
    /// Whole values only.
    pub fn is_stepped(&self) -> bool {
        self.flags & (CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_BYPASS | CLAP_PARAM_IS_ENUM) != 0
    }

    /// Not to be shown: not in use now.
    pub fn is_hidden(&self) -> bool {
        self.flags & CLAP_PARAM_IS_HIDDEN != 0
    }

    /// Not to be changed by the host.
    pub fn is_readonly(&self) -> bool {
        self.flags & CLAP_PARAM_IS_READONLY != 0
    }

    /// Automation may play it.
    pub fn is_automatable(&self) -> bool {
        self.flags & CLAP_PARAM_IS_AUTOMATABLE != 0
    }

    /// `value` in its range, a stepped one's truncated as CLAP says (to a
    /// whole number within the range); `None` for a value that is not a
    /// number.
    pub fn clamp(&self, value: f64) -> Option<f64> {
        if !value.is_finite() {
            return None;
        }
        Some(if self.is_stepped() {
            value.trunc().clamp(self.min.ceil(), self.max.floor())
        } else {
            value.clamp(self.min, self.max)
        })
    }

    /// Whether a value can be set: a range, holding a whole number if it
    /// is stepped.
    fn has_values(&self) -> bool {
        let (min, max) = (self.min, self.max);
        min.is_finite() && max.is_finite() && if self.is_stepped() { min.ceil() <= max.floor() } else { min <= max }
    }
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
    /// Its extensions, read once it is initialised.
    exts: OnceLock<Exts>,
    /// Its parameters as last read, for its handles; on the main thread.
    params: Mutex<Arc<[ParamInfo]>>,
    /// CLAP's rescan flags it set since the last poll.
    rescan: AtomicU32,
    /// The parameters it asked to be cleared since the last poll, with
    /// CLAP's clear flags, each id once.
    cleared: Mutex<Vec<(u32, u32)>>,
    /// The parameters it cleared all of: changes waiting for them are
    /// dropped at its next restart, when the ids may name others.
    forget: Mutex<Vec<u32>>,
    /// It asked for its parameter changes to be flushed.
    flush: AtomicBool,
    dirty: AtomicBool,
    /// Every parameter change with a serial up to this one has reached the
    /// plugin, or was dropped with its parameter.
    delivered: AtomicU64,
}

/// The extensions an instance has that the host calls, checked complete
/// once.
#[derive(Clone, Copy, Default)]
struct Exts {
    params: Option<ParamsFns>,
    state: Option<StateFns>,
    latency: Option<unsafe extern "C" fn(*const ClapPlugin) -> u32>,
}

#[derive(Clone, Copy)]
struct ParamsFns {
    count: unsafe extern "C" fn(*const ClapPlugin) -> u32,
    get_info: unsafe extern "C" fn(*const ClapPlugin, u32, *mut ClapParamInfo) -> bool,
    get_value: unsafe extern "C" fn(*const ClapPlugin, ClapId, *mut f64) -> bool,
    value_to_text: Option<unsafe extern "C" fn(*const ClapPlugin, ClapId, f64, *mut c_char, u32) -> bool>,
    text_to_value: Option<unsafe extern "C" fn(*const ClapPlugin, ClapId, *const c_char, *mut f64) -> bool>,
    flush: Option<unsafe extern "C" fn(*const ClapPlugin, *const ClapInputEvents, *const ClapOutputEvents)>,
}

#[derive(Clone, Copy)]
struct StateFns {
    save: unsafe extern "C" fn(*const ClapPlugin, *const ClapOStream) -> bool,
    load: unsafe extern "C" fn(*const ClapPlugin, *const ClapIStream) -> bool,
}

impl Exts {
    /// # Safety
    /// `plugin` is initialised, with `get_extension` its own.
    unsafe fn of(
        plugin: *const ClapPlugin,
        get_extension: unsafe extern "C" fn(*const ClapPlugin, *const c_char) -> *const c_void,
    ) -> Exts {
        // SAFETY: by the caller's promise; each pointer is null or the
        // plugin's own extension, valid until it is destroyed.
        unsafe {
            let params = get_extension(plugin, CLAP_EXT_PARAMS.as_ptr()).cast::<ClapPluginParams>();
            let state = get_extension(plugin, CLAP_EXT_STATE.as_ptr()).cast::<ClapPluginState>();
            let latency = get_extension(plugin, CLAP_EXT_LATENCY.as_ptr()).cast::<ClapPluginLatency>();
            Exts {
                params: params.as_ref().and_then(|p| {
                    Some(ParamsFns {
                        count: p.count?,
                        get_info: p.get_info?,
                        get_value: p.get_value?,
                        value_to_text: p.value_to_text,
                        text_to_value: p.text_to_value,
                        flush: p.flush,
                    })
                }),
                state: state.as_ref().and_then(|s| Some(StateFns { save: s.save?, load: s.load? })),
                latency: latency.as_ref().and_then(|l| l.get),
            }
        }
    }
}

/// Instances dropped off their host's thread, stopped there and waiting for
/// their host to destroy them; `None` once the host is gone.
type Inbox = Mutex<Option<Vec<Instance>>>;

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
static HOST_PARAMS: ClapHostParams = ClapHostParams {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_request_flush),
};
// The latency is read after every activation, so its change needs no note.
static HOST_LATENCY: ClapHostLatency = ClapHostLatency { changed: Some(host_latency_changed) };
static HOST_STATE: ClapHostState = ClapHostState { mark_dirty: Some(host_mark_dirty) };

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
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_THREAD_CHECK {
        (&raw const THREAD_CHECK).cast()
    } else if id == CLAP_EXT_PARAMS {
        (&raw const HOST_PARAMS).cast()
    } else if id == CLAP_EXT_LATENCY {
        (&raw const HOST_LATENCY).cast()
    } else if id == CLAP_EXT_STATE {
        (&raw const HOST_STATE).cast()
    } else {
        null()
    }
}

unsafe extern "C" fn host_params_rescan(host: *const ClapHost, flags: u32) {
    // SAFETY: the pointer this instance was given.
    if let Some(h) = unsafe { host_data(host) } {
        h.rescan.fetch_or(flags, Ordering::AcqRel);
    }
}

unsafe extern "C" fn host_params_clear(host: *const ClapHost, param_id: ClapId, flags: u32) {
    // SAFETY: as above.
    let Some(h) = (unsafe { host_data(host) }) else { return };
    let mut cleared = h.cleared.lock().unwrap_or_else(|e| e.into_inner());
    match cleared.iter().position(|c| c.0 == param_id) {
        Some(i) => cleared[i].1 |= flags,
        None if cleared.len() < MAX_PARAMS => cleared.push((param_id, flags)),
        None => {}
    }
    drop(cleared);
    if flags & CLAP_PARAM_CLEAR_ALL != 0 {
        let mut forget = h.forget.lock().unwrap_or_else(|e| e.into_inner());
        if !forget.contains(&param_id) && forget.len() < MAX_PARAMS {
            forget.push(param_id);
        }
    }
}

unsafe extern "C" fn host_request_flush(host: *const ClapHost) {
    // SAFETY: as above.
    if let Some(h) = unsafe { host_data(host) } {
        h.flush.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_latency_changed(_host: *const ClapHost) {}

unsafe extern "C" fn host_mark_dirty(host: *const ClapHost) {
    // SAFETY: as above.
    if let Some(h) = unsafe { host_data(host) } {
        h.dirty.store(true, Ordering::Release);
    }
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
    inbox: Arc<Inbox>,
    owner: u64,
    // Plugins' main-thread functions are called from here: never moved to
    // another thread.
    _main: PhantomData<*const ()>,
}

impl PluginHost {
    #[allow(clippy::new_without_default)]
    pub fn new() -> PluginHost {
        PluginHost {
            live: Vec::new(),
            inbox: Arc::new(Mutex::new(Some(Vec::new()))),
            owner: thread_key(),
            _main: PhantomData,
        }
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
        unsafe { self.create_with_state(path, id, role, mode, sample_rate, max_frames, None) }
    }

    /// [`PluginHost::create`], loading `state` (from [`PluginHandle::save_state`]
    /// or [`Instance::save_state`]) before it is first activated, so it
    /// starts as it was saved. A state it cannot load fails the call: the
    /// plugin may have taken part of it, so one at its defaults is made
    /// afresh.
    ///
    /// # Safety
    /// As [`PluginHost::create`].
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn create_with_state(
        &mut self,
        path: &Path,
        id: &str,
        role: Role,
        mode: Mode,
        sample_rate: f64,
        max_frames: usize,
        state: Option<&[u8]>,
    ) -> Result<Instance, String> {
        // SAFETY: passed on from the caller.
        let made = unsafe { self.make(path, id, role, mode, sample_rate, max_frames, state) };
        if made.is_err() {
            // A file loaded for an instance that came to nothing.
            let_go_of_unused_files();
        }
        made
    }

    /// # Safety
    /// As [`PluginHost::create`].
    #[allow(clippy::too_many_arguments)]
    unsafe fn make(
        &mut self,
        path: &Path,
        id: &str,
        role: Role,
        mode: Mode,
        sample_rate: f64,
        max_frames: usize,
        state: Option<&[u8]>,
    ) -> Result<Instance, String> {
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            return Err(format!("cannot run at {sample_rate} Hz"));
        }
        let plugin_ref = PluginRef::new(id).ok_or("has an id no plugin can have")?;
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
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
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
            exts: OnceLock::new(),
            params: Mutex::new(Arc::from([])),
            rescan: AtomicU32::new(0),
            cleared: Mutex::new(Vec::new()),
            forget: Mutex::new(Vec::new()),
            flush: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            delivered: AtomicU64::new(0),
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
        // SAFETY: initialised, with its own get_extension.
        let exts = unsafe { Exts::of(plugin, fns.get_extension) };
        let _ = host.exts.set(exts);
        self.live.push(Arc::downgrade(&host));
        // From here on dropping `instance` deactivates and destroys it.
        let mut instance = Instance {
            plugin,
            fns,
            role,
            layout: Layout::default(),
            buffers: Buffers::default(),
            events: Vec::with_capacity(MAX_EVENTS + MAX_PARAM_EVENTS),
            params_queued: 0,
            serials: Vec::with_capacity(MAX_PARAM_EVENTS),
            high: 0,
            exts,
            params: Arc::from([]),
            param_ids: Vec::new(),
            latency: 0,
            dry: Vec::new(),
            dry_at: 0,
            held: Vec::with_capacity(MAX_EVENTS),
            next_note: 0,
            steady: 0,
            sample_rate,
            max_frames: max_frames.clamp(1, MAX_FRAMES),
            activated: false,
            started: false,
            failed: false,
            tail: false,
            plugin_ref,
            inbox: Some(self.inbox.clone()),
            host: ManuallyDrop::new(host),
            dso: ManuallyDrop::new(dso),
        };
        if let Some(state) = state {
            instance.load_state(state)?;
        }
        if mode == Mode::Offline {
            instance.go_offline()?;
        }
        instance.activate()?;
        Ok(instance)
    }

    /// Destroys the instances dropped off this thread since the last call,
    /// then services what plugins asked for: calls `on_main_thread` for
    /// those that asked, and returns what their owners have to do. Lets go
    /// of plugin files no instance uses. Called often (CLAP suggests within
    /// about 30 ms of a request).
    pub fn poll(&mut self) -> Vec<Request> {
        // Taken out under the lock and destroyed after it, so the lock is
        // never held over a plugin's code or the files' lock.
        let home = self.inbox.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(std::mem::take);
        drop(home);
        let mut requests = Vec::new();
        self.live.retain(|w| {
            let Some(h) = w.upgrade() else { return false };
            // A destroyed instance's host data may live on in a handle; what
            // it asked for went with it.
            if h.plugin.load(Ordering::Acquire).is_null() {
                return false;
            }
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
            let rescan = h.rescan.swap(0, Ordering::AcqRel);
            if rescan & CLAP_PARAM_RESCAN_INFO != 0 {
                refresh_info(&h);
            }
            if rescan != 0 {
                requests.push(Request::Params(h.token, rescan));
            }
            let cleared = std::mem::take(&mut *h.cleared.lock().unwrap_or_else(|e| e.into_inner()));
            requests.extend(cleared.into_iter().map(|(id, flags)| Request::Cleared { token: h.token, id, flags }));
            if h.dirty.swap(false, Ordering::AcqRel) {
                requests.push(Request::StateDirty(h.token));
            }
            true
        });
        let_go_of_unused_files();
        requests
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        // Closed, so an instance dropped off this thread from now on is
        // leaked with its file: nothing is left to destroy it.
        let home = self.inbox.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(home);
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
    param: ClapEventParamValue,
}

/// The events of one call, as an input list's context.
struct EventSlice {
    ptr: *const Event,
    len: usize,
}

/// What an output list's context reaches: the plugin's own changes to its
/// parameters are noted for the host.
struct Outputs<'a> {
    host: &'a HostData,
    params: &'a [ParamInfo],
    ids: &'a [(u32, u32)],
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

impl NoteId {
    /// The note's CLAP note id: 0 to `i32::MAX`.
    pub fn get(self) -> i32 {
        self.0
    }

    /// The note with CLAP note id `id`, if it can be one.
    pub fn from_id(id: i32) -> Option<NoteId> {
        (id >= 0).then_some(NoteId(id))
    }
}

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
    /// Parameter changes first (`params_queued` of them), then notes.
    events: Vec<Event>,
    params_queued: usize,
    /// The serial of each parameter change queued, in its order.
    serials: Vec<u64>,
    /// The highest serial given.
    high: u64,
    exts: Exts,
    params: Arc<[ParamInfo]>,
    /// (id, index into `params`), by id.
    param_ids: Vec<(u32, u32)>,
    latency: u32,
    /// An effect's input as late as its output ([`Instance::delay_dry`]):
    /// `latency` stereo frames, and where the next goes.
    dry: Vec<f32>,
    dry_at: usize,
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
    /// Its last process call said it has a tail still to play.
    tail: bool,
    plugin_ref: PluginRef,
    /// Where it goes if it is dropped off its host's thread.
    inbox: Option<Arc<Inbox>>,
    // Both outlive the plugin: let go of in `drop` after `destroy`.
    host: ManuallyDrop<Arc<HostData>>,
    dso: ManuallyDrop<Arc<Dso>>,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Instance({} {:?})", self.plugin_ref.id(), self.token())
    }
}

/// Two instances are equal when they are the same instance.
impl PartialEq for Instance {
    fn eq(&self, other: &Self) -> bool {
        self.token() == other.token()
    }
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

    /// Which plugin it is.
    pub fn plugin(&self) -> PluginRef {
        self.plugin_ref
    }

    /// The sample rate it is activated for.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Its parameters, as read when it was last activated.
    pub fn params(&self) -> &Arc<[ParamInfo]> {
        &self.params
    }

    /// Its parameter `id`.
    pub fn param(&self, id: u32) -> Option<&ParamInfo> {
        let i = self.param_ids.binary_search_by_key(&id, |p| p.0).ok()?;
        self.params.get(self.param_ids[i].1 as usize)
    }

    /// The frames its output comes later than its input, as it said when
    /// it was last activated.
    pub fn latency(&self) -> u32 {
        self.latency
    }

    /// A handle for the main-thread calls CLAP allows while it processes.
    pub fn handle(&self) -> PluginHandle {
        PluginHandle { host: Arc::clone(&self.host), _main: PhantomData }
    }

    /// Saves its state: `None` if it keeps none (no `clap.state`). On its
    /// host's thread.
    pub fn save_state(&self) -> Result<Option<Vec<u8>>, String> {
        if !self.on_host_thread() {
            return Err("can only save its state on its host's thread".into());
        }
        let Some(state) = self.exts.state else { return Ok(None) };
        // SAFETY: main thread, a live plugin and its own extension.
        unsafe { save_state(self.plugin, state) }.map(Some)
    }

    /// Loads a state it saved: before it is activated, as
    /// [`PluginHost::create_with_state`] does.
    fn load_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        let Some(state) = self.exts.state else { return Err("keeps no state, so cannot load one".into()) };
        // SAFETY: main thread (creation), a live plugin and its own
        // extension.
        unsafe { load_state(self.plugin, state, bytes) }
    }

    /// Sets parameter `id` to `value` (in its range, a stepped one's
    /// truncated) from the start of the next process call, or the next
    /// [`Instance::skip`] if it is not processed meanwhile (one that needs
    /// processing, CLAP's `REQUIRES_PROCESS`, waits for a process call).
    /// `serial` is acknowledged once the plugin has it, or it was dropped
    /// with its parameter ([`PluginHandle::delivered`]); serials are to
    /// grow. False, with nothing queued, for an id it lacks, a read-only
    /// one, a value that is not a number, or no room left. Never allocates.
    pub fn set_param(&mut self, id: u32, value: f64, serial: u64) -> bool {
        let Some(info) = self.param(id) else { return false };
        if info.is_readonly() {
            return false;
        }
        let Some(value) = info.clamp(value) else { return false };
        let pending = &mut self.events[..self.params_queued];
        // SAFETY: the first `params_queued` events are parameter changes.
        if let Some(i) = pending.iter().position(|e| unsafe { e.param.param_id } == id) {
            // SAFETY: as above.
            let mut p = unsafe { pending[i].param };
            p.value = value;
            pending[i] = Event { param: p };
            self.serials[i] = serial;
        } else if self.params_queued < MAX_PARAM_EVENTS {
            let param = ClapEventParamValue {
                header: ClapEventHeader {
                    size: size_of::<ClapEventParamValue>() as u32,
                    time: 0,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    kind: CLAP_EVENT_PARAM_VALUE,
                    flags: 0,
                },
                param_id: id,
                cookie: null_mut(),
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value,
            };
            // Within the capacities reserved: no allocation.
            self.events.insert(self.params_queued, Event { param });
            self.serials.push(serial);
            self.params_queued += 1;
        } else {
            return false;
        }
        self.high = self.high.max(serial);
        true
    }

    /// Delays `io` (interleaved stereo) by the frames an effect's output
    /// comes late ([`Instance::latency`]), so its input can be mixed with
    /// its output, or stand in for it, in time. Each call carries on from
    /// the last, from silence when it was activated. Never allocates.
    pub fn delay_dry(&mut self, io: &mut [f32]) {
        let n = self.dry.len() / 2;
        if n == 0 {
            return;
        }
        for f in io.chunks_exact_mut(2) {
            let at = 2 * self.dry_at;
            let late = [self.dry[at], self.dry[at + 1]];
            self.dry[at] = f[0];
            self.dry[at + 1] = f[1];
            f.copy_from_slice(&late);
            self.dry_at = if self.dry_at + 1 == n { 0 } else { self.dry_at + 1 };
        }
    }

    /// Parameter changes waiting for the plugin.
    pub fn params_queued(&self) -> usize {
        self.params_queued
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
        self.read_params();
        self.layout = self.read_layout()?;
        self.buffers = Buffers::new(&self.layout, self.max_frames)?;
        // SAFETY: main thread, deactivated, with frame counts in CLAP's
        // range ([1, i32::MAX]; at most MAX_FRAMES, clamped in `make`).
        if !unsafe { (self.fns.activate)(self.plugin, self.sample_rate, 1, self.max_frames as u32) } {
            return Err(format!("could not start at {} Hz", self.sample_rate));
        }
        self.activated = true;
        self.failed = false;
        // SAFETY: main thread, active.
        let latency = self.exts.latency.map_or(0, |get| unsafe { get(self.plugin) });
        if latency as usize > max_latency(self.sample_rate) {
            // SAFETY: main thread, active and not processing.
            unsafe { (self.fns.deactivate)(self.plugin) };
            self.activated = false;
            let ms = (f64::from(latency) / self.sample_rate * 1000.0).round();
            return Err(format!("comes {ms} ms late, more than the {} ms made up for", MAX_LATENCY_SECONDS * 1000.0));
        }
        self.latency = latency;
        let dry = if self.role == Role::Effect { 2 * latency as usize } else { 0 };
        if self.dry.len() == dry {
            self.dry.fill(0.0);
        } else {
            self.dry = vec![0.0; dry];
        }
        self.dry_at = 0;
        Ok(())
    }

    /// Reads its parameters (main thread, deactivated): at most
    /// [`MAX_PARAMS`], passing over any it cannot describe, with a range
    /// that is not one, or an id already seen.
    fn read_params(&mut self) {
        // SAFETY: main thread, a live plugin and its own extension.
        let (params, ids) = self.exts.params.map(|p| unsafe { read_params(self.plugin, p) }).unwrap_or_default();
        self.params = Arc::from(params);
        self.param_ids = ids;
        *self.host.params.lock().unwrap_or_else(|e| e.into_inner()) = self.params.clone();
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
        // Parameter changes are kept for the plugin as it starts again;
        // the notes go, with the voices they belong to.
        self.events.truncate(self.params_queued);
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
        let activated = self.activate();
        // Those of parameters it no longer has (or cleared, or no longer
        // lets the host set) are dropped, the rest kept to their ranges now.
        let forget = std::mem::take(&mut *self.host.forget.lock().unwrap_or_else(|e| e.into_inner()));
        let mut i = 0;
        while i < self.events.len() {
            // SAFETY: the events left are parameter changes.
            let mut p = unsafe { self.events[i].param };
            let kept = (!forget.contains(&p.param_id))
                .then(|| self.param(p.param_id))
                .flatten()
                .filter(|info| !info.is_readonly())
                .and_then(|info| info.clamp(p.value));
            match kept {
                Some(v) => {
                    p.value = v;
                    self.events[i] = Event { param: p };
                    i += 1;
                }
                None => {
                    self.events.remove(i);
                    self.serials.remove(i);
                }
            }
        }
        self.params_queued = self.events.len();
        settle(&self.serials, self.high, &self.host.delivered);
        activated
    }

    /// Plays note `key` (0 to 127) at `velocity` (0 to 1), from the start
    /// of the next process call; `None` if its event list has no room left
    /// for the note and its release, or it is not playing.
    pub fn note_on(&mut self, key: u8, velocity: f32) -> Option<NoteId> {
        let (_, clap) = self.layout.notes?;
        if !velocity.is_finite() || !self.is_playing() {
            return None;
        }
        let key = key.min(127);
        if self.events.len() - self.params_queued + self.held.len() + 2 > MAX_EVENTS {
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
        debug_assert!(self.events.len() - self.params_queued < MAX_EVENTS);
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
        if self.events.len() - self.params_queued < MAX_EVENTS {
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
        let slice = EventSlice { ptr: self.events.as_ptr(), len: self.events.len() };
        let in_events = ClapInputEvents {
            ctx: (&raw const slice).cast_mut().cast(),
            size: Some(events_size),
            get: Some(events_get),
        };
        let noted = Outputs { host: &self.host, params: &self.params, ids: &self.param_ids };
        let out_events = ClapOutputEvents { ctx: (&raw const noted).cast_mut().cast(), try_push: Some(events_out) };
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
        self.tail = status == CLAP_PROCESS_TAIL;
        if status == CLAP_PROCESS_ERROR {
            // Its events stay queued for whatever follows: a restart clears
            // them, with the notes they belong to.
            return false;
        }
        // By field: `b` still borrows the buffers. The changes sent change
        // what the plugin would save, as CLAP says, but whoever sent them
        // knows: an automation lane's are already in the project.
        self.events.clear();
        self.serials.clear();
        self.params_queued = 0;
        settle(&self.serials, self.high, &self.host.delivered);
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
    /// Parameter changes waiting (but those that need processing) reach
    /// it through `flush`, as does a flush the plugin asked for: on the
    /// audio thread (taken here), as CLAP asks of an active plugin.
    pub fn skip(&mut self, frames: usize) {
        self.tail = false;
        if self.params_queued > 0 || self.host.flush.load(Ordering::Acquire) {
            self.flush_params();
        }
        self.advance(frames);
    }

    /// Hands the plugin the parameter changes it can take without
    /// processing.
    fn flush_params(&mut self) {
        let Some(flush) = self.exts.params.and_then(|p| p.flush) else { return };
        if !self.activated || self.failed {
            return;
        }
        let asked = self.host.flush.swap(false, Ordering::AcqRel);
        // Those flushed first; order does not matter, as each is of its
        // own parameter.
        let mut n = 0;
        for i in 0..self.params_queued {
            // SAFETY: the first `params_queued` events are parameter changes.
            let id = unsafe { self.events[i].param.param_id };
            if self.param(id).is_some_and(|p| p.flags & CLAP_PARAM_REQUIRES_PROCESS == 0) {
                self.events.swap(i, n);
                self.serials.swap(i, n);
                n += 1;
            }
        }
        if n == 0 && !asked {
            return;
        }
        let _role = AudioRole::enter();
        let slice = EventSlice { ptr: self.events.as_ptr(), len: n };
        let in_events = ClapInputEvents {
            ctx: (&raw const slice).cast_mut().cast(),
            size: Some(events_size),
            get: Some(events_get),
        };
        let noted = Outputs { host: &self.host, params: &self.params, ids: &self.param_ids };
        let out_events = ClapOutputEvents { ctx: (&raw const noted).cast_mut().cast(), try_push: Some(events_out) };
        // SAFETY: audio thread, active, not processing meanwhile (this
        // holds `&mut self`); the lists live until it returns.
        unsafe { flush(self.plugin, &in_events, &out_events) };
        // No allocation: what is behind moves up.
        self.events.drain(..n);
        self.serials.drain(..n);
        self.params_queued -= n;
        settle(&self.serials, self.high, &self.host.delivered);
    }

    /// Whether its last process call said it has a tail still to play
    /// (`CLAP_PROCESS_TAIL`): until it says otherwise, silent input may
    /// still bring out sound. Not called since (stopped or skipped), it
    /// has none.
    pub fn has_tail(&self) -> bool {
        self.tail
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
        self.tail = false;
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if !self.on_host_thread() {
            // Destroying it here would break CLAP's threading rules. It
            // stops processing on this thread, which played its audio
            // thread, and goes home whole, for its host to destroy (a poll
            // may serve it meanwhile, which CLAP allows of a live instance).
            // Off the audio callback: this locks, and may allocate.
            self.stop();
            let Some(inbox) = self.inbox.take() else { return };
            let home = Instance {
                plugin: self.plugin,
                fns: self.fns,
                role: self.role,
                layout: std::mem::take(&mut self.layout),
                buffers: std::mem::take(&mut self.buffers),
                events: std::mem::take(&mut self.events),
                params_queued: self.params_queued,
                serials: std::mem::take(&mut self.serials),
                high: self.high,
                exts: self.exts,
                params: std::mem::take(&mut self.params),
                param_ids: std::mem::take(&mut self.param_ids),
                latency: self.latency,
                dry: std::mem::take(&mut self.dry),
                dry_at: self.dry_at,
                held: std::mem::take(&mut self.held),
                next_note: self.next_note,
                steady: self.steady,
                sample_rate: self.sample_rate,
                max_frames: self.max_frames,
                activated: self.activated,
                started: false,
                failed: self.failed,
                tail: false,
                plugin_ref: self.plugin_ref,
                inbox: None,
                // SAFETY: moved out once; this drop touches neither again.
                host: ManuallyDrop::new(unsafe { ManuallyDrop::take(&mut self.host) }),
                dso: ManuallyDrop::new(unsafe { ManuallyDrop::take(&mut self.dso) }),
            };
            match inbox.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                Some(list) => list.push(home),
                // Its host is gone: leaked, with its file.
                None => std::mem::forget(home),
            }
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
    // SAFETY: `ctx` is an `EventSlice` of the instance's events, alive
    // during the call it is given to.
    unsafe { (*(*list).ctx.cast::<EventSlice>()).len as u32 }
}

unsafe extern "C" fn events_get(list: *const ClapInputEvents, index: u32) -> *const ClapEventHeader {
    // SAFETY: as above.
    let slice = unsafe { &*(*list).ctx.cast::<EventSlice>() };
    if (index as usize) < slice.len {
        // SAFETY: within the slice.
        unsafe { slice.ptr.add(index as usize) }.cast()
    } else {
        null()
    }
}

/// Takes the plugin's output events. A change it made to one of its
/// parameters (not a read-only one, which only reports) is noted: its
/// values are to be read again, and its state is dirty, as CLAP says such a
/// change makes it. The rest are let go of.
unsafe extern "C" fn events_out(list: *const ClapOutputEvents, event: *const ClapEventHeader) -> bool {
    if list.is_null() || event.is_null() {
        return false;
    }
    // SAFETY: `ctx` is the instance's `Outputs`, alive during the call; an
    // event starts with its header.
    let (out, header) = unsafe { (&*(*list).ctx.cast::<Outputs>(), *event) };
    if header.space_id == CLAP_CORE_EVENT_SPACE_ID
        && header.kind == CLAP_EVENT_PARAM_VALUE
        && header.size as usize >= size_of::<ClapEventParamValue>()
    {
        // SAFETY: a parameter value event, by its kind and size.
        let id = unsafe { (*event.cast::<ClapEventParamValue>()).param_id };
        out.host.rescan.fetch_or(CLAP_PARAM_RESCAN_VALUES, Ordering::AcqRel);
        let writable = out
            .ids
            .binary_search_by_key(&id, |p| p.0)
            .is_ok_and(|i| out.params.get(out.ids[i].1 as usize).is_some_and(|p| !p.is_readonly()));
        if writable {
            out.host.dirty.store(true, Ordering::Release);
        }
    }
    true
}

/// Moves `delivered` on to the serial before the oldest change still
/// waiting (`serials`), or with none waiting to `high`.
fn settle(serials: &[u64], high: u64, delivered: &AtomicU64) {
    let upto = serials.iter().min().map_or(high, |s| s.saturating_sub(1));
    delivered.fetch_max(upto, Ordering::AcqRel);
}

/// Reads a plugin's parameters: at most [`MAX_PARAMS`], passing over any it
/// cannot describe, with no values to set, or with an id already seen; with
/// the (id, index) of each, by id.
///
/// # Safety
/// Main thread, a live plugin and its own extension.
unsafe fn read_params(plugin: *const ClapPlugin, p: ParamsFns) -> (Vec<ParamInfo>, Vec<(u32, u32)>) {
    let mut params = Vec::new();
    // SAFETY: by the caller's promise.
    let n = unsafe { (p.count)(plugin) } as usize;
    for i in 0..n.min(MAX_PARAMS) {
        let mut info = ClapParamInfo {
            id: CLAP_INVALID_ID,
            flags: 0,
            cookie: null_mut(),
            name: [0; CLAP_NAME_SIZE],
            module: [0; CLAP_PATH_SIZE],
            min_value: 0.0,
            max_value: 0.0,
            default_value: 0.0,
        };
        // SAFETY: an index below its count, and room for the info.
        if !unsafe { (p.get_info)(plugin, i as u32, &mut info) } {
            continue;
        }
        let mut param = ParamInfo {
            id: info.id,
            flags: info.flags,
            name: c_text(&info.name),
            module: c_text(&info.module),
            min: info.min_value,
            max: info.max_value,
            default: 0.0,
        };
        if info.id == CLAP_INVALID_ID || !param.has_values() {
            continue;
        }
        param.default = param.clamp(info.default_value).unwrap_or(param.clamp(param.min).unwrap_or(param.min));
        params.push(param);
    }
    let mut ids: Vec<(u32, u32)> = params.iter().enumerate().map(|(i, p)| (p.id, i as u32)).collect();
    ids.sort_unstable();
    ids.dedup_by_key(|p| p.0);
    if ids.len() < params.len() {
        // Each id once, the first described.
        let mut keep = vec![false; params.len()];
        for &(_, i) in &ids {
            keep[i as usize] = true;
        }
        let mut i = 0;
        params.retain(|_| {
            i += 1;
            keep[i - 1]
        });
        ids = params.iter().enumerate().map(|(i, p)| (p.id, i as u32)).collect();
        ids.sort_unstable();
    }
    (params, ids)
}

/// Reads again what CLAP's `RESCAN_INFO` says may change while a plugin is
/// active (names, and whether a parameter is hidden or periodic) into the
/// parameters its handles see. Its ranges and the rest cannot change then,
/// so the instance's own copy still holds. On the host's thread.
fn refresh_info(h: &HostData) {
    const LIVE_FLAGS: u32 = CLAP_PARAM_IS_HIDDEN | CLAP_PARAM_IS_PERIODIC;
    let plugin = h.plugin.load(Ordering::Acquire);
    let Some(p) = h.exts.get().and_then(|e| e.params) else { return };
    if plugin.is_null() || h.owner != thread_key() {
        return;
    }
    // SAFETY: main thread, a live plugin (destroyed on this thread only)
    // and its own extension; CLAP lets these run while it processes.
    let (fresh, ids) = unsafe { read_params(plugin, p) };
    let mut params = h.params.lock().unwrap_or_else(|e| e.into_inner());
    let updated: Vec<ParamInfo> = params
        .iter()
        .map(|old| match ids.binary_search_by_key(&old.id, |p| p.0) {
            Ok(i) => {
                let new = &fresh[ids[i].1 as usize];
                ParamInfo {
                    name: new.name.clone(),
                    module: new.module.clone(),
                    flags: (old.flags & !LIVE_FLAGS) | (new.flags & LIVE_FLAGS),
                    ..old.clone()
                }
            }
            Err(_) => old.clone(),
        })
        .collect();
    *params = Arc::from(updated);
}

/// A C string in a fixed array, up to its NUL (or the whole array).
fn c_text(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Where a state is saved to: at most [`MAX_STATE`] bytes, and once over,
/// the save has failed whatever the plugin says.
struct Sink {
    bytes: Vec<u8>,
    over: bool,
}

unsafe extern "C" fn sink_write(stream: *const ClapOStream, buffer: *const c_void, size: u64) -> i64 {
    // SAFETY: `ctx` is the `Sink` of the save under way.
    let sink = unsafe { &mut *(*stream).ctx.cast::<Sink>() };
    let size = match usize::try_from(size) {
        Ok(n) if !sink.over && sink.bytes.len().saturating_add(n) <= MAX_STATE => n,
        _ => {
            sink.over = true;
            return -1;
        }
    };
    if size == 0 {
        return 0;
    }
    if buffer.is_null() || sink.bytes.try_reserve(size).is_err() {
        sink.over = true;
        return -1;
    }
    // SAFETY: the plugin's `size` bytes at `buffer`.
    sink.bytes.extend_from_slice(unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), size) });
    size as i64
}

/// Where a state is loaded from.
struct Source<'a> {
    bytes: &'a [u8],
    at: usize,
}

unsafe extern "C" fn source_read(stream: *const ClapIStream, buffer: *mut c_void, size: u64) -> i64 {
    // SAFETY: `ctx` is the `Source` of the load under way.
    let source = unsafe { &mut *(*stream).ctx.cast::<Source>() };
    let left = &source.bytes[source.at..];
    let n = usize::try_from(size).map_or(left.len(), |s| s.min(left.len()));
    if n == 0 {
        return 0;
    }
    if buffer.is_null() {
        return -1;
    }
    // SAFETY: room for `size` bytes at `buffer`, of which `n` are written.
    unsafe { std::ptr::copy_nonoverlapping(left.as_ptr(), buffer.cast::<u8>(), n) };
    source.at += n;
    n as i64
}

/// # Safety
/// Main thread, `plugin` live and `state` its own.
unsafe fn save_state(plugin: *const ClapPlugin, state: StateFns) -> Result<Vec<u8>, String> {
    let mut sink = Sink { bytes: Vec::new(), over: false };
    let stream = ClapOStream { ctx: (&raw mut sink).cast(), write: Some(sink_write) };
    // SAFETY: by the caller's promise; the stream lives until it returns.
    let saved = unsafe { (state.save)(plugin, &stream) };
    if sink.over {
        Err(format!("has a state over {} MiB, too large to keep", MAX_STATE >> 20))
    } else if !saved {
        Err("could not save its state".into())
    } else {
        Ok(sink.bytes)
    }
}

/// # Safety
/// As [`save_state`].
unsafe fn load_state(plugin: *const ClapPlugin, state: StateFns, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_STATE {
        return Err(format!("has a saved state over {} MiB", MAX_STATE >> 20));
    }
    let mut source = Source { bytes, at: 0 };
    let stream = ClapIStream { ctx: (&raw mut source).cast(), read: Some(source_read) };
    // SAFETY: by the caller's promise; the stream lives until it returns.
    if unsafe { (state.load)(plugin, &stream) } { Ok(()) } else { Err("could not load its saved state".into()) }
}

/// The main-thread calls CLAP allows while an instance processes, for its
/// owner while the audio thread holds the [`Instance`]: they reach the
/// plugin through its host data, never the instance. Each works only on
/// its host's thread and while the plugin lives; otherwise it does
/// nothing. Not `Send`.
pub struct PluginHandle {
    host: Arc<HostData>,
    _main: PhantomData<*const ()>,
}

impl PluginHandle {
    /// Its instance's token.
    pub fn token(&self) -> u64 {
        self.host.token
    }

    /// The plugin, if it lives and this is its host's thread (where it is
    /// destroyed, so it cannot be while a call here runs).
    fn plugin(&self) -> Option<(*const ClapPlugin, Exts)> {
        let plugin = self.host.plugin.load(Ordering::Acquire);
        (self.host.owner == thread_key() && !plugin.is_null())
            .then(|| (plugin.cast_const(), self.host.exts.get().copied().unwrap_or_default()))
    }

    /// Whether its instance still lives.
    pub fn is_alive(&self) -> bool {
        !self.host.plugin.load(Ordering::Acquire).is_null()
    }

    /// Its parameters, as last read: at its last activation, with names
    /// and what is hidden as of the last poll that followed a change of
    /// them.
    pub fn params(&self) -> Arc<[ParamInfo]> {
        self.host.params.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The value of parameter `id` now.
    pub fn value(&self, id: u32) -> Option<f64> {
        let (plugin, exts) = self.plugin()?;
        let mut v = f64::NAN;
        // SAFETY: main thread, a live plugin, room for the value.
        let ok = unsafe { (exts.params?.get_value)(plugin, id, &mut v) };
        (ok && v.is_finite()).then_some(v)
    }

    /// Parameter `id` at `value`, as the plugin writes it.
    pub fn text(&self, id: u32, value: f64) -> Option<String> {
        let (plugin, exts) = self.plugin()?;
        let f = exts.params?.value_to_text?;
        let mut buf: [c_char; 128] = [0; 128];
        // SAFETY: main thread, a live plugin, and the buffer's capacity.
        if !unsafe { f(plugin, id, value, buf.as_mut_ptr(), buf.len() as u32) } {
            return None;
        }
        // One that filled it unterminated is cut short.
        buf[buf.len() - 1] = 0;
        Some(c_text(&buf))
    }

    /// The value the plugin reads `text` as, for parameter `id`.
    pub fn parse(&self, id: u32, text: &str) -> Option<f64> {
        let (plugin, exts) = self.plugin()?;
        let f = exts.params?.text_to_value?;
        let text = CString::new(text).ok()?;
        let mut v = f64::NAN;
        // SAFETY: main thread, a live plugin, a NUL-terminated text and
        // room for the value.
        let ok = unsafe { f(plugin, id, text.as_ptr(), &mut v) };
        (ok && v.is_finite()).then_some(v)
    }

    /// Saves its state, while it plays: `None` if it keeps none.
    pub fn save_state(&self) -> Result<Option<Vec<u8>>, String> {
        let Some((plugin, exts)) = self.plugin() else { return Err("is gone, or on another thread".into()) };
        let Some(state) = exts.state else { return Ok(None) };
        // SAFETY: main thread, a live plugin and its own extension.
        unsafe { save_state(plugin, state) }.map(Some)
    }

    /// The serial up to which every parameter change
    /// ([`Instance::set_param`]) has reached the plugin, or was dropped
    /// with its parameter.
    pub fn delivered(&self) -> u64 {
        self.host.delivered.load(Ordering::Acquire)
    }
}
