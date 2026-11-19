//! A CLAP plugin file for TINVO's tests (`tests/clap.rs`), built as a
//! shared library by `cargo test`. It is not an example to copy.
//!
//! What it does depends on the name it is loaded under, the file name
//! without its extension. For scanning:
//!
//! - `crash` ends the process in `init`;
//! - `hang` never returns from `init`;
//! - `refuse` fails its `init`;
//! - `noisy` writes a few megabytes to stdout and stderr in `init`;
//! - `nofactory` has no plugin factory;
//! - `deinitcrash` ends the process in `deinit`;
//! - `helper` starts a shell that writes `helper.marker` beside the file
//!   three seconds later, unless it is ended first.
//!
//! Any other offers a gain effect (halving a stereo input) and a sine
//! instrument (stereo, the right channel half the left), plus a third
//! descriptor with no name, which a host must pass over. Their instances
//! check every call against CLAP's rules (the thread the host's
//! `clap.thread-check` says it is on, the same OS thread for every
//! main-thread call, the order of the lifecycle, the buffers and events
//! they are given) and count what breaks one; the extension
//! `org.tinvo.test.stats` reads the counts. Names that change them:
//!
//! - `midi`: the sine takes MIDI 1 only;
//! - `both`: the gain takes notes too, so it can be made as an instrument;
//! - `tail`: the gain says it has a tail to play after every call;
//! - `sidechain`: the gain has a second stereo input and a second mono
//!   output, and checks the input is silent;
//! - `noactivate` / `nostart`: `activate` / `start_processing` fail;
//! - `hardrt`: needs real time (`clap.render`); `offlinerefuse`: refuses
//!   to render offline;
//! - `restart` (and `leak`): asks for a restart and a main-thread callback
//!   in its first process call;
//! - `incomplete`: creates plugins without a `process`;
//! - `initfail`: its plugins' `init` fails;
//! - `nomain`: port 0 each way is not flagged main;
//! - `latency`: both plugins say their output comes 64 frames late, and
//!   it does; 64 more at each activation after; `relatency`: the same,
//!   asking for a restart in its first process call as `restart` does;
//!   `latefail`: the same, failing its second process call as `procerror`
//!   does;
//! - `meter`: the gain reports its read-only meter after every call, and
//!   once, a change of its own to its mode, after which (in a main-thread
//!   callback) it renames its gain "Volume", shows its hold and clears
//!   what refers to the hold;
//! - `dirty`: marks its state dirty in a callback it asks for in its
//!   first process call;
//! - `bigstate`: the state saved is 17 MiB, and the save says it worked
//!   however much the host took; `savefail` / `loadfail`: saving / loading
//!   the state fails;
//! - `procerror`: `process` fails on its second call, before reading its
//!   events; `procerrors`: on every call from the second, asking for a
//!   restart each time, and from the main thread on every callback after;
//! - `lifecycle`, `leak`, and the five above but `restart`: write
//!   `NAME.deinit` beside the file in `deinit`, with the times the file
//!   was initialised, the instances alive then and the rules broken
//!   overall.
//!
//! Their parameters (`clap.params`): the gain's gain (10, 0 to 1, 0.5 at
//! first, the factor it plays at), mode (11, stepped 0 to 2), a hidden
//! hold (12: set to 1, its next process call waits until a state is saved,
//! with the meter at 1 meanwhile), a read-only meter (13) and an offset
//! added to its output (14, -1 to 1, which CLAP's `REQUIRES_PROCESS` keeps
//! out of a flush while it is active); the sine's level (20, 0 to 1, 0.25
//! at first, each voice's). Their state (`clap.state`) is their
//! parameters' values, read and written a few bytes at a time; loading it
//! tells the host its values changed.
//!
//! Voices survive deactivation and activation; only `reset` (or a release)
//! ends them, as CLAP allows.

use std::cell::UnsafeCell;
use std::collections::hash_map::DefaultHasher;
use std::ffi::{CStr, c_char, c_void};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};
use tinvo::clap::ffi::*;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Mode {
    Normal,
    NoFactory,
    CrashOnDeinit,
    Midi,
    Sidechain,
    NoActivate,
    NoStart,
    HardRealtime,
    OfflineRefuse,
    Restart,
    Incomplete,
    Lifecycle,
    InitFail,
    NoMain,
    ProcessError,
    Leak,
    Both,
    Tail,
    ProcessErrors,
    Latency,
    Meter,
    BigState,
    SaveFail,
    LoadFail,
    Dirty,
    Relatency,
    LateFail,
}

static MODE: AtomicU8 = AtomicU8::new(Mode::Normal as u8);
static PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static ENTRY_INITS: AtomicU32 = AtomicU32::new(0);
static LIVE: AtomicU32 = AtomicU32::new(0);
static BROKEN: AtomicU32 = AtomicU32::new(0);
/// States saved, by any instance: a held process call waits for one.
static SAVES: AtomicU32 = AtomicU32::new(0);

fn mode() -> Mode {
    const ALL: [Mode; 27] = [
        Mode::Normal,
        Mode::NoFactory,
        Mode::CrashOnDeinit,
        Mode::Midi,
        Mode::Sidechain,
        Mode::NoActivate,
        Mode::NoStart,
        Mode::HardRealtime,
        Mode::OfflineRefuse,
        Mode::Restart,
        Mode::Incomplete,
        Mode::Lifecycle,
        Mode::InitFail,
        Mode::NoMain,
        Mode::ProcessError,
        Mode::Leak,
        Mode::Both,
        Mode::Tail,
        Mode::ProcessErrors,
        Mode::Latency,
        Mode::Meter,
        Mode::BigState,
        Mode::SaveFail,
        Mode::LoadFail,
        Mode::Dirty,
        Mode::Relatency,
        Mode::LateFail,
    ];
    ALL[usize::from(MODE.load(Ordering::Relaxed))]
}

/// Whether its output comes late.
fn late() -> bool {
    matches!(mode(), Mode::Latency | Mode::Relatency | Mode::LateFail)
}

/// Static C data: only ever read, by any thread.
struct Shared<T>(T);
// SAFETY: the pointers inside point at static, never-written data.
unsafe impl<T> Sync for Shared<T> {}

static GAIN_FEATURES: Shared<[*const c_char; 3]> = Shared([c"audio-effect".as_ptr(), c"stereo".as_ptr(), null()]);
static SINE_FEATURES: Shared<[*const c_char; 3]> = Shared([c"instrument".as_ptr(), c"synthesizer".as_ptr(), null()]);
static NO_FEATURES: Shared<[*const c_char; 1]> = Shared([null()]);

const fn descriptor(
    id: &'static CStr,
    name: &'static CStr,
    description: &'static CStr,
    features: *const *const c_char,
) -> ClapPluginDescriptor {
    ClapPluginDescriptor {
        clap_version: CLAP_VERSION,
        id: id.as_ptr(),
        name: name.as_ptr(),
        vendor: c"TINVO tests".as_ptr(),
        url: c"".as_ptr(),
        manual_url: c"".as_ptr(),
        support_url: c"".as_ptr(),
        version: c"1.0.0".as_ptr(),
        description: description.as_ptr(),
        features,
    }
}

static DESCRIPTORS: Shared<[ClapPluginDescriptor; 3]> = Shared([
    descriptor(c"org.tinvo.test.gain", c"Test Gain", c"Halves its input", GAIN_FEATURES.0.as_ptr()),
    descriptor(c"org.tinvo.test.sine", c"Test Sine", c"A plain sine", SINE_FEATURES.0.as_ptr()),
    descriptor(c"org.tinvo.test.unnamed", c"", c"Has no name", NO_FEATURES.0.as_ptr()),
]);

/// What `org.tinvo.test.stats` reports; `tests/clap.rs` has the same.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub entry_inits: u32,
    pub live: u32,
    pub activates: u32,
    pub deactivates: u32,
    pub starts: u32,
    pub stops: u32,
    pub processes: u32,
    pub callbacks: u32,
    pub broken: u32,
    pub notes_on: u32,
    pub notes_off: u32,
    pub offline: u32,
    pub max_frames: u32,
    pub frames: u32,
    /// Where the last process call left steady time (`u32::MAX` unknown).
    pub steady: u32,
    /// Parameter changes taken, in process calls and flushes.
    pub params_set: u32,
    pub flushes: u32,
    /// States saved while a process call was held.
    pub overlaps: u32,
    /// The gain as it was activated.
    pub start_gain: f64,
}

#[repr(C)]
struct StatsExt {
    get: unsafe extern "C" fn(plugin: *const ClapPlugin, out: *mut Stats),
    /// What the host's `clap.thread-check` says of the calling thread:
    /// bit 0 main, bit 1 audio.
    probe: unsafe extern "C" fn(plugin: *const ClapPlugin) -> u32,
}

static STATS_EXT: StatsExt = StatsExt { get: get_stats, probe };

unsafe extern "C" fn probe(p: *const ClapPlugin) -> u32 {
    // SAFETY: the caller's instance.
    let me = unsafe { me(p) };
    u32::from(me.is_main()) | u32::from(me.is_audio()) << 1
}

/// One of the plugins' parameters.
struct Param {
    id: u32,
    flags: u32,
    name: &'static CStr,
    module: &'static CStr,
    min: f64,
    max: f64,
    default: f64,
}

const GAIN: u32 = 10;
const KIND: u32 = 11;
/// Set to 1, it holds the next process call until a state is saved.
const HOLD: u32 = 12;
const METER: u32 = 13;
const OFFSET: u32 = 14;
const LEVEL: u32 = 20;
/// Frames the gain's output comes late in `latency`, at each activation:
/// 64 at the first, 128 at the second, and so on.
const LATENCY: u32 = 64;

static GAIN_PARAMS: [Param; 5] = [
    Param {
        id: GAIN,
        flags: CLAP_PARAM_IS_AUTOMATABLE,
        name: c"Gain",
        module: c"Main",
        min: 0.0,
        max: 1.0,
        default: 0.5,
    },
    Param {
        id: KIND,
        flags: CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_ENUM | CLAP_PARAM_IS_AUTOMATABLE,
        name: c"Mode",
        module: c"Main",
        min: 0.0,
        max: 2.0,
        default: 0.0,
    },
    Param { id: HOLD, flags: CLAP_PARAM_IS_HIDDEN, name: c"Hold", module: c"", min: 0.0, max: 1.0, default: 0.0 },
    Param { id: METER, flags: CLAP_PARAM_IS_READONLY, name: c"Meter", module: c"", min: 0.0, max: 1.0, default: 0.0 },
    Param {
        id: OFFSET,
        flags: CLAP_PARAM_REQUIRES_PROCESS | CLAP_PARAM_IS_AUTOMATABLE,
        name: c"Offset",
        module: c"",
        min: -1.0,
        max: 1.0,
        default: 0.0,
    },
];
static SINE_PARAMS: [Param; 1] = [Param {
    id: LEVEL,
    flags: CLAP_PARAM_IS_AUTOMATABLE,
    name: c"Level",
    module: c"",
    min: 0.0,
    max: 1.0,
    default: 0.25,
}];

/// Enough for every note a host can hold at once.
const VOICES: usize = 512;

#[derive(Clone, Copy, Default)]
struct Voice {
    on: bool,
    id: i32,
    key: u8,
    phase: f64,
}

/// What only one call at a time uses: `activate` (not active) or the
/// audio-thread calls (active).
struct Audio {
    sample_rate: f64,
    max_frames: u32,
    next_steady: i64,
    voices: [Voice; VOICES],
    asked: bool,
    /// The gain's output still to come, in `latency`.
    delay: Vec<[f32; 2]>,
    delay_at: usize,
    /// The mode change it made itself, in `meter`.
    told: bool,
}

/// One instance. `plugin` comes first: the host's `*const ClapPlugin` is
/// this. The rest is atomic, or in `audio`, as `on_main_thread` may run
/// during `process`.
#[repr(C)]
struct Instance {
    plugin: ClapPlugin,
    host: *const ClapHost,
    check: AtomicPtr<ClapHostThreadCheck>,
    sine: bool,
    main_thread: AtomicU64,
    active: AtomicBool,
    processing: AtomicBool,
    audio: UnsafeCell<Audio>,
    max_frames: AtomicU32,
    activates: AtomicU32,
    deactivates: AtomicU32,
    starts: AtomicU32,
    stops: AtomicU32,
    processes: AtomicU32,
    callbacks: AtomicU32,
    broken: AtomicU32,
    notes_on: AtomicU32,
    notes_off: AtomicU32,
    offline: AtomicU32,
    frames: AtomicU32,
    steady: AtomicU32,
    /// Its parameters' values, as `f64` bits, in the order of its table.
    values: [AtomicU64; 5],
    params_set: AtomicU32,
    flushes: AtomicU32,
    overlaps: AtomicU32,
    start_gain: AtomicU64,
    /// Its latency since it was last activated, in `latency`.
    latency: AtomicU32,
    /// In `meter`, it renamed its gain and showed its hold.
    renamed: AtomicBool,
}

/// # Safety
/// `p` is a plugin `create_plugin` made and `destroy` has not freed.
unsafe fn me<'a>(p: *const ClapPlugin) -> &'a Instance {
    // SAFETY: by the caller's promise; `plugin` is the first field.
    unsafe { &*p.cast::<Instance>() }
}

// `thread::current` registers a thread-exit destructor in this library's
// own standard library, on whatever thread calls in, as real plugins do: a
// host that unloaded the library would crash when such a thread exits.
fn thread_hash() -> u64 {
    let mut h = DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    h.finish()
}

impl Instance {
    /// # Safety
    /// Called from `activate` or an audio-thread call, which CLAP never
    /// runs at once for one instance.
    #[allow(clippy::mut_from_ref)]
    unsafe fn audio(&self) -> &mut Audio {
        // SAFETY: by the caller's promise, nothing else is using it.
        unsafe { &mut *self.audio.get() }
    }

    fn broke(&self) {
        self.broken.fetch_add(1, Ordering::Relaxed);
        BROKEN.fetch_add(1, Ordering::Relaxed);
    }

    fn require(&self, ok: bool) {
        if !ok {
            self.broke();
        }
    }

    fn is_main(&self) -> bool {
        let check = self.check.load(Ordering::Relaxed);
        // SAFETY: the host's extension, valid while the plugin lives.
        !check.is_null() && unsafe { (*check).is_main_thread.is_some_and(|f| f(self.host)) }
    }

    fn is_audio(&self) -> bool {
        let check = self.check.load(Ordering::Relaxed);
        // SAFETY: as above.
        !check.is_null() && unsafe { (*check).is_audio_thread.is_some_and(|f| f(self.host)) }
    }

    fn params(&self) -> &'static [Param] {
        if self.sine { &SINE_PARAMS } else { &GAIN_PARAMS }
    }

    fn value(&self, id: u32) -> Option<f64> {
        let i = self.params().iter().position(|p| p.id == id)?;
        Some(f64::from_bits(self.values[i].load(Ordering::Relaxed)))
    }

    fn set_value(&self, id: u32, v: f64) {
        if let Some(i) = self.params().iter().position(|p| p.id == id) {
            self.values[i].store(v.to_bits(), Ordering::Relaxed);
        }
    }

    /// A parameter change from the host, checked: an id it has and may be
    /// set, a value in range (whole for a stepped one), for every note.
    fn take_param(&self, e: &ClapEventParamValue) {
        let p = self.params().iter().find(|p| p.id == e.param_id);
        let ok = p.is_some_and(|p| {
            p.flags & CLAP_PARAM_IS_READONLY == 0
                && (p.min..=p.max).contains(&e.value)
                && (p.flags & CLAP_PARAM_IS_STEPPED == 0 || e.value.fract() == 0.0)
        }) && e.cookie.is_null()
            && (e.note_id, e.port_index, e.channel, e.key) == (-1, -1, -1, -1);
        if !ok {
            self.broke();
            return;
        }
        self.set_value(e.param_id, e.value);
        self.params_set.fetch_add(1, Ordering::Relaxed);
    }

    fn ask_callback(&self) {
        // SAFETY: the host's thread-safe callback.
        unsafe {
            if let Some(f) = (*self.host).request_callback {
                f(self.host);
            }
        }
    }

    /// The host's `clap.params`, on the main thread.
    fn host_params(&self) -> Option<&ClapHostParams> {
        // SAFETY: the host's own extension, valid while the plugin lives.
        unsafe { self.host_extension(CLAP_EXT_PARAMS).cast::<ClapHostParams>().as_ref() }
    }

    /// The host's extension `id`, or null.
    fn host_extension(&self, id: &CStr) -> *const c_void {
        // SAFETY: the host given to create_plugin, usable from init on.
        unsafe { (*self.host).get_extension.map_or(null(), |f| f(self.host, id.as_ptr())) }
    }

    /// A main-thread call: the host says so, and it is the OS thread the
    /// plugin was initialised on.
    fn on_main(&self) {
        self.require(self.is_main());
        let here = thread_hash();
        let first = self.main_thread.compare_exchange(0, here, Ordering::Relaxed, Ordering::Relaxed);
        self.require(first.is_ok() || first == Err(here));
    }

    fn ask_restart_and_callback(&self) {
        // SAFETY: the host's thread-safe callbacks.
        unsafe {
            if let Some(f) = (*self.host).request_restart {
                f(self.host);
            }
            if let Some(f) = (*self.host).request_callback {
                f(self.host);
            }
        }
    }
}

unsafe extern "C" fn plugin_init(p: *const ClapPlugin) -> bool {
    // SAFETY: the host calls with its plugin.
    let me = unsafe { me(p) };
    // SAFETY: the host given to create_plugin, usable from init on.
    let check = unsafe {
        (*me.host)
            .get_extension
            .map_or(null(), |f| f(me.host, CLAP_EXT_THREAD_CHECK.as_ptr()).cast::<ClapHostThreadCheck>())
    };
    me.check.store(check.cast_mut(), Ordering::Relaxed);
    me.require(!check.is_null());
    me.on_main();
    mode() != Mode::InitFail
}

unsafe extern "C" fn plugin_destroy(p: *const ClapPlugin) {
    // SAFETY: as above, freed once here.
    let me = unsafe { me(p) };
    // A plugin a host lets go of without initialising has not seen the
    // host's thread-check yet.
    if !me.check.load(Ordering::Relaxed).is_null() {
        me.on_main();
    }
    me.require(!me.active.load(Ordering::Relaxed));
    LIVE.fetch_sub(1, Ordering::Relaxed);
    // SAFETY: made by Box::into_raw in create_plugin, and taken back through
    // that same pointer; nothing uses it now.
    drop(unsafe { Box::from_raw(p.cast::<Instance>().cast_mut()) });
}

unsafe extern "C" fn plugin_activate(p: *const ClapPlugin, sample_rate: f64, min: u32, max: u32) -> bool {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    me.require(!me.active.load(Ordering::Relaxed) && min >= 1 && min <= max && sample_rate > 0.0);
    if mode() == Mode::NoActivate {
        return false;
    }
    // SAFETY: activate, not active, so no audio-thread call.
    let audio = unsafe { me.audio() };
    audio.sample_rate = sample_rate;
    audio.max_frames = max;
    audio.next_steady = 0;
    let n = me.activates.fetch_add(1, Ordering::Relaxed) + 1;
    let latency = if late() { LATENCY * n } else { 0 };
    audio.delay = vec![[0.0; 2]; latency as usize];
    audio.delay_at = 0;
    me.latency.store(latency, Ordering::Relaxed);
    me.start_gain.store(me.values[0].load(Ordering::Relaxed), Ordering::Relaxed);
    me.max_frames.store(max, Ordering::Relaxed);
    me.active.store(true, Ordering::Relaxed);
    true
}

unsafe extern "C" fn plugin_deactivate(p: *const ClapPlugin) {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    me.require(me.active.load(Ordering::Relaxed) && !me.processing.load(Ordering::Relaxed));
    me.active.store(false, Ordering::Relaxed);
    me.deactivates.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn plugin_start(p: *const ClapPlugin) -> bool {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.require(me.is_audio() && me.active.load(Ordering::Relaxed) && !me.processing.load(Ordering::Relaxed));
    if mode() == Mode::NoStart {
        return false;
    }
    me.processing.store(true, Ordering::Relaxed);
    me.starts.fetch_add(1, Ordering::Relaxed);
    true
}

unsafe extern "C" fn plugin_stop(p: *const ClapPlugin) {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.require(me.is_audio() && me.processing.load(Ordering::Relaxed));
    me.processing.store(false, Ordering::Relaxed);
    me.stops.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn plugin_reset(p: *const ClapPlugin) {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.require(me.is_audio() && me.active.load(Ordering::Relaxed));
    // SAFETY: an audio-thread call.
    unsafe { me.audio() }.voices = [Voice::default(); VOICES];
}

/// The channels of audio buffer `b`, `frames` long.
///
/// # Safety
/// `b` is one of the host's buffers for this call, with that many frames.
unsafe fn channel<'a>(b: &ClapAudioBuffer, c: usize, frames: usize) -> &'a mut [f32] {
    // SAFETY: by the caller's promise.
    unsafe { std::slice::from_raw_parts_mut(*b.data32.add(c), frames) }
}

/// Delays the output as late as it says it comes.
fn delay(audio: &mut Audio, l: &mut [f32], r: &mut [f32]) {
    let n = audio.delay.len();
    if n == 0 {
        return;
    }
    for (l, r) in l.iter_mut().zip(r.iter_mut()) {
        let late = audio.delay[audio.delay_at];
        audio.delay[audio.delay_at] = [*l, *r];
        audio.delay_at = (audio.delay_at + 1) % n;
        (*l, *r) = (late[0], late[1]);
    }
}

unsafe extern "C" fn plugin_process(p: *const ClapPlugin, process: *const ClapProcess) -> ClapProcessStatus {
    // SAFETY: as above, and the host's process data for this call.
    let (me, pr) = unsafe { (me(p), &*process) };
    me.require(me.is_audio() && me.processing.load(Ordering::Relaxed));
    // SAFETY: an audio-thread call.
    let audio = unsafe { me.audio() };
    let frames = pr.frames_count as usize;
    me.require(frames >= 1 && pr.frames_count <= audio.max_frames);
    // -1 is "unknown", and stays so; otherwise time only moves on.
    if audio.next_steady >= 0 {
        me.require(pr.steady_time == -1 || pr.steady_time >= audio.next_steady);
    } else {
        me.require(pr.steady_time == -1);
    }
    audio.next_steady = match pr.steady_time {
        -1 => -1,
        t => t.checked_add(i64::from(pr.frames_count)).unwrap_or(-1),
    };
    me.steady.store(u32::try_from(audio.next_steady).unwrap_or(u32::MAX), Ordering::Relaxed);
    me.processes.fetch_add(1, Ordering::Relaxed);
    me.frames.fetch_add(pr.frames_count, Ordering::Relaxed);
    if matches!(mode(), Mode::Restart | Mode::Leak | Mode::Relatency) && !audio.asked {
        audio.asked = true;
        // SAFETY: the host's thread-safe callbacks.
        unsafe {
            if let Some(f) = (*me.host).request_restart {
                f(me.host);
            }
            if let Some(f) = (*me.host).request_callback {
                f(me.host);
            }
        }
    }
    if matches!(mode(), Mode::ProcessError | Mode::LateFail) && me.processes.load(Ordering::Relaxed) == 2 {
        return CLAP_PROCESS_ERROR;
    }
    if mode() == Mode::ProcessErrors && me.processes.load(Ordering::Relaxed) >= 2 {
        me.ask_restart_and_callback();
        return CLAP_PROCESS_ERROR;
    }
    let (inputs, outputs) = ports(me.sine);
    if pr.audio_inputs_count as usize != inputs.len() || pr.audio_outputs_count as usize != outputs.len() {
        me.broke();
        return CLAP_PROCESS_ERROR;
    }
    // SAFETY: arrays of the counts just checked; the outputs are the
    // plugin's to write, masks included.
    let ins = unsafe { std::slice::from_raw_parts(pr.audio_inputs, inputs.len()) };
    let outs = unsafe { std::slice::from_raw_parts_mut(pr.audio_outputs, outputs.len()) };
    for (b, &c) in ins.iter().zip(inputs).chain(outs.iter().zip(outputs)) {
        if b.channel_count != c || b.data32.is_null() || b.constant_mask != 0 {
            me.broke();
            return CLAP_PROCESS_ERROR;
        }
    }
    if me.sine {
        // SAFETY: the host's event list for this call.
        let events = unsafe { &*pr.in_events };
        // SAFETY: the main output, stereo as checked.
        let (l, r) = unsafe { (channel(&outs[0], 0, frames), channel(&outs[0], 1, frames)) };
        let count = events.size.map_or(0, |f| unsafe { f(events) });
        let mut next = 0;
        let mut last_time = 0;
        for i in 0..frames {
            while next < count {
                // SAFETY: an index below the list's size.
                let e = unsafe { events.get.map_or(null(), |f| f(events, next)) };
                if e.is_null() {
                    me.broke();
                    next += 1;
                    continue;
                }
                // SAFETY: the host's event, valid during this call.
                let time = unsafe { (*e).time };
                if time as usize > i {
                    break;
                }
                me.require(time >= last_time && (time as usize) < frames);
                last_time = time;
                // SAFETY: an event of the kind its header says.
                unsafe { me.event(audio, e) };
                next += 1;
            }
            let mut s = 0.0;
            let rate = audio.sample_rate;
            let level = me.value(LEVEL).unwrap_or(0.25);
            for v in audio.voices.iter_mut().filter(|v| v.on) {
                s += level * v.phase.cos();
                let hz = 440.0 * 2f64.powf((f64::from(v.key) - 69.0) / 12.0);
                v.phase = (v.phase + std::f64::consts::TAU * hz / rate) % std::f64::consts::TAU;
            }
            l[i] = s as f32;
            r[i] = 0.5 * s as f32;
        }
        while next < count {
            // SAFETY: as above.
            let e = unsafe { events.get.map_or(null(), |f| f(events, next)) };
            me.require(e.is_null());
            next += 1;
        }
        delay(audio, l, r);
        if l.iter().all(|&x| x == 0.0) {
            // Silent: both channels constant, which a host may use.
            outs[0].constant_mask = 0b11;
        }
    } else {
        // SAFETY: the host's event list for this call.
        let events = unsafe { &*pr.in_events };
        let count = events.size.map_or(0, |f| unsafe { f(events) });
        for n in 0..count {
            // SAFETY: an index below the list's size.
            let e = unsafe { events.get.map_or(null(), |f| f(events, n)) };
            // SAFETY: the host's event, valid during this call.
            if !e.is_null() && unsafe { (*e).kind } == CLAP_EVENT_PARAM_VALUE {
                // SAFETY: as above.
                me.require(unsafe { (*e).time } == 0);
                // SAFETY: a parameter change, by its kind.
                unsafe { me.event(audio, e) };
            }
        }
        let gain = me.value(GAIN).unwrap_or(0.5) as f32;
        let offset = me.value(OFFSET).unwrap_or(0.0) as f32;
        for c in 0..2 {
            // SAFETY: the main ports, stereo as checked.
            let (i, o) = unsafe { (channel(&ins[0], c, frames), channel(&outs[0], c, frames)) };
            for (o, i) in o.iter_mut().zip(i.iter()) {
                *o = gain * i + offset;
            }
        }
        // SAFETY: the main output, stereo as checked.
        let (l, r) = unsafe { (channel(&outs[0], 0, frames), channel(&outs[0], 1, frames)) };
        delay(audio, l, r);
        if mode() == Mode::Normal && me.value(HOLD) == Some(1.0) {
            // Held until the main thread saves a state, with the meter at 1
            // meanwhile; given up after a while, which the test sees.
            let saves = SAVES.load(Ordering::Acquire);
            me.set_value(METER, 1.0);
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_secs(10) {
                if SAVES.load(Ordering::Acquire) != saves {
                    me.overlaps.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                std::thread::yield_now();
            }
            me.set_value(METER, 0.0);
            me.set_value(HOLD, 0.0);
        }
        if mode() == Mode::Dirty && !audio.told {
            audio.told = true;
            me.ask_callback();
        }
        if mode() == Mode::Meter {
            // SAFETY: the host's output list for this call.
            let out = unsafe { &*pr.out_events };
            let tell = |id: u32, value: f64| {
                let e = ClapEventParamValue {
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
                me.set_value(id, value);
                // SAFETY: the host's list, given an event it may copy (the
                // whole of it, which the pointer covers).
                me.require(out.try_push.is_some_and(|f| unsafe { f(out, (&raw const e).cast()) }));
            };
            tell(METER, 0.5);
            if !audio.told {
                audio.told = true;
                tell(KIND, 1.0);
                me.ask_callback();
            }
        }
        if mode() == Mode::Sidechain {
            for c in 0..2 {
                // SAFETY: the second input, stereo as checked.
                let side = unsafe { channel(&ins[1], c, frames) };
                me.require(side.iter().all(|&x| x == 0.0));
            }
            // SAFETY: the second output, mono as checked.
            unsafe { channel(&outs[1], 0, frames) }.fill(1.0);
        }
    }
    if mode() == Mode::Tail { CLAP_PROCESS_TAIL } else { CLAP_PROCESS_CONTINUE }
}

impl Instance {
    /// # Safety
    /// `e` heads an event of the kind it says, from the host.
    unsafe fn event(&self, audio: &mut Audio, e: *const ClapEventHeader) {
        let midi_only = mode() == Mode::Midi;
        // SAFETY: by the caller's promise.
        let h = unsafe { *e };
        if (h.space_id, h.kind) == (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE) {
            self.require(h.size as usize == size_of::<ClapEventParamValue>());
            // SAFETY: a parameter change, by its kind.
            self.take_param(unsafe { &*e.cast::<ClapEventParamValue>() });
            return;
        }
        let (on, id, key, velocity) = match (h.space_id, h.kind) {
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF) => {
                self.require(!midi_only && h.size as usize == size_of::<ClapEventNote>());
                // SAFETY: a note event, by its kind.
                let n = unsafe { &*e.cast::<ClapEventNote>() };
                self.require(n.port_index == 0 && n.channel == 0 && (0..128).contains(&n.key) && n.note_id >= 0);
                (h.kind == CLAP_EVENT_NOTE_ON, n.note_id, n.key as u8, n.velocity)
            }
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI) => {
                self.require(midi_only && h.size as usize == size_of::<ClapEventMidi>());
                // SAFETY: a MIDI event, by its kind.
                let m = unsafe { &*e.cast::<ClapEventMidi>() };
                match m.data[0] & 0xf0 {
                    0x90 if m.data[2] > 0 => (true, -1, m.data[1], f64::from(m.data[2]) / 127.0),
                    0x80 | 0x90 => (false, -1, m.data[1], 0.0),
                    _ => return,
                }
            }
            _ => {
                self.broke();
                return;
            }
        };
        if on {
            self.require((0.0..=1.0).contains(&velocity));
            self.notes_on.fetch_add(1, Ordering::Relaxed);
            if let Some(v) = audio.voices.iter_mut().find(|v| !v.on) {
                *v = Voice { on: true, id, key, phase: 0.0 };
            }
        } else {
            self.notes_off.fetch_add(1, Ordering::Relaxed);
            // A release names a held note: by id for CLAP's notes, by key
            // for MIDI 1's.
            match audio.voices.iter_mut().find(|v| v.on && v.key == key && (id < 0 || v.id == id)) {
                Some(v) => v.on = false,
                None => self.broke(),
            }
        }
    }
}

/// The channels of each input and output port.
fn ports(sine: bool) -> (&'static [u32], &'static [u32]) {
    match (sine, mode()) {
        (true, _) => (&[], &[2]),
        (false, Mode::Sidechain) => (&[2, 2], &[2, 1]),
        (false, _) => (&[2], &[2]),
    }
}

unsafe extern "C" fn audio_count(p: *const ClapPlugin, is_input: bool) -> u32 {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    me.require(!me.active.load(Ordering::Relaxed));
    let (i, o) = ports(me.sine);
    if is_input { i.len() as u32 } else { o.len() as u32 }
}

unsafe extern "C" fn audio_get(p: *const ClapPlugin, index: u32, is_input: bool, info: *mut ClapAudioPortInfo) -> bool {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    let (i, o) = ports(me.sine);
    let Some(&channels) = (if is_input { i } else { o }).get(index as usize) else {
        me.broke();
        return false;
    };
    // SAFETY: the host's out-pointer.
    let info = unsafe { &mut *info };
    info.id = index;
    info.flags = if index == 0 && mode() != Mode::NoMain { CLAP_AUDIO_PORT_IS_MAIN } else { 0 };
    info.channel_count = channels;
    info.port_type = null();
    info.in_place_pair = CLAP_INVALID_ID;
    true
}

static AUDIO_PORTS: ClapPluginAudioPorts = ClapPluginAudioPorts { count: Some(audio_count), get: Some(audio_get) };

unsafe extern "C" fn note_count(p: *const ClapPlugin, is_input: bool) -> u32 {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    u32::from(is_input && me.takes_notes())
}

unsafe extern "C" fn note_get(p: *const ClapPlugin, index: u32, is_input: bool, info: *mut ClapNotePortInfo) -> bool {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    if !(me.takes_notes() && is_input && index == 0) {
        me.broke();
        return false;
    }
    let dialects =
        if mode() == Mode::Midi { CLAP_NOTE_DIALECT_MIDI } else { CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI };
    // SAFETY: the host's out-pointer.
    let info = unsafe { &mut *info };
    info.id = 0;
    info.supported_dialects = dialects;
    info.preferred_dialect = dialects & CLAP_NOTE_DIALECT_CLAP;
    true
}

static NOTE_PORTS: ClapPluginNotePorts = ClapPluginNotePorts { count: Some(note_count), get: Some(note_get) };

unsafe extern "C" fn render_hard(p: *const ClapPlugin) -> bool {
    // SAFETY: as above.
    unsafe { me(p) }.on_main();
    mode() == Mode::HardRealtime
}

unsafe extern "C" fn render_set(p: *const ClapPlugin, mode_: i32) -> bool {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    me.require(!me.active.load(Ordering::Relaxed));
    if mode_ == CLAP_RENDER_OFFLINE {
        if mode() == Mode::OfflineRefuse {
            return false;
        }
        me.offline.store(1, Ordering::Relaxed);
    }
    true
}

static RENDER: ClapPluginRender =
    ClapPluginRender { has_hard_realtime_requirement: Some(render_hard), set: Some(render_set) };

unsafe extern "C" fn get_stats(p: *const ClapPlugin, out: *mut Stats) {
    // SAFETY: as above, and the caller's out-pointer.
    let (me, out) = unsafe { (me(p), &mut *out) };
    let n = |a: &AtomicU32| a.load(Ordering::Relaxed);
    *out = Stats {
        entry_inits: n(&ENTRY_INITS),
        live: n(&LIVE),
        activates: n(&me.activates),
        deactivates: n(&me.deactivates),
        starts: n(&me.starts),
        stops: n(&me.stops),
        processes: n(&me.processes),
        callbacks: n(&me.callbacks),
        broken: n(&me.broken),
        notes_on: n(&me.notes_on),
        notes_off: n(&me.notes_off),
        offline: n(&me.offline),
        max_frames: n(&me.max_frames),
        frames: n(&me.frames),
        steady: n(&me.steady),
        params_set: n(&me.params_set),
        flushes: n(&me.flushes),
        overlaps: n(&me.overlaps),
        start_gain: f64::from_bits(me.start_gain.load(Ordering::Relaxed)),
    };
}

unsafe extern "C" fn params_count(p: *const ClapPlugin) -> u32 {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    me.params().len() as u32
}

/// `src` into the C string array `dst`, NUL-terminated.
fn copy_text(dst: &mut [c_char], src: &CStr) {
    let bytes = src.to_bytes_with_nul();
    let n = bytes.len().min(dst.len());
    for (d, &b) in dst.iter_mut().zip(&bytes[..n]) {
        *d = b as c_char;
    }
    dst[dst.len() - 1] = 0;
}

unsafe extern "C" fn params_info(p: *const ClapPlugin, index: u32, info: *mut ClapParamInfo) -> bool {
    // SAFETY: as above, and the host's room for one info.
    let (me, info) = unsafe { (me(p), &mut *info) };
    me.on_main();
    let Some(param) = me.params().get(index as usize) else { return false };
    let renamed = me.renamed.load(Ordering::Relaxed);
    info.id = param.id;
    info.flags = if renamed && param.id == HOLD { param.flags & !CLAP_PARAM_IS_HIDDEN } else { param.flags };
    info.cookie = null_mut();
    copy_text(&mut info.name, if renamed && param.id == GAIN { c"Volume" } else { param.name });
    copy_text(&mut info.module, param.module);
    info.min_value = param.min;
    info.max_value = param.max;
    info.default_value = param.default;
    true
}

unsafe extern "C" fn params_value(p: *const ClapPlugin, id: ClapId, out: *mut f64) -> bool {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    match me.value(id) {
        // SAFETY: the host's room for the value.
        Some(v) => unsafe { *out = v },
        None => return false,
    }
    true
}

unsafe extern "C" fn params_to_text(p: *const ClapPlugin, id: ClapId, v: f64, out: *mut c_char, n: u32) -> bool {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    let text = match id {
        KIND => ["soft", "hard", "wild"].get(v as usize).map(|s| s.to_string()),
        GAIN | LEVEL | METER | HOLD | OFFSET => Some(format!("{:.1} %", v * 100.0)),
        _ => None,
    };
    let Some(text) = text else { return false };
    let Ok(text) = std::ffi::CString::new(text) else { return false };
    // SAFETY: the host's buffer of `n` chars.
    copy_text(unsafe { std::slice::from_raw_parts_mut(out, n as usize) }, &text);
    true
}

unsafe extern "C" fn params_from_text(p: *const ClapPlugin, id: ClapId, text: *const c_char, out: *mut f64) -> bool {
    // SAFETY: as above, and a NUL-terminated text.
    let (me, text) = unsafe { (me(p), CStr::from_ptr(text).to_string_lossy()) };
    me.on_main();
    let v = match id {
        KIND => ["soft", "hard", "wild"].iter().position(|s| *s == text.trim()).map(|i| i as f64),
        _ => text.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0),
    };
    match v {
        // SAFETY: the host's room for the value.
        Some(v) => unsafe { *out = v },
        None => return false,
    }
    true
}

unsafe extern "C" fn params_flush(p: *const ClapPlugin, input: *const ClapInputEvents, _out: *const ClapOutputEvents) {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    // An active plugin flushes on the audio thread, not while it processes.
    if me.active.load(Ordering::Relaxed) {
        me.require(me.is_audio());
    } else {
        me.on_main();
    }
    me.flushes.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the host's list, valid during this call.
    let events = unsafe { &*input };
    let count = events.size.map_or(0, |f| unsafe { f(events) });
    for n in 0..count {
        // SAFETY: an index below the list's size.
        let e = unsafe { events.get.map_or(null(), |f| f(events, n)) };
        // SAFETY: the host's event, valid during this call.
        let ok = !e.is_null() && unsafe { (*e).kind } == CLAP_EVENT_PARAM_VALUE;
        me.require(ok);
        if ok {
            // SAFETY: a parameter change, by its kind.
            let e = unsafe { &*e.cast::<ClapEventParamValue>() };
            // One that needs processing never comes here while active.
            let processed =
                me.params().iter().any(|p| p.id == e.param_id && p.flags & CLAP_PARAM_REQUIRES_PROCESS != 0);
            me.require(!(processed && me.active.load(Ordering::Relaxed)));
            me.take_param(e);
        }
    }
}

static PARAMS: ClapPluginParams = ClapPluginParams {
    count: Some(params_count),
    get_info: Some(params_info),
    get_value: Some(params_value),
    value_to_text: Some(params_to_text),
    text_to_value: Some(params_from_text),
    flush: Some(params_flush),
};

unsafe extern "C" fn latency_get(p: *const ClapPlugin) -> u32 {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    me.require(me.active.load(Ordering::Relaxed));
    me.latency.load(Ordering::Relaxed)
}

static LATENCY_EXT: ClapPluginLatency = ClapPluginLatency { get: Some(latency_get) };

const STATE_MAGIC: &[u8; 4] = b"TVST";

unsafe extern "C" fn state_save(p: *const ClapPlugin, stream: *const ClapOStream) -> bool {
    // SAFETY: as above, and the host's stream.
    let (me, stream) = unsafe { (me(p), &*stream) };
    me.on_main();
    let Some(write) = stream.write else { return false };
    if mode() == Mode::SaveFail {
        return false;
    }
    SAVES.fetch_add(1, Ordering::Release);
    let mut bytes = STATE_MAGIC.to_vec();
    if mode() == Mode::BigState {
        bytes.resize(17 << 20, 7);
    }
    for v in &me.values[..me.params().len()] {
        bytes.extend_from_slice(&v.load(Ordering::Relaxed).to_le_bytes());
    }
    // A few bytes at a time, each part as far as the host took it.
    let mut at = 0;
    while at < bytes.len() {
        let part = &bytes[at..(at + 5).max(at + (1 << 20).min(bytes.len() / 4)).min(bytes.len())];
        // SAFETY: the host's stream, given bytes it may copy.
        let n = unsafe { write(stream, part.as_ptr().cast(), part.len() as u64) };
        if n <= 0 {
            // `bigstate` says it worked anyway.
            return mode() == Mode::BigState;
        }
        at += n as usize;
    }
    true
}

unsafe extern "C" fn state_load(p: *const ClapPlugin, stream: *const ClapIStream) -> bool {
    // SAFETY: as above, and the host's stream.
    let (me, stream) = unsafe { (me(p), &*stream) };
    me.on_main();
    let Some(read) = stream.read else { return false };
    if mode() == Mode::LoadFail {
        return false;
    }
    let mut bytes = Vec::new();
    loop {
        let mut part = [0u8; 3];
        // SAFETY: the host's stream, given room for three bytes.
        let n = unsafe { read(stream, part.as_mut_ptr().cast(), 3) };
        if !(0..=3).contains(&n) {
            return false;
        }
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&part[..n as usize]);
    }
    let values = me.params().len();
    if bytes.len() != 4 + 8 * values || &bytes[..4] != STATE_MAGIC {
        return false;
    }
    for (i, v) in bytes[4..].chunks_exact(8).enumerate() {
        me.values[i].store(u64::from_le_bytes(v.try_into().unwrap()), Ordering::Relaxed);
    }
    // SAFETY: the host's own function, on the main thread.
    if let Some(rescan) = me.host_params().and_then(|h| h.rescan) {
        unsafe { rescan(me.host, CLAP_PARAM_RESCAN_VALUES) };
    }
    true
}

static STATE: ClapPluginState = ClapPluginState { save: Some(state_save), load: Some(state_load) };

impl Instance {
    fn takes_notes(&self) -> bool {
        self.sine || mode() == Mode::Both
    }
}

unsafe extern "C" fn plugin_extension(p: *const ClapPlugin, id: *const c_char) -> *const c_void {
    // SAFETY: as above, and a NUL-terminated id.
    let (me, id) = unsafe { (me(p), CStr::from_ptr(id)) };
    if id == CLAP_EXT_AUDIO_PORTS {
        (&raw const AUDIO_PORTS).cast()
    } else if id == CLAP_EXT_NOTE_PORTS && me.takes_notes() {
        (&raw const NOTE_PORTS).cast()
    } else if id == CLAP_EXT_RENDER {
        (&raw const RENDER).cast()
    } else if id == CLAP_EXT_PARAMS {
        (&raw const PARAMS).cast()
    } else if id == CLAP_EXT_STATE {
        (&raw const STATE).cast()
    } else if id == CLAP_EXT_LATENCY && late() {
        (&raw const LATENCY_EXT).cast()
    } else if id == c"org.tinvo.test.stats" {
        (&raw const STATS_EXT).cast()
    } else {
        null()
    }
}

unsafe extern "C" fn plugin_on_main_thread(p: *const ClapPlugin) {
    // SAFETY: as above.
    let me = unsafe { me(p) };
    me.on_main();
    me.callbacks.fetch_add(1, Ordering::Relaxed);
    if mode() == Mode::ProcessErrors {
        me.ask_restart_and_callback();
    }
    if mode() == Mode::Dirty {
        let state = me.host_extension(CLAP_EXT_STATE).cast::<ClapHostState>();
        // SAFETY: the host's own extension, on the main thread.
        if let Some(dirty) = unsafe { state.as_ref() }.and_then(|h| h.mark_dirty) {
            unsafe { dirty(me.host) };
        }
    }
    if mode() == Mode::Meter && !me.renamed.swap(true, Ordering::Relaxed) {
        // After changing its mode itself: a new name and its hold shown,
        // which may change while it is active; and its hold no longer
        // what it was, for automation.
        if let Some(h) = me.host_params() {
            // SAFETY: the host's own functions, on the main thread.
            unsafe {
                if let Some(rescan) = h.rescan {
                    rescan(me.host, CLAP_PARAM_RESCAN_INFO);
                }
                if let Some(clear) = h.clear {
                    clear(me.host, HOLD, CLAP_PARAM_CLEAR_ALL);
                    clear(me.host, HOLD, CLAP_PARAM_CLEAR_AUTOMATIONS);
                }
            }
        }
    }
}

unsafe extern "C" fn plugin_count(_: *const ClapPluginFactory) -> u32 {
    DESCRIPTORS.0.len() as u32
}

unsafe extern "C" fn plugin_descriptor(_: *const ClapPluginFactory, index: u32) -> *const ClapPluginDescriptor {
    DESCRIPTORS.0.get(index as usize).map_or(null(), |d| d as *const _)
}

unsafe extern "C" fn create_plugin(
    _: *const ClapPluginFactory,
    host: *const ClapHost,
    id: *const c_char,
) -> *const ClapPlugin {
    if host.is_null() || id.is_null() {
        return null();
    }
    // SAFETY: the host passes a NUL-terminated id.
    let id = unsafe { CStr::from_ptr(id) };
    let sine = match id.to_bytes() {
        b"org.tinvo.test.gain" => false,
        b"org.tinvo.test.sine" => true,
        _ => return null(),
    };
    let desc = &DESCRIPTORS.0[usize::from(sine)];
    let table: &[Param] = if sine { &SINE_PARAMS } else { &GAIN_PARAMS };
    let values = std::array::from_fn(|i| AtomicU64::new(table.get(i).map_or(0.0, |p| p.default).to_bits()));
    let instance = Box::new(Instance {
        plugin: ClapPlugin {
            desc,
            plugin_data: null_mut(),
            init: Some(plugin_init),
            destroy: Some(plugin_destroy),
            activate: Some(plugin_activate),
            deactivate: Some(plugin_deactivate),
            start_processing: Some(plugin_start),
            stop_processing: Some(plugin_stop),
            reset: Some(plugin_reset),
            process: if mode() == Mode::Incomplete { None } else { Some(plugin_process) },
            get_extension: Some(plugin_extension),
            on_main_thread: Some(plugin_on_main_thread),
        },
        host,
        check: AtomicPtr::new(null_mut()),
        sine,
        main_thread: AtomicU64::new(0),
        active: AtomicBool::new(false),
        processing: AtomicBool::new(false),
        audio: UnsafeCell::new(Audio {
            sample_rate: 0.0,
            max_frames: 0,
            next_steady: 0,
            voices: [Voice::default(); VOICES],
            asked: false,
            delay: Vec::new(),
            delay_at: 0,
            told: false,
        }),
        max_frames: AtomicU32::new(0),
        activates: AtomicU32::new(0),
        deactivates: AtomicU32::new(0),
        starts: AtomicU32::new(0),
        stops: AtomicU32::new(0),
        processes: AtomicU32::new(0),
        callbacks: AtomicU32::new(0),
        broken: AtomicU32::new(0),
        notes_on: AtomicU32::new(0),
        notes_off: AtomicU32::new(0),
        offline: AtomicU32::new(0),
        frames: AtomicU32::new(0),
        steady: AtomicU32::new(0),
        values,
        params_set: AtomicU32::new(0),
        flushes: AtomicU32::new(0),
        overlaps: AtomicU32::new(0),
        start_gain: AtomicU64::new(0),
        latency: AtomicU32::new(0),
        renamed: AtomicBool::new(false),
    });
    LIVE.fetch_add(1, Ordering::Relaxed);
    Box::into_raw(instance).cast_const().cast()
}

static FACTORY: ClapPluginFactory = ClapPluginFactory {
    get_plugin_count: Some(plugin_count),
    get_plugin_descriptor: Some(plugin_descriptor),
    create_plugin: Some(create_plugin),
};

unsafe extern "C" fn init(plugin_path: *const c_char) -> bool {
    if plugin_path.is_null() {
        return false;
    }
    // SAFETY: the host passes a NUL-terminated path.
    let path = unsafe { CStr::from_ptr(plugin_path) }.to_string_lossy().into_owned();
    ENTRY_INITS.fetch_add(1, Ordering::Relaxed);
    *PATH.lock().unwrap() = Some(PathBuf::from(&path));
    let set = |m: Mode| MODE.store(m as u8, Ordering::Relaxed);
    match Path::new(&path).file_stem().and_then(|s| s.to_str()).unwrap_or("") {
        "crash" => std::process::abort(),
        "hang" => loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        },
        "refuse" => false,
        "noisy" => {
            let line = "noise ".repeat(170);
            for _ in 0..4096 {
                println!("{line}");
                eprintln!("{line}");
            }
            true
        }
        "nofactory" => {
            set(Mode::NoFactory);
            true
        }
        "helper" => {
            let dir = Path::new(&path).parent().unwrap_or(Path::new("."));
            let mut shell = if cfg!(windows) {
                let mut c = std::process::Command::new("cmd");
                c.args(["/C", "ping -n 4 127.0.0.1 >NUL && type NUL > helper.marker"]);
                c
            } else {
                let mut c = std::process::Command::new("sh");
                c.args(["-c", "sleep 3; touch helper.marker"]);
                c
            };
            shell.current_dir(dir).spawn().is_ok()
        }
        "deinitcrash" => {
            set(Mode::CrashOnDeinit);
            true
        }
        "midi" => {
            set(Mode::Midi);
            true
        }
        "sidechain" => {
            set(Mode::Sidechain);
            true
        }
        "noactivate" => {
            set(Mode::NoActivate);
            true
        }
        "nostart" => {
            set(Mode::NoStart);
            true
        }
        "hardrt" => {
            set(Mode::HardRealtime);
            true
        }
        "offlinerefuse" => {
            set(Mode::OfflineRefuse);
            true
        }
        "restart" => {
            set(Mode::Restart);
            true
        }
        "incomplete" => {
            set(Mode::Incomplete);
            true
        }
        "lifecycle" => {
            set(Mode::Lifecycle);
            true
        }
        "initfail" => {
            set(Mode::InitFail);
            true
        }
        "nomain" => {
            set(Mode::NoMain);
            true
        }
        "procerror" => {
            set(Mode::ProcessError);
            true
        }
        "latency" => {
            set(Mode::Latency);
            true
        }
        "relatency" => {
            set(Mode::Relatency);
            true
        }
        "latefail" => {
            set(Mode::LateFail);
            true
        }
        "meter" => {
            set(Mode::Meter);
            true
        }
        "bigstate" => {
            set(Mode::BigState);
            true
        }
        "savefail" => {
            set(Mode::SaveFail);
            true
        }
        "loadfail" => {
            set(Mode::LoadFail);
            true
        }
        "dirty" => {
            set(Mode::Dirty);
            true
        }
        "procerrors" => {
            set(Mode::ProcessErrors);
            true
        }
        "both" => {
            set(Mode::Both);
            true
        }
        "tail" => {
            set(Mode::Tail);
            true
        }
        "leak" => {
            set(Mode::Leak);
            true
        }
        _ => true,
    }
}

unsafe extern "C" fn deinit() {
    match mode() {
        Mode::CrashOnDeinit => std::process::abort(),
        Mode::Lifecycle
        | Mode::Leak
        | Mode::Incomplete
        | Mode::InitFail
        | Mode::NoMain
        | Mode::NoActivate
        | Mode::ProcessError => {
            let path = PATH.lock().unwrap().clone().unwrap_or_default();
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let report = format!(
                "inits={} live={} broken={}\n",
                ENTRY_INITS.load(Ordering::Relaxed),
                LIVE.load(Ordering::Relaxed),
                BROKEN.load(Ordering::Relaxed)
            );
            let _ = std::fs::write(path.with_file_name(format!("{stem}.deinit")), report);
        }
        _ => {}
    }
}

unsafe extern "C" fn get_factory(factory_id: *const c_char) -> *const c_void {
    if factory_id.is_null() || mode() == Mode::NoFactory {
        return null();
    }
    // SAFETY: the host passes a NUL-terminated id.
    if unsafe { CStr::from_ptr(factory_id) } == CLAP_PLUGIN_FACTORY_ID { (&raw const FACTORY).cast() } else { null() }
}

/// The symbol a CLAP host looks for.
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static clap_entry: ClapPluginEntry = ClapPluginEntry {
    clap_version: CLAP_VERSION,
    init: Some(init),
    deinit: Some(deinit),
    get_factory: Some(get_factory),
};
