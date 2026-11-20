//! What the tests of CLAP hosting share: the test plugin, built by
//! `cargo test` as an example, put where a test wants it, and made into an
//! instance; and the probe of its window.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tinvo::clap::ffi::ClapPlugin;
use tinvo::clap::host::{Instance, Mode, PluginHost, Role};

pub const WORKER: &str = env!("CARGO_BIN_EXE_tinvo");

/// The test plugin `cargo test` built.
pub fn fixture() -> PathBuf {
    let name = if cfg!(windows) {
        "clap_fixture.dll"
    } else if cfg!(target_os = "macos") {
        "libclap_fixture.dylib"
    } else {
        "libclap_fixture.so"
    };
    let starts = [PathBuf::from(WORKER), std::env::current_exe().unwrap()];
    starts
        .iter()
        .flat_map(|s| s.ancestors().skip(1).take(6))
        .map(|dir| dir.join("examples").join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} is not built: run cargo test, or cargo build --example clap_fixture"))
}

pub fn temp_dir(name: &str) -> PathBuf {
    static SWEEP: std::sync::Once = std::sync::Once::new();
    SWEEP.call_once(sweep_earlier_runs);
    let d = std::env::temp_dir().join(format!("tinvo-clap-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Removes the folders earlier runs left. A test's plugin file stays loaded
/// until its process ends, and on Windows that keeps the file, and its
/// folder, from being removed when the test ends. An hour old, a folder is
/// no other run's still going.
fn sweep_earlier_runs() {
    let own = format!("-{}", std::process::id());
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("tinvo-clap-it-") || name.ends_with(&own) {
            continue;
        }
        let age = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if age.is_some_and(|a| a > Duration::from_secs(3600)) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Puts the test plugin in `dir` as `stem.clap`: a file, or on macOS a
/// bundle.
pub fn install(dir: &Path, stem: &str) -> PathBuf {
    let path = dir.join(format!("{stem}.clap"));
    if cfg!(target_os = "macos") {
        let contents = path.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::copy(fixture(), contents.join("MacOS").join(stem)).unwrap();
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             <key>CFBundleExecutable</key><string>{stem}</string>\n\
             <key>CFBundleIdentifier</key><string>org.tinvo.test.{stem}</string>\n\
             <key>CFBundlePackageType</key><string>BNDL</string>\n\
             </dict></plist>\n"
        );
        std::fs::write(contents.join("Info.plist"), plist).unwrap();
    } else {
        std::fs::copy(fixture(), &path).unwrap();
    }
    path
}

pub const GAIN: &str = "org.tinvo.test.gain";
pub const RATE: f64 = 48_000.0;

/// Creates `id` from `file` for `role`, in real time at 48 kHz in calls of
/// up to 64 frames.
pub fn create(host: &mut PluginHost, file: &Path, id: &str, role: Role) -> Result<Instance, String> {
    // SAFETY: the test plugin, loaded only through plugin hosts here.
    unsafe { host.create(file, id, role, Mode::Realtime, RATE, 64) }
}

/// What `org.tinvo.test.gui` reports; the test plugin has the same.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GuiStats {
    pub timer_calls: u32,
    pub fd_calls: u32,
    pub fd: i32,
    pub width: u32,
    pub height: u32,
    pub broken: u32,
    pub parent: u64,
}

#[repr(C)]
pub struct GuiTestExt {
    log: unsafe extern "C" fn(plugin: *const ClapPlugin, out: *mut u8, cap: usize) -> usize,
    stats: unsafe extern "C" fn(plugin: *const ClapPlugin, out: *mut GuiStats),
    poke: unsafe extern "C" fn(plugin: *const ClapPlugin, what: u32, a: u32, b: u32) -> bool,
}

/// The test plugin's window and the calls to it, through its own test
/// extension, from any thread.
#[derive(Clone, Copy)]
pub struct GuiProbe {
    plugin: *const ClapPlugin,
    ext: *const GuiTestExt,
}

// SAFETY: the test plugin's probe functions may be called from any thread
// while its instance lives.
unsafe impl Send for GuiProbe {}
unsafe impl Sync for GuiProbe {}

impl GuiProbe {
    pub fn of(i: &Instance) -> GuiProbe {
        let ext = i.extension(c"org.tinvo.test.gui").cast::<GuiTestExt>();
        assert!(!ext.is_null(), "the test plugin's window probe");
        GuiProbe { plugin: i.as_ptr(), ext }
    }

    /// The calls its window had, a line each.
    pub fn log(&self) -> Vec<String> {
        let mut buf = vec![0u8; 1 << 16];
        // SAFETY: the test plugin's extension, given its own instance and
        // the buffer's capacity.
        let n = unsafe { ((*self.ext).log)(self.plugin, buf.as_mut_ptr(), buf.len()) };
        String::from_utf8_lossy(&buf[..n]).lines().map(str::to_string).collect()
    }

    pub fn stats(&self) -> GuiStats {
        let mut s = GuiStats::default();
        // SAFETY: as above.
        unsafe { ((*self.ext).stats)(self.plugin, &mut s) };
        s
    }

    /// Makes the plugin call its host's `clap.gui` (see the test plugin).
    pub fn poke(&self, what: u32, a: u32, b: u32) -> bool {
        // SAFETY: as above.
        unsafe { ((*self.ext).poke)(self.plugin, what, a, b) }
    }
}
