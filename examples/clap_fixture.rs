//! A CLAP plugin file for TINVO's tests (`tests/clap.rs`), built as a
//! shared library by `cargo test`. It is not an example to copy.
//!
//! What it does depends on the name it is loaded under, the file name
//! without its extension:
//!
//! - `crash` ends the process in `init`;
//! - `hang` never returns from `init`;
//! - `refuse` fails its `init`;
//! - `noisy` writes a few megabytes to stdout and stderr in `init`;
//! - `nofactory` has no plugin factory;
//! - `deinitcrash` ends the process in `deinit`;
//! - `helper` starts a shell that writes `helper.marker` beside the file
//!   three seconds later, unless it is ended first;
//! - any other offers a gain effect and a sine instrument, plus a third
//!   descriptor with no name, which a host must pass over.

use std::ffi::{CStr, c_char, c_void};
use std::path::Path;
use std::ptr::null;
use std::sync::atomic::{AtomicU8, Ordering};
use tinvo::clap::ffi::{
    CLAP_PLUGIN_FACTORY_ID, CLAP_VERSION, ClapHost, ClapPlugin, ClapPluginDescriptor, ClapPluginEntry,
    ClapPluginFactory,
};

const NORMAL: u8 = 0;
const NO_FACTORY: u8 = 1;
const CRASH_ON_DEINIT: u8 = 2;
static MODE: AtomicU8 = AtomicU8::new(NORMAL);

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

unsafe extern "C" fn plugin_count(_: *const ClapPluginFactory) -> u32 {
    DESCRIPTORS.0.len() as u32
}

unsafe extern "C" fn plugin_descriptor(_: *const ClapPluginFactory, index: u32) -> *const ClapPluginDescriptor {
    DESCRIPTORS.0.get(index as usize).map_or(null(), |d| d as *const _)
}

unsafe extern "C" fn create_plugin(
    _: *const ClapPluginFactory,
    _: *const ClapHost,
    _: *const c_char,
) -> *const ClapPlugin {
    null()
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
            MODE.store(NO_FACTORY, Ordering::Relaxed);
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
            MODE.store(CRASH_ON_DEINIT, Ordering::Relaxed);
            true
        }
        _ => true,
    }
}

unsafe extern "C" fn deinit() {
    if MODE.load(Ordering::Relaxed) == CRASH_ON_DEINIT {
        std::process::abort();
    }
}

unsafe extern "C" fn get_factory(factory_id: *const c_char) -> *const c_void {
    if factory_id.is_null() || MODE.load(Ordering::Relaxed) == NO_FACTORY {
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
