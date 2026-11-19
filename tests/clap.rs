//! Real CLAP plugin files: the test plugin in `examples/clap_fixture.rs`,
//! copied under the names that make it crash, hang or fail, read by TINVO's
//! own `scan-clap` child processes; and its instances played in this
//! process, on their own and in the engine.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tinvo::app::{App, EngineLink, Mode as EditorMode, PluginFor, PluginPicker};
use tinvo::clap::ffi::{
    CLAP_PARAM_CLEAR_ALL, CLAP_PARAM_CLEAR_AUTOMATIONS, CLAP_PARAM_IS_STEPPED, CLAP_PARAM_REQUIRES_PROCESS,
    CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_VALUES, ClapPlugin,
};
use tinvo::clap::host::{
    Instance, MAX_EVENTS, MAX_LATENCY_SECONDS, Mode, ParamInfo, PluginHost, Request, Role, max_latency,
};
use tinvo::clap::scan::{self, Catalog, Options};
use tinvo::clap::{PluginRef, Roles};
use tinvo::engine::{
    ChainId, Command as EngineCommand, Edit, Engine, LaneTarget, PluginAt, PluginPlace, RETIRE_QUEUE, Retired, Shared,
    Step,
};
use tinvo::insert::{EffectKind, Insert};
use tinvo::instrument::InstrumentKind;
use tinvo::project::Project;
use tinvo::render::{CatalogPlugins, NoPlugins, PluginSource, RenderOptions, render_into};
use tinvo::rt::spsc::{self, Consumer};
use tinvo::tui::Key;

const WORKER: &str = env!("CARGO_BIN_EXE_tinvo");

/// The test plugin `cargo test` built.
fn fixture() -> PathBuf {
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

fn temp_dir(name: &str) -> PathBuf {
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
fn install(dir: &Path, stem: &str) -> PathBuf {
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

fn options(per_file: Duration) -> Options {
    Options { worker: WORKER.into(), per_file, budget: Duration::from_secs(120), rescan: false }
}

/// The plugins a catalog lists, as (file name, id, roles).
fn listed(catalog: &Catalog) -> Vec<(String, String, Roles)> {
    catalog
        .plugins()
        .iter()
        .map(|(f, p)| (f.file_name().unwrap().to_string_lossy().into_owned(), p.id.clone(), p.roles))
        .collect()
}

fn failure<'a>(catalog: &'a Catalog, stem: &str) -> &'a str {
    let name = format!("{stem}.clap");
    catalog
        .failures()
        .into_iter()
        .find(|(f, _)| f.file_name().is_some_and(|n| n == name.as_str()))
        .map(|(_, why)| why)
        .unwrap_or_else(|| panic!("{name} did not fail: {catalog:?}"))
}

#[test]
fn a_plugin_file_s_descriptors_are_listed() {
    let dir = temp_dir("list");
    install(&dir, "fixture");
    let catalog = scan::scan(std::slice::from_ref(&dir), None, &options(Duration::from_secs(30)));
    assert_eq!(
        listed(&catalog),
        [
            ("fixture.clap".into(), "org.tinvo.test.gain".into(), Roles::EFFECT),
            ("fixture.clap".into(), "org.tinvo.test.sine".into(), Roles::INSTRUMENT),
        ],
        "the descriptor with no name is passed over"
    );
    let (_, gain) = catalog.plugins()[0];
    assert_eq!(
        (gain.name.as_str(), gain.vendor.as_str(), gain.version.as_str()),
        ("Test Gain", "TINVO tests", "1.0.0")
    );
    assert_eq!(gain.features, ["audio-effect", "stereo"]);
    assert!(catalog.failures().is_empty() && catalog.left == 0 && catalog.notes.is_empty(), "{catalog:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_that_crashes_or_fails_is_named_with_why() {
    let dir = temp_dir("fail");
    for stem in ["crash", "refuse", "noisy", "nofactory", "deinitcrash"] {
        install(&dir, stem);
    }
    // On macOS, where plugins are folders, this file is not looked at.
    std::fs::write(dir.join("text.clap"), "not a library").unwrap();
    let catalog = scan::scan(std::slice::from_ref(&dir), None, &options(Duration::from_secs(30)));
    assert!(failure(&catalog, "crash").starts_with("crashed while loading ("), "{catalog:?}");
    assert_eq!(failure(&catalog, "refuse"), "refused to start (its init failed)");
    assert_eq!(failure(&catalog, "nofactory"), "has no plugin factory");
    assert!(failure(&catalog, "deinitcrash").starts_with("crashed while loading ("), "a crash in deinit counts");
    if !cfg!(target_os = "macos") {
        assert!(failure(&catalog, "text").starts_with("cannot be loaded ("), "{catalog:?}");
    }
    let names: Vec<String> = listed(&catalog).into_iter().map(|(f, id, _)| format!("{f} {id}")).collect();
    assert_eq!(names, ["noisy.clap org.tinvo.test.gain", "noisy.clap org.tinvo.test.sine"], "output is not read");
    assert_eq!(catalog.left, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_that_hangs_is_ended_and_named() {
    let dir = temp_dir("hang");
    install(&dir, "hang");
    let catalog = scan::scan(std::slice::from_ref(&dir), None, &options(Duration::from_secs(2)));
    assert_eq!(failure(&catalog, "hang"), "did not finish loading within 2 s");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_helper_a_plugin_starts_ends_with_its_scan() {
    let dir = temp_dir("helper");
    install(&dir, "helper");
    let catalog = scan::scan(std::slice::from_ref(&dir), None, &options(Duration::from_secs(30)));
    assert_eq!(listed(&catalog).len(), 2, "{catalog:?}");
    std::thread::sleep(Duration::from_secs(6));
    assert!(!dir.join("helper.marker").exists(), "the helper outlived the scan");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_the_scan_s_time_cuts_short_is_left_for_next_time() {
    let dir = temp_dir("cut");
    let cache = dir.join("plugins.txt");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    install(&plugins, "hang");
    let short = Options { budget: Duration::from_secs(1), ..options(Duration::from_secs(30)) };
    let started = std::time::Instant::now();
    let catalog = scan::scan(&[plugins], Some(&cache), &short);
    assert!(started.elapsed() < Duration::from_secs(10), "the child got the time left, not its own");
    assert_eq!((catalog.entries.len(), catalog.left), (0, 1), "not taken for a hang: {catalog:?}");
    assert!(scan::read_cache(&cache).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_scan_called_off_ends_its_child_and_leaves_the_file_for_next_time() {
    let dir = temp_dir("called-off");
    let cache = dir.join("plugins.txt");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    install(&plugins, "hang");
    // A file the scan would come to next.
    let next = dir.join("next");
    std::fs::create_dir_all(&next).unwrap();
    install(&next, "fixture");
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let caller = cancel.clone();
    let off = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        caller.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let started = std::time::Instant::now();
    let catalog = scan::scan_until(&[plugins, next], Some(&cache), &options(Duration::from_secs(30)), &cancel);
    off.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "the child was not waited for");
    assert_eq!((catalog.entries.len(), catalog.left), (0, 2), "not taken for a hang, none read after: {catalog:?}");
    assert!(scan::read_cache(&cache).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_cache_is_used_until_a_file_changes() {
    let dir = temp_dir("cache");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let cache = dir.join("state").join("plugins.txt");
    let file = install(&plugins, "fixture");
    install(&plugins, "refuse");
    let roots = [plugins.clone()];
    let first = scan::scan(&roots, Some(&cache), &options(Duration::from_secs(30)));
    assert_eq!(listed(&first).len(), 2);
    assert!(cache.is_file());

    // A scanner that cannot start shows whether a file is read again.
    let broken = Options { worker: dir.join("no-such-program"), ..options(Duration::from_secs(30)) };
    let again = scan::scan(&roots, Some(&cache), &broken);
    assert_eq!(listed(&again), listed(&first), "both files come from the cache");
    assert_eq!(failure(&again, "refuse"), "refused to start (its init failed)", "so do failures");
    assert_eq!((again.left, again.notes.len()), (0, 0), "{again:?}");

    // A changed file is read again; one that cannot be is left for next time.
    let stamp = if cfg!(target_os = "macos") { file.join("Contents").join("Info.plist") } else { file.clone() };
    let later = std::fs::metadata(&stamp).unwrap().modified().unwrap() + Duration::from_secs(10);
    std::fs::File::options().write(true).open(&stamp).unwrap().set_modified(later).unwrap();
    let changed = scan::scan(&roots, Some(&cache), &broken);
    assert_eq!(changed.left, 1);
    assert!(listed(&changed).is_empty());
    assert!(changed.notes.iter().any(|n| n.starts_with("the scanner could not be started")), "{changed:?}");
    let read = scan::scan(&roots, Some(&cache), &options(Duration::from_secs(30)));
    assert_eq!((listed(&read).len(), read.left), (2, 0), "{read:?}");

    // --rescan reads every file again.
    let rescan = scan::scan(&roots, Some(&cache), &Options { rescan: true, ..broken.clone() });
    assert_eq!(rescan.left, 2);

    // A file gone is gone from the cache.
    let read = scan::scan(&roots, Some(&cache), &options(Duration::from_secs(30)));
    assert_eq!(read.entries.len(), 2);
    remove(&file);
    let gone = scan::scan(&roots, Some(&cache), &broken);
    assert_eq!(gone.entries.len(), 1);
    assert_eq!(scan::read_cache(&cache).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

fn remove(path: &Path) {
    if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }.unwrap();
}

#[test]
fn files_past_the_scan_s_time_are_left_for_next_time() {
    let dir = temp_dir("budget");
    let cache = dir.join("plugins.txt");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    install(&plugins, "fixture");
    let roots = [plugins];
    let none = Options { budget: Duration::ZERO, ..options(Duration::from_secs(30)) };
    let first = scan::scan(&roots, Some(&cache), &none);
    assert_eq!((first.entries.len(), first.left), (0, 1));
    assert!(scan::read_cache(&cache).is_empty(), "nothing is cached for it");
    let next = scan::scan(&roots, Some(&cache), &options(Duration::from_secs(30)));
    assert_eq!((listed(&next).len(), next.left), (2, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_same_plugin_in_two_files_is_listed_once() {
    let dir = temp_dir("dup");
    install(&dir, "a");
    install(&dir, "b");
    let catalog = scan::scan(std::slice::from_ref(&dir), None, &options(Duration::from_secs(30)));
    let files: Vec<String> = listed(&catalog).into_iter().map(|(f, _, _)| f).collect();
    assert_eq!(files, ["a.clap", "a.clap"]);
    let dups: Vec<(&str, String)> = catalog
        .duplicates()
        .into_iter()
        .map(|(id, _, other)| (id, other.file_name().unwrap().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(dups, [("org.tinvo.test.gain", "b.clap".into()), ("org.tinvo.test.sine", "b.clap".into())]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tinvo_plugins_lists_what_clap_path_holds() {
    let dir = temp_dir("cli");
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    install(&a, "fixture");
    install(&b, "refuse");
    let home = dir.join("home");
    let run = |args: &[&str]| {
        let out = Command::new(WORKER)
            .arg("plugins")
            .args(args)
            .env("CLAP_PATH", std::env::join_paths([&a, &b]).unwrap())
            .env("HOME", &home)
            .env("XDG_STATE_HOME", home.join("state"))
            .env("LOCALAPPDATA", home.join("local"))
            .env("COMMONPROGRAMFILES", home.join("common"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    // Plugins installed in the system's own folders may be listed too.
    for text in [run(&[]), run(&["--rescan"])] {
        assert!(text.contains("Test Gain 1.0.0 by TINVO tests (effect)\n    org.tinvo.test.gain\n"), "{text}");
        assert!(text.contains("Test Sine 1.0.0 by TINVO tests (instrument)\n    org.tinvo.test.sine\n"), "{text}");
        assert!(text.contains("\nNot loaded:\n"), "{text}");
        assert!(text.contains("refuse.clap\n    refused to start (its init failed)\n"), "{text}");
    }
    let state = if cfg!(windows) {
        home.join("local")
    } else if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        home.join("state")
    };
    let cached: Vec<PathBuf> =
        scan::read_cache(&state.join("tinvo").join("plugins.txt")).into_iter().map(|e| e.path).collect();
    assert!(cached.contains(&a.join("fixture.clap")) && cached.contains(&b.join("refuse.clap")), "{cached:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// Playing plugins in this process: the test plugin's instances, which
// check every call against CLAP's rules and count what breaks one. CLAP's
// main thread is the program's on Windows and macOS; the test plugin uses
// nothing of the system's that cares, so each test's own thread stands in
// for it.

/// What the test plugin's `org.tinvo.test.stats` reports; the plugin has
/// the same.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Stats {
    entry_inits: u32,
    live: u32,
    activates: u32,
    deactivates: u32,
    starts: u32,
    stops: u32,
    processes: u32,
    callbacks: u32,
    broken: u32,
    notes_on: u32,
    notes_off: u32,
    offline: u32,
    max_frames: u32,
    frames: u32,
    steady: u32,
    params_set: u32,
    flushes: u32,
    overlaps: u32,
    start_gain: f64,
}

#[repr(C)]
struct StatsExt {
    get: unsafe extern "C" fn(plugin: *const ClapPlugin, out: *mut Stats),
    probe: unsafe extern "C" fn(plugin: *const ClapPlugin) -> u32,
}

fn stats(i: &Instance) -> Stats {
    let ext = i.extension(c"org.tinvo.test.stats").cast::<StatsExt>();
    assert!(!ext.is_null(), "the test plugin's stats");
    let mut s = Stats::default();
    // SAFETY: the test plugin's extension, given its own instance.
    unsafe { ((*ext).get)(i.as_ptr(), &mut s) };
    s
}

/// What the host's `clap.thread-check` tells the plugin of this thread, as
/// (main, audio).
fn thread_check(i: &Instance) -> (bool, bool) {
    let ext = i.extension(c"org.tinvo.test.stats").cast::<StatsExt>();
    // SAFETY: as in `stats`.
    let bits = unsafe { ((*ext).probe)(i.as_ptr()) };
    (bits & 1 != 0, bits & 2 != 0)
}

const GAIN: &str = "org.tinvo.test.gain";
const SINE: &str = "org.tinvo.test.sine";
const RATE: f64 = 48_000.0;

/// Creates `id` from `file` for `role`, in real time at 48 kHz in calls of
/// up to 64 frames.
fn create(host: &mut PluginHost, file: &Path, id: &str, role: Role) -> Result<Instance, String> {
    // SAFETY: the test plugin, loaded only through plugin hosts here.
    unsafe { host.create(file, id, role, Mode::Realtime, RATE, 64) }
}

/// Interleaved stereo: left a ramp, right its negative, so the channels
/// cannot be mistaken for each other.
fn ramp(frames: usize) -> Vec<f32> {
    (0..frames).flat_map(|i| [i as f32 / 100.0, -(i as f32) / 100.0]).collect()
}

fn halved(frames: usize) -> Vec<f32> {
    ramp(frames).iter().map(|x| x * 0.5).collect()
}

#[test]
fn an_effect_plays_on_the_audio_thread_and_ends_on_the_main_one() {
    let dir = temp_dir("effect");
    let file = install(&dir, "effect");
    let mut host = PluginHost::new();
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    assert!(gain.is_playing());
    assert_eq!(gain.layout().inputs, [2]);
    assert_eq!(gain.layout().outputs, [2]);
    assert_eq!(gain.layout().notes, None);
    std::thread::scope(|s| {
        s.spawn(|| {
            // Odd sizes, and one past the largest call, which is split.
            for frames in [1, 31, 64, 200, 7] {
                let mut io = ramp(frames);
                assert!(gain.process(&mut io));
                assert_eq!(io, halved(frames), "{frames} frames");
            }
            gain.stop();
        });
    });
    let s = stats(&gain);
    assert_eq!(s.broken, 0, "{s:?}");
    assert_eq!((s.activates, s.starts, s.stops, s.deactivates), (1, 1, 1, 0), "{s:?}");
    assert_eq!((s.processes, s.frames, s.max_frames), (8, 303, 64), "{s:?}");
    // Steady time past what it can count becomes unknown, and stays so.
    gain.skip(i64::MAX as usize - 400);
    for _ in 0..3 {
        let mut io = ramp(64);
        assert!(gain.process(&mut io));
        gain.skip(usize::MAX);
    }
    gain.stop();
    assert_eq!(stats(&gain).broken, 0);
    drop(gain);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_instrument_plays_notes_from_the_start_of_the_next_call() {
    let dir = temp_dir("instrument");
    let file = install(&dir, "instrument");
    let mut host = PluginHost::new();
    let mut sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    assert_eq!(sine.layout().notes, Some((0, true)), "CLAP's own notes where they are taken");
    let mut io = vec![1.0; 2 * 16];
    assert!(sine.process(&mut io));
    assert!(io.iter().all(|&x| x == 0.0), "an instrument's output replaces what was there");
    assert_eq!(sine.note_on(60, f32::NAN), None, "no velocity, no note");
    assert_eq!(sine.queued(), 0);
    let note = sine.note_on(69, 0.8).unwrap();
    assert!(sine.process(&mut io));
    assert_eq!((io[0], io[1]), (0.25, 0.125), "from the call's first frame, the right half the left");
    for f in io.chunks_exact(2) {
        assert_eq!(f[1], 0.5 * f[0]);
    }
    sine.note_off(note);
    sine.note_off(note);
    assert!(sine.process(&mut io));
    assert!(io.iter().all(|&x| x == 0.0), "released, once");
    // Two notes on one key are two notes.
    let (a, b) = (sine.note_on(60, 1.0).unwrap(), sine.note_on(60, 1.0).unwrap());
    assert_ne!(a, b);
    assert!(sine.process(&mut io));
    assert_eq!(io[0], 0.5, "two voices");
    sine.note_off(a);
    sine.note_off(b);
    assert!(sine.process(&mut io));
    let s = stats(&sine);
    assert_eq!((s.notes_on, s.notes_off, s.broken), (3, 3, 0), "{s:?}");
    sine.stop();
    drop(sine);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_midi_instrument_gets_one_note_a_key() {
    let dir = temp_dir("midi");
    let file = install(&dir, "midi");
    let mut host = PluginHost::new();
    let mut sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    assert_eq!(sine.layout().notes, Some((0, false)));
    let mut io = vec![0.0; 2 * 8];
    let a = sine.note_on(60, 0.5).unwrap();
    let b = sine.note_on(60, 0.5).unwrap();
    assert!(sine.process(&mut io));
    assert_eq!(io[0], 0.25, "one voice: the first ended as the second began");
    sine.note_off(a);
    assert!(sine.process(&mut io));
    assert_ne!(io[0], 0.0, "the first one's release is dropped");
    sine.note_off(b);
    assert!(sine.process(&mut io));
    assert!(io.iter().all(|&x| x == 0.0));
    let s = stats(&sine);
    assert_eq!((s.notes_on, s.notes_off, s.broken), (2, 2, 0), "{s:?}");
    sine.stop();
    drop(sine);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_note_played_can_be_released() {
    let dir = temp_dir("full");
    let file = install(&dir, "full");
    let mut host = PluginHost::new();
    let mut sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let mut io = vec![0.0; 2 * 4];
    let mut notes = Vec::new();
    while let Some(n) = sine.note_on((notes.len() % 128) as u8, 1.0) {
        notes.push(n);
    }
    assert_eq!(notes.len(), MAX_EVENTS / 2, "in one call, each note keeps room for its release");
    sine.release_all();
    assert!(sine.process(&mut io));
    let s = stats(&sine);
    assert_eq!((s.notes_on, s.notes_off, s.broken), (256, 256, 0), "{s:?}");
    // Held across calls, they keep their room.
    let mut held = Vec::new();
    for _ in 0..20 {
        while let Some(n) = sine.note_on((held.len() % 128) as u8, 1.0) {
            held.push(n);
        }
        assert!(sine.process(&mut io));
    }
    assert_eq!(held.len(), MAX_EVENTS - 1);
    for n in held {
        sine.note_off(n);
    }
    assert!(sine.process(&mut io));
    let s = stats(&sine);
    assert_eq!((s.notes_on, s.notes_off, s.broken), (767, 767, 0), "{s:?}");
    sine.stop();
    drop(sine);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_audio_port_gets_buffers() {
    let dir = temp_dir("sidechain");
    let file = install(&dir, "sidechain");
    let mut host = PluginHost::new();
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    assert_eq!((gain.layout().inputs.as_slice(), gain.layout().outputs.as_slice()), (&[2, 2][..], &[2, 1][..]));
    let mut io = ramp(64);
    assert!(gain.process(&mut io));
    assert_eq!(io, halved(64), "the main ports are the ones heard");
    assert_eq!(stats(&gain).broken, 0, "the second input is silent");
    gain.stop();
    drop(gain);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_that_cannot_play_a_role_or_start_says_why() {
    let dir = temp_dir("refusals");
    let mut host = PluginHost::new();
    let normal = install(&dir, "roles");
    let err = |r: Result<Instance, String>| r.expect_err("refused");
    assert_eq!(err(create(&mut host, &normal, SINE, Role::Effect)), "has no mono or stereo main input");
    assert_eq!(err(create(&mut host, &normal, GAIN, Role::Instrument)), "takes no notes");
    assert_eq!(
        err(create(&mut host, &normal, "org.tinvo.test.unnamed", Role::Effect)),
        "has no plugin 'org.tinvo.test.unnamed' (or could not create it)"
    );
    let file = install(&dir, "noactivate");
    assert_eq!(err(create(&mut host, &file, GAIN, Role::Effect)), "could not start at 48000 Hz");
    let file = install(&dir, "incomplete");
    assert_eq!(err(create(&mut host, &file, GAIN, Role::Effect)), "is an incomplete plugin");
    let file = install(&dir, "refuse");
    assert_eq!(err(create(&mut host, &file, GAIN, Role::Effect)), "refused to start (its init failed)");

    // One that cannot start processing passes its input, and says so once.
    let file = install(&dir, "nostart");
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let mut io = ramp(16);
    assert!(!gain.process(&mut io));
    assert!(!gain.process(&mut io));
    assert_eq!(io, ramp(16), "passed as it was");
    assert!(!gain.is_playing());
    assert_eq!(host.poll(), [Request::Failed(gain.token())]);
    assert_eq!(host.poll(), []);
    let s = stats(&gain);
    assert_eq!((s.starts, s.processes, s.broken), (0, 0, 0), "{s:?}");
    let mut sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let mut io = vec![1.0; 8];
    assert!(!sine.process(&mut io));
    assert!(io.iter().all(|&x| x == 0.0), "an instrument that cannot start is silent");
    drop((gain, sine));
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rendering_a_file_asks_for_offline_processing() {
    let dir = temp_dir("offline");
    let mut host = PluginHost::new();
    let offline = |host: &mut PluginHost, file: &Path| {
        // SAFETY: as in `create`.
        unsafe { host.create(file, GAIN, Role::Effect, Mode::Offline, RATE, 64) }
    };
    let file = install(&dir, "offline");
    let mut gain = offline(&mut host, &file).unwrap();
    assert_eq!(stats(&gain).offline, 1);
    // A file rendered on the main thread: it plays both roles.
    let mut io = ramp(64);
    assert!(gain.process(&mut io));
    gain.stop();
    assert_eq!(stats(&gain).broken, 0);
    let realtime = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    assert_eq!(stats(&realtime).offline, 0);
    let file = install(&dir, "hardrt");
    assert_eq!(offline(&mut host, &file).err().unwrap(), "can only run in real time, not render a file");
    let file = install(&dir, "offlinerefuse");
    assert_eq!(offline(&mut host, &file).err().unwrap(), "refused to render offline");
    let refusing = create(&mut host, &file, GAIN, Role::Effect).expect("in real time it plays");
    drop((gain, realtime, refusing));
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_restart_and_a_main_thread_callback_are_served() {
    let dir = temp_dir("restart");
    let file = install(&dir, "restart");
    let mut host = PluginHost::new();
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let mut io = ramp(32);
    std::thread::scope(|s| {
        s.spawn(|| assert!(gain.process(&mut io)));
    });
    assert_eq!(host.poll(), [Request::Restart(gain.token())]);
    assert_eq!(stats(&gain).callbacks, 1, "on_main_thread, from the poll");
    assert_eq!(host.poll(), []);
    assert_eq!(stats(&gain).callbacks, 1, "once a request");
    std::thread::scope(|s| {
        s.spawn(|| gain.stop());
    });
    gain.restart().unwrap();
    let mut io = ramp(32);
    assert!(gain.process(&mut io));
    assert_eq!(io, halved(32));
    gain.stop();
    let s = stats(&gain);
    assert_eq!((s.activates, s.deactivates, s.starts, s.stops, s.broken), (2, 1, 2, 2, 0), "{s:?}");
    // One still processing is stopped first.
    assert!(gain.process(&mut io));
    gain.restart().unwrap();
    let s = stats(&gain);
    assert_eq!((s.activates, s.deactivates, s.starts, s.stops, s.broken), (3, 2, 3, 3, 0), "{s:?}");
    assert!(gain.process(&mut io));
    gain.stop();
    drop(gain);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_file_is_initialised_once_and_let_go_after_its_last_instance() {
    let dir = temp_dir("lifecycle");
    let file = install(&dir, "lifecycle");
    let report = dir.join("lifecycle.deinit");
    let mut host = PluginHost::new();
    let mut playing = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let note = playing.note_on(64, 1.0).unwrap();
    std::thread::scope(|s| {
        // One processing on the audio thread while another (an export's)
        // is created and processed on the main one.
        let audio = s.spawn(|| {
            let mut io = vec![0.0; 128];
            for _ in 0..200 {
                assert!(playing.process(&mut io));
            }
        });
        // SAFETY: as in `create`.
        let mut export = unsafe { host.create(&file, GAIN, Role::Effect, Mode::Offline, RATE, 64) }.unwrap();
        let mut io = ramp(64);
        assert!(export.process(&mut io));
        export.stop();
        let s = stats(&export);
        assert_eq!((s.entry_inits, s.live, s.broken), (1, 2, 0), "{s:?}");
        drop(export);
        audio.join().unwrap();
    });
    playing.note_off(note);
    let mut io = vec![0.0; 8];
    assert!(playing.process(&mut io));
    let s = stats(&playing);
    assert_eq!((s.entry_inits, s.live, s.broken), (1, 1, 0), "{s:?}");
    host.poll();
    assert!(!report.exists(), "in use");
    // Left processing: it is stopped where it is dropped, playing the
    // audio thread; its file goes with it.
    drop(playing);
    let text = std::fs::read_to_string(&report).expect("deinitialised");
    assert_eq!(text, "inits=1 live=0 broken=0\n");
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The test plugin's report from `deinit`, under `stem`.
fn deinit_report(dir: &Path, stem: &str) -> String {
    std::fs::read_to_string(dir.join(format!("{stem}.deinit"))).unwrap_or_else(|_| format!("{stem} is still loaded"))
}

#[test]
fn an_instance_that_comes_to_nothing_is_destroyed_once_and_its_file_let_go() {
    let dir = temp_dir("nothing");
    let mut host = PluginHost::new();
    for (stem, role, why) in [
        ("initfail", Role::Effect, "refused to start (its init failed)"),
        ("noactivate", Role::Effect, "could not start at 48000 Hz"),
        ("incomplete", Role::Effect, "is an incomplete plugin"),
        ("nomain", Role::Effect, "has no mono or stereo main output"),
    ] {
        let file = install(&dir, stem);
        assert_eq!(create(&mut host, &file, GAIN, role).err().as_deref(), Some(why));
        assert_eq!(deinit_report(&dir, stem), "inits=1 live=0 broken=0\n", "{stem}");
    }
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_restart_ends_an_instrument_s_voices() {
    let dir = temp_dir("revoice");
    let file = install(&dir, "revoice");
    let mut host = PluginHost::new();
    let mut sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let note = sine.note_on(60, 1.0).unwrap();
    let mut io = vec![0.0; 16];
    assert!(sine.process(&mut io));
    assert_ne!(io[0], 0.0);
    // The plugin keeps voices across deactivation; CLAP's reset ends them.
    sine.restart().unwrap();
    assert!(sine.process(&mut io));
    assert!(io.iter().all(|&x| x == 0.0), "{io:?}");
    sine.note_off(note);
    assert!(sine.process(&mut io));
    let s = stats(&sine);
    assert_eq!((s.notes_on, s.notes_off, s.broken), (1, 0, 0), "the release of a note from before is not sent");
    sine.stop();
    drop(sine);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_process_call_stops_the_instance_until_it_is_restarted() {
    let dir = temp_dir("procerror");
    let file = install(&dir, "procerror");
    let mut host = PluginHost::new();
    let mut sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let note = sine.note_on(60, 1.0).unwrap();
    let mut io = vec![0.0; 16];
    assert!(sine.process(&mut io));
    assert_ne!(io[0], 0.0);
    sine.note_off(note);
    // The plugin fails before reading the release.
    assert!(!sine.process(&mut io));
    assert!(io.iter().all(|&x| x == 0.0), "silent");
    assert!(!sine.is_playing());
    assert_eq!(sine.queued(), 1, "the release it did not read is kept");
    assert_eq!(host.poll(), [Request::Failed(sine.token())]);
    assert!(!sine.process(&mut io), "not called again");
    assert!(sine.note_on(62, 1.0).is_none(), "takes no notes meanwhile");
    assert_eq!(sine.queued(), 1);
    assert_eq!(stats(&sine).processes, 2);
    sine.restart().unwrap();
    assert!(sine.process(&mut io));
    assert!(io.iter().all(|&x| x == 0.0), "the voice the lost release left is reset");
    let s = stats(&sine);
    assert_eq!((s.processes, s.broken), (3, 0), "{s:?}");
    sine.stop();
    drop(sine);
    assert_eq!(deinit_report(&dir, "procerror"), "inits=1 live=0 broken=0\n");
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_instance_is_main_threaded_on_its_own_host_s_thread_only() {
    let dir = temp_dir("leak");
    let file = install(&dir, "leak");
    let mut host = PluginHost::new();
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    assert_eq!(thread_check(&gain), (true, false));
    std::thread::scope(|s| {
        s.spawn(move || {
            // Another thread with a host of its own is not this instance's
            // main thread.
            let _other = PluginHost::new();
            assert_eq!(thread_check(&gain), (false, false));
            assert_eq!(gain.restart().err().as_deref(), Some("can only be restarted on its host's thread"));
            let mut io = ramp(8);
            // It asks for a restart and a callback here, before it is lost.
            assert!(gain.process(&mut io), "processing can be anywhere");
            gain.stop();
            assert_eq!(stats(&gain).broken, 0);
            // Dropped here, it goes home, never destroyed off its thread.
            drop(gain);
        });
    });
    assert_eq!(deinit_report(&dir, "leak"), "leak is still loaded", "it keeps its file on its way home");
    assert_eq!(host.poll(), [], "destroyed by the poll, and not served");
    assert_eq!(deinit_report(&dir, "leak"), "inits=1 live=0 broken=0\n", "destroyed on its host's thread");
    // A host's own instances all get their own token.
    let mut other = PluginHost::new();
    let a = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let b = create(&mut other, &file, GAIN, Role::Effect).unwrap();
    assert_ne!(a.token(), b.token());
    // One dropped off its thread after its host has gone is leaked.
    drop(other);
    std::thread::scope(|s| {
        s.spawn(move || drop(b));
    });
    drop(a);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

// Parameters, state and latency.

/// `ramp` times `gain`.
fn scaled(frames: usize, gain: f32) -> Vec<f32> {
    ramp(frames).iter().map(|x| x * gain).collect()
}

fn processed(i: &mut Instance, frames: usize) -> Vec<f32> {
    let mut io = ramp(frames);
    assert!(i.process(&mut io));
    io
}

#[test]
fn a_plugin_s_parameters_are_read_and_set_by_id() {
    let dir = temp_dir("params");
    let file = install(&dir, "params");
    let mut host = PluginHost::new();
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let handle = gain.handle();
    let params = gain.params().clone();
    let ids: Vec<u32> = params.iter().map(|p| p.id).collect();
    assert_eq!(ids, [10, 11, 12, 13, 14]);
    assert_eq!(handle.params(), params, "the handle sees what the instance read");
    let (g, kind, hold, meter, offset) = (&params[0], &params[1], &params[2], &params[3], &params[4]);
    assert_eq!((g.name.as_str(), g.module.as_str(), g.min, g.max, g.default), ("Gain", "Main", 0.0, 1.0, 0.5));
    assert!(g.is_automatable() && !g.is_stepped() && !g.is_hidden() && !g.is_readonly());
    assert!(kind.is_stepped() && hold.is_hidden() && meter.is_readonly());
    assert_ne!(offset.flags & CLAP_PARAM_REQUIRES_PROCESS, 0);
    assert_eq!(handle.value(10), Some(0.5));
    assert_eq!(handle.text(10, 0.5).as_deref(), Some("50.0 %"));
    assert_eq!(handle.text(11, 2.0).as_deref(), Some("wild"));
    assert_eq!(handle.parse(11, "hard"), Some(1.0));
    assert_eq!(handle.parse(10, "25 %"), Some(0.25));
    assert_eq!(handle.value(99), None);

    // A change reaches the plugin in the next call, and is acknowledged.
    assert!(gain.set_param(10, 0.25, 1));
    assert_eq!(handle.delivered(), 0, "not before the plugin has it");
    assert_eq!(processed(&mut gain, 16), scaled(16, 0.25));
    assert_eq!((handle.delivered(), handle.value(10)), (1, Some(0.25)));
    // Two to one parameter before a call: the later one, once.
    assert!(gain.set_param(10, 0.1, 2) && gain.set_param(10, 0.75, 3));
    assert_eq!(gain.params_queued(), 1);
    assert_eq!(processed(&mut gain, 16), scaled(16, 0.75));
    assert_eq!((handle.delivered(), stats(&gain).params_set), (3, 2));
    // Kept in range, a stepped one whole; refused, nothing queued.
    assert!(gain.set_param(10, 5.0, 4) && gain.set_param(11, 1.7, 5));
    for (id, value) in [(13, 0.5), (99, 0.5), (10, f64::NAN), (10, f64::INFINITY)] {
        assert!(!gain.set_param(id, value, 6), "{id} {value}");
    }
    processed(&mut gain, 8);
    assert_eq!((handle.value(10), handle.value(11), handle.delivered()), (Some(1.0), Some(1.0), 5));
    gain.stop();
    assert_eq!(stats(&gain).broken, 0, "every change in range, whole where stepped, for every note");
    // A stepped one with bounds between whole numbers stays within them.
    let stepped = ParamInfo { min: 0.5, max: 2.5, flags: CLAP_PARAM_IS_STEPPED, ..g.clone() };
    let kept = [0.0, 0.7, 1.7, 9.0, -3.0].map(|v| stepped.clamp(v));
    assert_eq!(kept, [Some(1.0), Some(1.0), Some(1.0), Some(2.0), Some(1.0)]);

    // Parameter changes never take a note's room.
    let mut sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let notes = |i: &mut Instance| (0..).take_while(|_| i.note_on(60, 0.5).is_some()).count();
    let plain = notes(&mut sine);
    sine.restart().unwrap();
    assert!(sine.set_param(20, 0.5, 1));
    assert_eq!(notes(&mut sine), plain);
    sine.restart().unwrap();
    assert_eq!(sine.params_queued(), 1, "kept through a restart");
    sine.note_on(69, 0.8).unwrap();
    let mut io = vec![0.0; 2 * 16];
    assert!(sine.process(&mut io));
    assert_eq!((io[0], io[1]), (0.5, 0.25), "each voice at the level set");
    sine.stop();
    assert_eq!(stats(&sine).broken, 0);
    drop((gain, sine));
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_parameter_change_reaches_a_plugin_not_processed_through_a_flush() {
    let dir = temp_dir("flush");
    let file = install(&dir, "flush");
    let mut host = PluginHost::new();
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let handle = gain.handle();
    let token = gain.token();
    std::thread::scope(|s| {
        s.spawn(|| {
            processed(&mut gain, 8);
            gain.skip(64);
            assert_eq!(stats(&gain).flushes, 0, "nothing to flush");
            // The offset needs processing, so it waits for a process call;
            // the serial it waits with is its later change's.
            assert!(gain.set_param(14, 0.2, 6) && gain.set_param(10, 0.25, 7) && gain.set_param(14, 0.5, 8));
            gain.skip(64);
            gain.skip(64);
        });
    });
    let s = stats(&gain);
    assert_eq!((s.flushes, s.params_set, s.broken), (1, 1, 0), "on the audio thread, once: {s:?}");
    assert_eq!((gain.params_queued(), handle.delivered(), handle.value(10)), (1, 7, Some(0.25)));
    assert_eq!(host.poll(), [Request::StateDirty(token)], "what the plugin would save changed");
    std::thread::scope(|s| {
        s.spawn(|| {
            let offset: Vec<f32> = scaled(8, 0.25).iter().map(|x| x + 0.5).collect();
            assert_eq!(processed(&mut gain, 8), offset);
            gain.stop();
        });
    });
    assert_eq!((gain.params_queued(), handle.delivered(), stats(&gain).broken), (0, 8, 0));
    drop(gain);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_handle_works_on_its_host_s_thread_while_the_plugin_plays_elsewhere() {
    let dir = temp_dir("handle");
    let file = install(&dir, "handle");
    let mut host = PluginHost::new();
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let handle = gain.handle();
    let token = gain.token();
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        let audio = s.spawn(|| {
            for n in 0..2000u64 {
                gain.set_param(10, (n % 100) as f64 / 100.0, n + 1);
                processed(&mut gain, 32);
            }
            gain.stop();
            done.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let mut reads = 0;
        while !done.load(std::sync::atomic::Ordering::Relaxed) || reads == 0 {
            assert!(handle.value(10).is_some_and(|v| (0.0..=1.0).contains(&v)));
            assert!(handle.text(10, 0.5).is_some());
            assert!(handle.save_state().unwrap().is_some_and(|b| b.len() == 4 + 5 * 8));
            reads += 1;
        }
        audio.join().unwrap();
    });
    assert_eq!(handle.delivered(), 2000);
    // For certain while a process call is under way: the plugin holds the
    // call until a state is saved.
    assert!(gain.set_param(12, 1.0, 2001));
    std::thread::scope(|s| {
        let audio = s.spawn(|| {
            processed(&mut gain, 32);
            gain.stop();
        });
        let start = std::time::Instant::now();
        while handle.value(13) != Some(1.0) {
            assert!(start.elapsed() < Duration::from_secs(10), "the call is held");
            std::thread::yield_now();
        }
        assert!(handle.save_state().unwrap().is_some());
        assert_eq!(handle.value(10), Some(0.99));
        audio.join().unwrap();
    });
    assert_eq!(stats(&gain).overlaps, 1, "saved during the call");
    // Not `Send` nor `Sync`, it cannot reach another thread at all.
    assert_eq!(stats(&gain).broken, 0, "every call on its thread");
    // Its instance gone, it does nothing, and its host forgets it.
    drop(gain);
    assert!(!handle.is_alive());
    assert_eq!((handle.value(10), handle.text(10, 0.5)), (None, None));
    assert!(handle.save_state().is_err());
    assert_eq!(handle.token(), token);
    assert_eq!(host.poll(), [], "nothing it asked for outlives it");
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_saved_state_is_loaded_before_the_plugin_starts() {
    let dir = temp_dir("state");
    let file = install(&dir, "state");
    let mut host = PluginHost::new();
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    gain.set_param(10, 0.25, 1);
    gain.set_param(11, 2.0, 2);
    processed(&mut gain, 8);
    gain.stop();
    assert_eq!(host.poll(), [Request::StateDirty(gain.token())]);
    let saved = gain.handle().save_state().unwrap().unwrap();
    assert_eq!(gain.save_state().unwrap().as_deref(), Some(&saved[..]));
    let make = |host: &mut PluginHost, state: &[u8]| unsafe {
        host.create_with_state(&file, GAIN, Role::Effect, Mode::Realtime, RATE, 64, Some(state))
    };
    let mut again = make(&mut host, &saved).unwrap();
    assert_eq!(stats(&again).start_gain, 0.25, "loaded before it was activated");
    assert_eq!(processed(&mut again, 16), scaled(16, 0.25), "from its first call");
    assert_eq!(again.handle().value(11), Some(2.0));
    let rescan = CLAP_PARAM_RESCAN_VALUES;
    assert_eq!(host.poll(), [Request::Params(again.token(), rescan)], "it said its values changed");
    again.stop();
    assert_eq!(stats(&again).broken, 0, "loaded on the main thread, while not active");
    // A state it cannot read is refused, and the instance made comes to
    // nothing.
    assert_eq!(make(&mut host, b"nonsense").err().as_deref(), Some("could not load its saved state"));
    let too_big = vec![0; tinvo::clap::host::MAX_STATE + 1];
    assert_eq!(make(&mut host, &too_big).err().as_deref(), Some("has a saved state over 16 MiB"));
    drop((gain, again));
    for (stem, error) in
        [("savefail", "could not save its state"), ("bigstate", "has a state over 16 MiB, too large to keep")]
    {
        let file = install(&dir, stem);
        let gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
        assert_eq!(gain.save_state().err().as_deref(), Some(error), "{stem}");
        assert_eq!(gain.handle().save_state().err().as_deref(), Some(error), "{stem}");
    }
    let file = install(&dir, "loadfail");
    let made = unsafe { host.create_with_state(&file, GAIN, Role::Effect, Mode::Realtime, RATE, 64, Some(&saved)) };
    assert_eq!(made.err().as_deref(), Some("could not load its saved state"));
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_s_own_changes_and_a_dirty_state_are_reported() {
    let dir = temp_dir("dirty");
    let file = install(&dir, "plain");
    let mut host = PluginHost::new();
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let token = gain.token();
    processed(&mut gain, 8);
    assert_eq!(host.poll(), [], "nothing changed");
    // A change the host made dirties the state, as CLAP says, though the
    // plugin does not say so; processed or flushed.
    gain.set_param(10, 0.3, 1);
    processed(&mut gain, 8);
    assert_eq!(host.poll(), [Request::StateDirty(token)]);
    assert_eq!(host.poll(), []);
    gain.set_param(10, 0.4, 2);
    gain.skip(8);
    assert_eq!(host.poll(), [Request::StateDirty(token)]);
    gain.stop();
    drop(gain);

    // The plugin's own `mark_dirty`.
    let file = install(&dir, "dirty");
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    processed(&mut gain, 8);
    assert_eq!(host.poll(), [Request::StateDirty(gain.token())]);
    gain.stop();
    drop(gain);

    let (values, info) = (CLAP_PARAM_RESCAN_VALUES, CLAP_PARAM_RESCAN_INFO);
    let file = install(&dir, "meter");
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let (token, handle) = (gain.token(), gain.handle());
    // Its meter (read-only) and, once, its mode, changed by itself; then
    // its names, what is hidden, and its hold cleared.
    processed(&mut gain, 8);
    let cleared = Request::Cleared { token, id: 12, flags: CLAP_PARAM_CLEAR_ALL | CLAP_PARAM_CLEAR_AUTOMATIONS };
    assert_eq!(host.poll(), [Request::Params(token, values | info), cleared, Request::StateDirty(token)]);
    assert_eq!(handle.value(11), Some(1.0));
    let names: Vec<(String, bool)> = handle.params().iter().map(|p| (p.name.clone(), p.is_hidden())).collect();
    assert_eq!((names[0].0.as_str(), names[2].1), ("Volume", false), "read again for the handle");
    assert_eq!((gain.params()[0].name.as_str(), gain.params()[2].is_hidden()), ("Gain", true), "not by the instance");
    processed(&mut gain, 8);
    assert_eq!(host.poll(), [Request::Params(token, values)], "a meter's change dirties nothing");
    // A change waiting for a parameter cleared goes at the restart, when
    // its id may name another.
    assert!(gain.set_param(12, 1.0, 5));
    gain.restart().unwrap();
    assert_eq!((gain.params_queued(), handle.delivered()), (0, 5), "dropped, so settled");
    gain.stop();
    assert_eq!(stats(&gain).broken, 0);
    drop(gain);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_s_latency_is_read_each_time_it_is_activated() {
    let dir = temp_dir("latency");
    let mut host = PluginHost::new();
    let file = install(&dir, "fixture");
    assert_eq!(create(&mut host, &file, GAIN, Role::Effect).unwrap().latency(), 0, "none said");
    let file = install(&dir, "latency");
    let mut gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    assert_eq!(gain.latency(), 64);
    let out = processed(&mut gain, 128);
    assert!(out[..128].iter().all(|&x| x == 0.0));
    assert_eq!(&out[128..], &halved(64)[..], "64 frames late");
    // Activated again, it says what it says then.
    gain.restart().unwrap();
    assert_eq!(gain.latency(), 128);
    let out = processed(&mut gain, 256);
    assert!(out[..256].iter().all(|&x| x == 0.0));
    assert_eq!(&out[256..], &halved(128)[..], "128 frames late");
    gain.stop();
    assert_eq!(stats(&gain).broken, 0, "asked on the main thread, while active");
    drop(gain);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

// In the engine: plugin tracks and inserts played by `Engine`, with the
// instances coming back through the retirement queue as the editor gets
// them.

/// An engine at 48 kHz with `edits` applied and a retirement queue.
fn plugin_engine(edits: &[Edit]) -> (Box<Engine>, Consumer<Retired>) {
    let mut e = Engine::with_edits(RATE as f32, edits, Arc::new(Shared::new()), None);
    let (tx, rx) = spsc::channel(RETIRE_QUEUE);
    e.attach_retirement(tx);
    (e, rx)
}

fn render(e: &mut Engine, frames: usize, size: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(2 * frames);
    let mut left = frames;
    while left > 0 {
        let n = size.min(left);
        let mut buf = vec![0.0; 2 * n];
        e.process(&mut buf);
        out.extend(buf);
        left -= n;
    }
    out
}

fn first_sound(out: &[f32]) -> Option<usize> {
    out.iter().position(|x| x.abs() > 1e-9).map(|i| i / 2)
}

/// One thing back through the retirement queue: its kind, and the token of
/// the instance in it with the instance.
type Back = (&'static str, Option<u64>, Option<Box<Instance>>);

/// What came back through the retirement queue.
/// Every instance in it came back stopped.
fn came_back(rx: &mut Consumer<Retired>) -> Vec<Back> {
    let stopped = |i: &Instance| {
        let s = stats(i);
        assert_eq!(s.starts, s.stops, "{i:?} came back stopped: {s:?}");
    };
    std::iter::from_fn(|| rx.pop())
        .map(|r| match r {
            Retired::Plugin(i) => {
                stopped(&i);
                ("plugin", Some(i.token()), Some(i))
            }
            Retired::Lent(i) => {
                stopped(&i);
                ("lent", Some(i.token()), Some(i))
            }
            Retired::Insert(mut i) => {
                if let Some(inst) = i.plugin_slot().and_then(|s| s.instance()) {
                    stopped(inst);
                }
                let inst = i.take_instance();
                ("insert", inst.as_ref().map(|i| i.token()), inst)
            }
            Retired::Sample(_) => ("sample", None, None),
        })
        .collect()
}

fn sine_ref() -> PluginRef {
    PluginRef::new(SINE).unwrap()
}

fn gain_ref() -> PluginRef {
    PluginRef::new(GAIN).unwrap()
}

#[test]
fn a_plugin_track_plays_its_notes_on_their_frames_in_stereo() {
    let dir = temp_dir("engine-notes");
    let file = install(&dir, "notes");
    let mut host = PluginHost::new();
    // At 120 BPM a step is 6000 frames: step 5 starts on frame 30000, off
    // the 64-frame block.
    let edits = [
        Edit::AddTrack(InstrumentKind::Plugin(sine_ref())),
        Edit::SetStep { track: 0, pattern: 0, step: 5, value: Some(Step::new(69, 100, 1)) },
    ];
    let mut reference = None;
    for size in [1usize, 17, 64, 500, 4096] {
        let (mut e, mut rx) = plugin_engine(&edits);
        let sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
        let token = sine.token();
        e.handle(EngineCommand::SetPlugin { track: 0, instance: Box::new(sine) });
        e.handle(EngineCommand::Play);
        let out = render(&mut e, 40_000, size);
        assert_eq!(first_sound(&out), Some(30_000), "buffer size {size}");
        // The sine's own first frame, its right half its left: a neutral
        // strip, a centred balance and the master leave it alone.
        assert_eq!((out[60_000], out[60_001]), (0.25, 0.125), "buffer size {size}");
        assert!(out.chunks_exact(2).all(|f| f[1] == 0.5 * f[0]), "stereo all through, buffer size {size}");
        match &reference {
            None => reference = Some(out),
            Some(r) => assert!(r == &out, "the same at buffer size {size}"),
        }
        e.handle(EngineCommand::Lend(token));
        let mut back = came_back(&mut rx);
        assert_eq!(back.len(), 1);
        let (what, t, inst) = back.pop().unwrap();
        assert_eq!((what, t), ("lent", Some(token)));
        let s = stats(&inst.unwrap());
        assert_eq!((s.broken, s.notes_on, s.stops), (0, 1, 1), "{s:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_insert_runs_in_its_chain() {
    let dir = temp_dir("engine-insert");
    let file = install(&dir, "insert");
    let mut host = PluginHost::new();
    let edits = [
        Edit::AddTrack(InstrumentKind::Synth),
        Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(48, 100, 4)) },
    ];
    let play = |insert: Option<Box<Insert>>| {
        let (mut e, rx) = plugin_engine(&edits);
        let had = insert.is_some();
        e.handle(EngineCommand::SetSlot { chain: ChainId::Track(0), slot: 0, insert });
        e.handle(EngineCommand::Play);
        let out = render(&mut e, 12_000, 64);
        assert!(!had || first_sound(&out).is_some());
        (out, e, rx)
    };
    let (dry, mut more_dry, _dry_rx) = play(None);
    let gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let token = gain.token();
    let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0));
    assert!(insert.attach(Box::new(gain)).unwrap().is_none());
    let (wet, mut e, mut rx) = play(Some(insert));
    let energy = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>();
    assert!(energy(&wet[..2 * 480]) < 0.8 * energy(&dry[..2 * 480]), "heard while it fades in");
    // In after its 10 ms fade, it halves the track.
    for i in 2 * 1000..wet.len() {
        assert!((wet[i] - 0.5 * dry[i]).abs() <= 1e-6 * dry[i].abs().max(1e-3), "sample {i}: {} {}", wet[i], dry[i]);
    }
    // Lent out, the slot passes its input; given back, it halves again.
    e.handle(EngineCommand::Lend(token));
    let (what, t, inst) = came_back(&mut rx).pop().unwrap();
    assert_eq!((what, t), ("lent", Some(token)));
    let mut inst = inst.unwrap();
    assert_eq!(render(&mut e, 64, 64), render(&mut more_dry, 64, 64), "its input as it was");
    inst.restart().unwrap();
    e.handle(EngineCommand::Return(inst));
    assert!(came_back(&mut rx).is_empty(), "taken back");
    let (wet, dry) = (render(&mut e, 64, 64), render(&mut more_dry, 64, 64));
    assert!(dry.iter().any(|x| x.abs() > 1e-3));
    for (w, d) in wet.iter().zip(&dry) {
        assert!((w - 0.5 * d).abs() <= 1e-6 * d.abs().max(1e-3), "halved again: {w} {d}");
    }
    // Emptied, the slot hands the insert back with its instance in it.
    e.handle(EngineCommand::SetSlot { chain: ChainId::Track(0), slot: 0, insert: None });
    render(&mut e, 2000, 64);
    let back = came_back(&mut rx);
    assert_eq!(back.iter().map(|b| (b.0, b.1)).collect::<Vec<_>>(), [("insert", Some(token))]);
    let s = stats(back[0].2.as_ref().unwrap());
    assert_eq!((s.broken, s.activates, s.deactivates), (0, 2, 1), "{s:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_lent_instance_comes_back_to_its_place_or_goes() {
    let dir = temp_dir("engine-lend");
    let file = install(&dir, "lend");
    let mut host = PluginHost::new();
    let edits = [
        Edit::AddTrack(InstrumentKind::Plugin(sine_ref())),
        Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 100, 16)) },
    ];
    let (mut e, mut rx) = plugin_engine(&edits);
    let sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let token = sine.token();
    e.handle(EngineCommand::SetPlugin { track: 0, instance: Box::new(sine) });
    e.handle(EngineCommand::Play);
    assert!(first_sound(&render(&mut e, 1000, 64)).is_some());
    e.handle(EngineCommand::Lend(token + 1_000_000));
    assert!(came_back(&mut rx).is_empty(), "no instance has that token");
    e.handle(EngineCommand::Lend(token));
    let (what, _, inst) = came_back(&mut rx).pop().unwrap();
    assert_eq!(what, "lent");
    assert_eq!(first_sound(&render(&mut e, 1000, 64)), None, "silent meanwhile");
    let mut inst = inst.unwrap();
    inst.restart().unwrap();
    let stranger = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let other = stranger.token();
    e.handle(EngineCommand::Return(Box::new(stranger)));
    assert_eq!(came_back(&mut rx).iter().map(|b| (b.0, b.1)).collect::<Vec<_>>(), [("plugin", Some(other))]);
    e.handle(EngineCommand::Return(inst));
    assert!(came_back(&mut rx).is_empty());
    // Its note went with the restart; the next pass plays again.
    let out = render(&mut e, 6000 * 16, 64);
    assert!(first_sound(&out[2 * 6000 * 15..]).is_some(), "plays again from the next pass");
    // Lent and given back as it was, it lets its notes go.
    e.handle(EngineCommand::Lend(token));
    let (_, _, inst) = came_back(&mut rx).pop().unwrap();
    let inst = inst.unwrap();
    let before = stats(&inst).notes_off;
    e.handle(EngineCommand::Return(inst));
    render(&mut e, 64, 64);
    e.handle(EngineCommand::Lend(token));
    let (_, _, inst) = came_back(&mut rx).pop().unwrap();
    let inst = inst.unwrap();
    assert_eq!(stats(&inst).notes_off, before + 1, "its held note released");
    e.handle(EngineCommand::Return(inst));
    // Lent out and its track removed meanwhile, it goes when it comes back.
    e.handle(EngineCommand::Lend(token));
    let (_, _, inst) = came_back(&mut rx).pop().unwrap();
    e.handle(EngineCommand::Edit(Edit::RemoveTrack(0)));
    render(&mut e, 2000, 64);
    assert!(came_back(&mut rx).is_empty(), "a lent-out track has nothing to give back");
    e.handle(EngineCommand::Return(inst.unwrap()));
    let back = came_back(&mut rx);
    assert_eq!(back.iter().map(|b| (b.0, b.1)).collect::<Vec<_>>(), [("plugin", Some(token))]);
    assert_eq!(stats(back[0].2.as_ref().unwrap()).broken, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_instance_a_track_lets_go_of_comes_back() {
    let dir = temp_dir("engine-back");
    let file = install(&dir, "back");
    let mut host = PluginHost::new();
    let r = sine_ref();
    let mut edits = vec![Edit::AddTrack(InstrumentKind::Synth)];
    edits.extend((0..17).map(|_| Edit::AddTrack(InstrumentKind::Plugin(r))));
    let (mut e, mut rx) = plugin_engine(&edits);
    let mut tokens = Vec::new();
    for t in 1..=17u8 {
        let sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
        tokens.push(sine.token());
        e.handle(EngineCommand::SetPlugin { track: t, instance: Box::new(sine) });
    }
    // One that fits no track there: a synth's, or one past the tracks.
    for t in [0u8, 30] {
        let sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
        let token = sine.token();
        e.handle(EngineCommand::SetPlugin { track: t, instance: Box::new(sine) });
        assert_eq!(came_back(&mut rx).iter().map(|b| (b.0, b.1)).collect::<Vec<_>>(), [("plugin", Some(token))]);
    }
    // An effect is not an instrument.
    let gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let g = gain.token();
    e.handle(EngineCommand::SetPlugin { track: 1, instance: Box::new(gain) });
    assert_eq!(came_back(&mut rx).iter().map(|b| (b.0, b.1)).collect::<Vec<_>>(), [("plugin", Some(g))]);
    render(&mut e, 64, 64);
    // Replaced, the one before comes back at once, stopped.
    let sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let newer = sine.token();
    e.handle(EngineCommand::SetPlugin { track: 1, instance: Box::new(sine) });
    assert_eq!(came_back(&mut rx).iter().map(|b| (b.0, b.1)).collect::<Vec<_>>(), [("plugin", Some(tokens[0]))]);
    tokens[0] = newer;
    let gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let in_chain = gain.token();
    let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0));
    assert!(insert.attach(Box::new(gain)).unwrap().is_none());
    e.handle(EngineCommand::SetSlot { chain: ChainId::Track(2), slot: 0, insert: Some(insert) });
    render(&mut e, 64, 64);
    // Seventeen removed at once: the oldest fade is cut short by the last
    // and its instance comes back then; the rest when their fades end.
    for _ in 0..17 {
        e.handle(EngineCommand::Edit(Edit::RemoveTrack(1)));
    }
    let early: Vec<u64> = came_back(&mut rx).iter().filter_map(|b| b.1).collect();
    assert_eq!(early, [tokens[0]]);
    render(&mut e, 479, 64);
    assert!(came_back(&mut rx).is_empty(), "not before the 10 ms fade ends");
    render(&mut e, 64, 64);
    // Each played its blocks and its fade, and no further.
    let mut rest: Vec<(u64, u32, u32)> = came_back(&mut rx)
        .into_iter()
        .map(|(_, t, i)| {
            let s = stats(&i.unwrap());
            (t.unwrap(), s.broken, s.frames)
        })
        .collect();
    rest.sort();
    let mut expected: Vec<(u64, u32, u32)> = tokens[1..].iter().map(|&t| (t, 0, 128 + 480)).collect();
    expected.push((in_chain, 0, 64 + 480));
    expected.sort();
    assert_eq!(rest, expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_instance_never_hears_the_last_one_s_releases() {
    let dir = temp_dir("engine-stale");
    let file = install(&dir, "stale");
    let mut host = PluginHost::new();
    let edits = [
        Edit::AddTrack(InstrumentKind::Plugin(sine_ref())),
        Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 100, 1)) },
    ];
    let (mut e, mut rx) = plugin_engine(&edits);
    let first = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    e.handle(EngineCommand::SetPlugin { track: 0, instance: Box::new(first) });
    e.handle(EngineCommand::Play);
    render(&mut e, 100, 64);
    // Its note's release is due at frame 6000; another instance takes over
    // and plays a note of its own, with the same note id.
    let second = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let token = second.token();
    e.handle(EngineCommand::SetPlugin { track: 0, instance: Box::new(second) });
    let (_, _, first) = came_back(&mut rx).pop().unwrap();
    assert_eq!(stats(&first.unwrap()).notes_on, 1);
    e.handle(EngineCommand::Audition { track: 0, step: Step::new(64, 100, 1), frames: 20_000 });
    render(&mut e, 7000, 64);
    e.handle(EngineCommand::Lend(token));
    let (_, _, second) = came_back(&mut rx).pop().unwrap();
    let s = stats(&second.unwrap());
    assert_eq!((s.notes_on, s.notes_off, s.broken), (1, 0, 0), "its note still held: {s:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_instance_never_hears_the_last_one_s_live_releases() {
    use tinvo::engine::LIVE_QUEUE;
    use tinvo::midi::Message;
    use tinvo::rt::mailbox::Mailbox;
    let dir = temp_dir("engine-live");
    let file = install(&dir, "live");
    let mut host = PluginHost::new();
    let (mut e, mut rx) = plugin_engine(&[Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))]);
    let live = Arc::new(Mailbox::new(LIVE_QUEUE));
    e.attach_live(live.clone());
    let first = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    e.handle(EngineCommand::SetPlugin { track: 0, instance: Box::new(first) });
    // A key held on the first instance: its note id is 0.
    assert!(live.push(Message::note(0, 67, 100)).is_ok());
    render(&mut e, 256, 64);
    let second = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let token = second.token();
    e.handle(EngineCommand::SetPlugin { track: 0, instance: Box::new(second) });
    let (_, _, first) = came_back(&mut rx).pop().unwrap();
    assert_eq!(stats(&first.unwrap()).notes_on, 1);
    // The second instance's own note, id 0 as well, outlives the key.
    e.handle(EngineCommand::Audition { track: 0, step: Step::new(64, 100, 1), frames: 20_000 });
    render(&mut e, 256, 64);
    assert!(live.push(Message::note(0, 67, 0)).is_ok());
    render(&mut e, 256, 64);
    e.handle(EngineCommand::Lend(token));
    let (_, _, second) = came_back(&mut rx).pop().unwrap();
    let s = stats(&second.unwrap());
    assert_eq!((s.notes_on, s.notes_off, s.broken), (1, 0, 0), "its note still held: {s:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn instances_in_an_engine_dropped_off_their_thread_go_home() {
    let dir = temp_dir("engine-home");
    let file = install(&dir, "leak");
    let mut host = PluginHost::new();
    let edits = [
        Edit::AddTrack(InstrumentKind::Plugin(sine_ref())),
        Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 100, 2)) },
    ];
    let (mut e, rx) = plugin_engine(&edits);
    e.handle(EngineCommand::SetPlugin {
        track: 0,
        instance: Box::new(create(&mut host, &file, SINE, Role::Instrument).unwrap()),
    });
    let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0));
    insert.attach(Box::new(create(&mut host, &file, GAIN, Role::Effect).unwrap())).unwrap();
    e.handle(EngineCommand::SetSlot { chain: ChainId::Master, slot: 0, insert: Some(insert) });
    e.handle(EngineCommand::Play);
    // An audio thread plays them, and its engine ends there, as a stream's
    // does when it stops.
    std::thread::scope(|s| {
        s.spawn(move || {
            assert!(first_sound(&render(&mut e, 4000, 64)).is_some());
            drop(e);
        });
    });
    drop(rx);
    assert_eq!(deinit_report(&dir, "leak"), "leak is still loaded");
    let _ = host.poll();
    assert_eq!(deinit_report(&dir, "leak"), "inits=1 live=0 broken=0\n", "destroyed on their host's thread");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_link_counts_an_insert_with_an_instance_as_two() {
    use tinvo::app::EngineLink;
    let dir = temp_dir("engine-link");
    let file = install(&dir, "link");
    let mut host = PluginHost::new();
    let (tx, mut commands) = spsc::channel(RETIRE_QUEUE);
    let (mut back, retired) = spsc::channel(RETIRE_QUEUE);
    let mut link = EngineLink::new(tx, Arc::new(Shared::new())).with_retirement(retired, tinvo::engine::HELD_FOR_GOOD);
    // Samples on their way, till two of the queue's places are left.
    let sample = Arc::new(tinvo::sample::Sample::from_mono(&[0.5; 16], 48_000).unwrap());
    for _ in tinvo::engine::HELD_FOR_GOOD..RETIRE_QUEUE - 2 {
        link.send(EngineCommand::SetSample { track: 0, sample: Some(sample.clone()) });
    }
    let sent = |c: &mut Consumer<EngineCommand>| std::iter::from_fn(|| c.pop()).collect::<Vec<_>>();
    assert_eq!(sent(&mut commands).len(), RETIRE_QUEUE - 2 - tinvo::engine::HELD_FOR_GOOD);
    let insert = |host: &mut PluginHost| {
        let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0));
        insert.attach(Box::new(create(host, &file, GAIN, Role::Effect).unwrap())).unwrap();
        insert
    };
    let first = insert(&mut host);
    link.send(EngineCommand::SetSlot { chain: ChainId::Master, slot: 0, insert: Some(first) });
    link.send(EngineCommand::SetSlot { chain: ChainId::Master, slot: 1, insert: Some(insert(&mut host)) });
    let sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let token = sine.token();
    link.send(EngineCommand::SetPlugin { track: 0, instance: Box::new(sine) });
    let one = |c: &mut Consumer<EngineCommand>| <[EngineCommand; 1]>::try_from(sent(c)).expect("one command sent");
    let [EngineCommand::SetSlot { insert: Some(first), .. }] = one(&mut commands) else { unreachable!() };
    // Back with its instance, two places are free again: room for the
    // second insert, not for the instance after it.
    assert!(back.push(Retired::Insert(first)).is_ok());
    link.flush();
    let [EngineCommand::SetSlot { insert: Some(mut second), .. }] = one(&mut commands) else { unreachable!() };
    // They come back apart: the insert, and its instance lent out, which
    // waits for its owner.
    let instance = second.take_instance().unwrap();
    let lent_token = instance.token();
    assert!(back.push(Retired::Insert(second)).is_ok());
    assert!(back.push(Retired::Lent(instance)).is_ok());
    let lent = link.take_lent();
    assert_eq!(lent.iter().map(|i| i.token()).collect::<Vec<_>>(), [lent_token]);
    let [EngineCommand::SetPlugin { instance, .. }] = one(&mut commands) else { unreachable!() };
    assert_eq!(instance.token(), token);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_instance_goes_only_where_it_fits() {
    let dir = temp_dir("engine-fit");
    // Its gain takes notes too, so it can be made as an instrument.
    let file = install(&dir, "both");
    let mut host = PluginHost::new();
    let (mut e, mut rx) = plugin_engine(&[
        Edit::AddTrack(InstrumentKind::Plugin(sine_ref())),
        Edit::AddTrack(InstrumentKind::Plugin(gain_ref())),
    ]);
    let mut sent_back = |track: u8, instance: Instance| {
        let token = instance.token();
        e.handle(EngineCommand::SetPlugin { track, instance: Box::new(instance) });
        came_back(&mut rx).iter().any(|b| b.1 == Some(token))
    };
    // SAFETY: the test plugin, loaded only through plugin hosts here.
    let slower = unsafe { host.create(&file, SINE, Role::Instrument, Mode::Realtime, 44_100.0, 64) }.unwrap();
    assert!(sent_back(0, slower), "made for another sample rate");
    assert!(sent_back(0, create(&mut host, &file, GAIN, Role::Instrument).unwrap()), "another plugin");
    assert!(sent_back(1, create(&mut host, &file, GAIN, Role::Effect).unwrap()), "made as an effect");
    assert!(!sent_back(1, create(&mut host, &file, GAIN, Role::Instrument).unwrap()), "it fits");
    // One already processing comes back stopped.
    let mut started = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    assert!(started.process(&mut [0.0; 128]));
    assert!(sent_back(30, started), "no track there");
    let mut insert = Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0);
    assert!(insert.attach(Box::new(create(&mut host, &file, GAIN, Role::Instrument).unwrap())).is_err());
    assert!(insert.attach(Box::new(create(&mut host, &file, GAIN, Role::Effect).unwrap())).unwrap().is_none());
    drop((e, insert));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_track_releases_its_notes_and_keys() {
    use tinvo::engine::LIVE_QUEUE;
    use tinvo::midi::Message;
    use tinvo::rt::mailbox::Mailbox;
    let dir = temp_dir("engine-release");
    let file = install(&dir, "release");
    let mut host = PluginHost::new();
    let edits = [
        Edit::AddTrack(InstrumentKind::Plugin(sine_ref())),
        Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 100, 1)) },
    ];
    let (mut e, mut rx) = plugin_engine(&edits);
    let live = Arc::new(Mailbox::new(LIVE_QUEUE));
    e.attach_live(live.clone());
    let sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
    let token = sine.token();
    e.handle(EngineCommand::SetPlugin { track: 0, instance: Box::new(sine) });
    e.handle(EngineCommand::Play);
    render(&mut e, 100, 64);
    assert!(live.push(Message::note(0, 67, 100)).is_ok());
    render(&mut e, 1000, 64);
    assert!(live.push(Message::note(0, 67, 0)).is_ok());
    // The step's note is released at frame 6000.
    render(&mut e, 8000, 64);
    e.handle(EngineCommand::Lend(token));
    let (_, _, sine) = came_back(&mut rx).pop().unwrap();
    let s = stats(&sine.unwrap());
    assert_eq!((s.notes_on, s.notes_off, s.broken), (2, 2, 0), "{s:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_track_keys_a_compressor() {
    use tinvo::mix::send;
    use tinvo::strip::sp;
    let dir = temp_dir("engine-key");
    let file = install(&dir, "key");
    let mut host = PluginHost::new();
    // A muted plugin track keys the compressor on a held synth note.
    let ns = InstrumentKind::Synth.params().len();
    let strip = |i: usize, value: f32| Edit::SetParam { track: 1, index: (ns + i) as u8, value };
    let edits = [
        Edit::AddTrack(InstrumentKind::Plugin(sine_ref())),
        Edit::AddTrack(InstrumentKind::Synth),
        Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 100, 2)) },
        Edit::SetStep { track: 1, pattern: 0, step: 0, value: Some(Step::new(48, 100, 16)) },
        Edit::SetMute { track: 0, mute: true },
        strip(sp::COMP_KEY, 1.0),
        strip(sp::COMP_THRESHOLD, -40.0),
        strip(sp::COMP_RATIO, 20.0),
        strip(sp::COMP_ATTACK, 0.0005),
        strip(sp::COMP_RELEASE, 0.05),
    ];
    let mut play = |keyed: bool| {
        let key = Edit::SetParam { track: 0, index: (sp::COUNT + send::KEY) as u8, value: 0.0 };
        let mut all = edits.to_vec();
        all.extend(keyed.then_some(key));
        let (mut e, _rx) = plugin_engine(&all);
        let sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
        e.handle(EngineCommand::SetPlugin { track: 0, instance: Box::new(sine) });
        e.handle(EngineCommand::Play);
        render(&mut e, 6000, 64)
    };
    let (plain, ducked) = (play(false), play(true));
    let energy = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>();
    let hit = 2 * 1000..2 * 6000;
    assert!(energy(&plain[hit.clone()]) > 0.0);
    assert!(energy(&ducked[hit.clone()]) < 0.1 * energy(&plain[hit]), "its middle keys the compressor");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bypassed_plugin_insert_keeps_its_time() {
    let dir = temp_dir("engine-bypass");
    let file = install(&dir, "bypass");
    let mut host = PluginHost::new();
    let (mut e, mut rx) = plugin_engine(&[]);
    let gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let token = gain.token();
    let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0));
    assert!(insert.attach(Box::new(gain)).unwrap().is_none());
    e.handle(EngineCommand::SetSlot { chain: ChainId::Master, slot: 0, insert: Some(insert) });
    let bypass = |on: bool| {
        EngineCommand::Edit(Edit::SetSlotParam {
            chain: ChainId::Master,
            slot: 0,
            index: 0,
            value: if on { 1.0 } else { 0.0 },
        })
    };
    render(&mut e, 1024, 64);
    e.handle(bypass(true));
    render(&mut e, 4096, 64);
    e.handle(bypass(false));
    render(&mut e, 1024, 64);
    e.handle(EngineCommand::Lend(token));
    let (_, _, gain) = came_back(&mut rx).pop().unwrap();
    let s = stats(&gain.unwrap());
    // In, the fade out, and the fade back in (1024 frames) were processed.
    assert_eq!(s.frames, 1024 + 480 + 1024, "not processed while bypassed: {s:?}");
    assert_eq!((s.steady, s.broken), (6144, 0), "its steady time counted every frame: {s:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_s_tail_keeps_the_render_going() {
    let dir = temp_dir("engine-tail");
    let mut host = PluginHost::new();
    for (stem, tail) in [("tail", true), ("notail", false)] {
        let file = install(&dir, stem);
        let (mut e, _rx) = plugin_engine(&[]);
        let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0));
        assert!(insert.attach(Box::new(create(&mut host, &file, GAIN, Role::Effect).unwrap())).unwrap().is_none());
        e.handle(EngineCommand::SetSlot { chain: ChainId::Master, slot: 0, insert: Some(insert) });
        render(&mut e, 128, 64);
        assert_eq!(e.effects_idle(), !tail, "{stem}");
        // Bypassed and no longer called, it has nothing more to play.
        e.handle(EngineCommand::Edit(Edit::SetSlotParam { chain: ChainId::Master, slot: 0, index: 0, value: 1.0 }));
        render(&mut e, 2048, 64);
        assert!(e.effects_idle(), "{stem}");
        // An instrument's holds the render's end as long.
        let (mut e, _rx) = plugin_engine(&[Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))]);
        e.set_plugin(0, Box::new(create(&mut host, &file, SINE, Role::Instrument).unwrap()));
        render(&mut e, 128, 64);
        assert_eq!(e.effects_idle(), !tail, "{stem}: an instrument");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_insert_made_for_another_rate_goes_back() {
    let dir = temp_dir("engine-rate");
    let file = install(&dir, "rate");
    let mut host = PluginHost::new();
    let (mut e, mut rx) = plugin_engine(&[]);
    // SAFETY: the test plugin, loaded only through plugin hosts here.
    let slower = unsafe { host.create(&file, GAIN, Role::Effect, Mode::Realtime, 44_100.0, 64) }.unwrap();
    let token = slower.token();
    let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), 44_100.0, 120.0));
    assert!(insert.attach(Box::new(slower)).unwrap().is_none());
    e.handle(EngineCommand::SetSlot { chain: ChainId::Master, slot: 0, insert: Some(insert) });
    assert_eq!(came_back(&mut rx).iter().map(|b| (b.0, b.1)).collect::<Vec<_>>(), [("insert", Some(token))]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_removed_synth_track_s_plugin_insert_plays_no_further_than_its_fade() {
    let dir = temp_dir("engine-mono-fade");
    let file = install(&dir, "monofade");
    let mut host = PluginHost::new();
    let (mut e, mut rx) = plugin_engine(&[
        Edit::AddTrack(InstrumentKind::Synth),
        Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(48, 100, 16)) },
    ]);
    let gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let token = gain.token();
    let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0));
    assert!(insert.attach(Box::new(gain)).unwrap().is_none());
    e.handle(EngineCommand::SetSlot { chain: ChainId::Track(0), slot: 0, insert: Some(insert) });
    e.handle(EngineCommand::Play);
    render(&mut e, 64, 64);
    e.handle(EngineCommand::Edit(Edit::RemoveTrack(0)));
    render(&mut e, 479, 64);
    assert!(came_back(&mut rx).is_empty(), "not before the 10 ms fade ends");
    // The fade's last frame starts a block.
    render(&mut e, 64, 64);
    let back = came_back(&mut rx);
    assert_eq!(back.iter().map(|b| (b.0, b.1)).collect::<Vec<_>>(), [("insert", Some(token))]);
    let s = stats(back[0].2.as_ref().unwrap());
    assert_eq!((s.frames, s.broken), (64 + 480, 0), "{s:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_instance_goes_into_the_plugin_insert_a_slot_will_hold() {
    let dir = temp_dir("engine-slot");
    let file = install(&dir, "slot");
    let mut host = PluginHost::new();
    let (mut e, mut rx) = plugin_engine(&[
        Edit::AddTrack(InstrumentKind::Synth),
        Edit::SetSlot { chain: ChainId::Track(0), slot: 0, kind: Some(EffectKind::Plugin(gain_ref())) },
        Edit::SetSlot { chain: ChainId::Track(0), slot: 1, kind: Some(EffectKind::Delay) },
    ]);
    let mut give = |e: &mut Engine, chain, slot| {
        let gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
        let token = gain.token();
        e.handle(EngineCommand::SetSlotPlugin { chain, slot, instance: Box::new(gain) });
        token
    };
    let first = give(&mut e, ChainId::Track(0), 0);
    assert!(came_back(&mut rx).is_empty(), "taken");
    // No plugin insert there: another effect, an empty slot, a chain that
    // is not there.
    for (chain, slot) in
        [(ChainId::Track(0), 1), (ChainId::Track(0), 2), (ChainId::Track(3), 0), (ChainId::Group(0), 0)]
    {
        let token = give(&mut e, chain, slot);
        assert_eq!(came_back(&mut rx).iter().map(|b| b.1).collect::<Vec<_>>(), [Some(token)], "{chain:?} {slot}");
    }
    render(&mut e, 64, 64);
    // Replaced, the one before comes back stopped.
    let second = give(&mut e, ChainId::Track(0), 0);
    assert_eq!(came_back(&mut rx).iter().map(|b| (b.0, b.1)).collect::<Vec<_>>(), [("plugin", Some(first))]);
    // A swap waiting for its fade: the insert is already where it goes.
    e.handle(EngineCommand::Edit(Edit::SwapSlots { chain: ChainId::Track(0), a: 0, b: 3 }));
    let third = give(&mut e, ChainId::Track(0), 0);
    assert_eq!(came_back(&mut rx).iter().map(|b| b.1).collect::<Vec<_>>(), [Some(third)], "slot 0 will be empty");
    let _ = give(&mut e, ChainId::Track(0), 3);
    assert_eq!(came_back(&mut rx).iter().map(|b| b.1).collect::<Vec<_>>(), [Some(second)]);
    // A removed group plays out in a slot past the groups in use, its
    // inserts with it: not a place for an instance.
    e.apply(Edit::InsertGroup(0));
    e.apply(Edit::SetSlot { chain: ChainId::Group(0), slot: 0, kind: Some(EffectKind::Plugin(gain_ref())) });
    let _ = give(&mut e, ChainId::Group(0), 0);
    assert!(came_back(&mut rx).is_empty(), "in the group's insert");
    e.apply(Edit::RemoveGroup(0));
    let token = give(&mut e, ChainId::Group(8), 0);
    let back: Vec<_> = came_back(&mut rx).into_iter().filter(|b| b.0 == "plugin").map(|b| b.1).collect();
    assert_eq!(back, [Some(token)], "a retiring group's insert");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A synth track holding one note for its whole pattern, then `more`.
fn held_note(more: &[Edit]) -> Vec<Edit> {
    let mut e = vec![
        Edit::AddTrack(InstrumentKind::Synth),
        Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(48, 100, 16)) },
    ];
    e.extend_from_slice(more);
    e
}

/// The factor `wet` is `dry` scaled by.
fn factor(wet: &[f32], dry: &[f32]) -> f32 {
    let dd: f32 = dry.iter().map(|d| d * d).sum();
    assert!(dd > 1e-6, "something to compare");
    wet.iter().zip(dry).map(|(w, d)| w * d).sum::<f32>() / dd
}

/// Renders `frames` of both, the factor the plugin scales the second
/// half of them by.
fn scaled_by(e: &mut Engine, dry: &mut Engine, frames: usize) -> f32 {
    let (w, d) = (render(e, frames, 64), render(dry, frames, 64));
    factor(&w[frames..], &d[frames..])
}

/// A gain insert from `file` with its instance's token.
fn gain_insert(host: &mut PluginHost, file: &Path) -> (Box<Insert>, u64) {
    let gain = create(host, file, GAIN, Role::Effect).unwrap();
    let token = gain.token();
    let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0));
    assert!(insert.attach(Box::new(gain)).unwrap().is_none());
    (insert, token)
}

const STEP: usize = 6000;

#[test]
fn a_plugin_parameter_is_set_unless_a_lane_holds_it() {
    let dir = temp_dir("engine-params");
    let file = install(&dir, "params");
    let mut host = PluginHost::new();
    let target = LaneTarget::Plugin { at: PluginAt::Slot(0), plugin: gain_ref(), id: 10 };
    let (mut e, _rx) = plugin_engine(&held_note(&[
        Edit::SetLane { track: 0, lane: 0, target: Some(target), ramp: false },
        Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 4, value: Some(0.75) },
    ]));
    let (mut dry, _dry_rx) = plugin_engine(&held_note(&[]));
    e.handle(EngineCommand::SetSlot {
        chain: ChainId::Track(0),
        slot: 0,
        insert: Some(gain_insert(&mut host, &file).0),
    });
    render(&mut e, 1000, 64);
    render(&mut dry, 1000, 64);
    e.handle(EngineCommand::Play);
    dry.handle(EngineCommand::Play);
    let place = PluginPlace::Slot(ChainId::Track(0), 0);
    let set = |value: f64| EngineCommand::SetPluginParam { place, plugin: gain_ref(), id: 10, value, serial: 1 };
    e.handle(set(0.25));
    assert!((scaled_by(&mut e, &mut dry, STEP) - 0.25).abs() < 1e-4, "set before its lane has a point");
    // Somewhere else, or another plugin's, it is nothing to this one.
    e.handle(EngineCommand::SetPluginParam { place, plugin: sine_ref(), id: 10, value: 0.9, serial: 2 });
    let elsewhere = PluginPlace::Slot(ChainId::Track(0), 1);
    e.handle(EngineCommand::SetPluginParam { place: elsewhere, plugin: gain_ref(), id: 10, value: 0.9, serial: 3 });
    assert!((scaled_by(&mut e, &mut dry, STEP) - 0.25).abs() < 1e-4);
    render(&mut e, 2 * STEP, 64);
    render(&mut dry, 2 * STEP, 64);
    // From step 4 its lane holds it.
    assert!((scaled_by(&mut e, &mut dry, STEP) - 0.75).abs() < 1e-4, "the lane's point");
    e.handle(set(0.1));
    e.handle(EngineCommand::Edit(Edit::SetLaneBase { track: 0, lane: 0, value: Some(0.2) }));
    assert!((scaled_by(&mut e, &mut dry, STEP) - 0.75).abs() < 1e-4, "held over both");
    // Back at step 0, with no point in force, the lane leaves it at its
    // base.
    render(&mut e, 16 * STEP - 5 * STEP, 64);
    render(&mut dry, 16 * STEP - 5 * STEP, 64);
    assert!((scaled_by(&mut e, &mut dry, STEP) - 0.2).abs() < 1e-4, "its base");
    e.handle(set(0.4));
    assert!((scaled_by(&mut e, &mut dry, STEP) - 0.4).abs() < 1e-4, "free to set again");
    e.handle(EngineCommand::Edit(Edit::SetLaneBase { track: 0, lane: 0, value: Some(0.3) }));
    assert!((scaled_by(&mut e, &mut dry, STEP) - 0.3).abs() < 1e-4, "a new base, at once");
    render(&mut e, STEP, 64);
    render(&mut dry, STEP, 64);
    assert!((scaled_by(&mut e, &mut dry, STEP) - 0.75).abs() < 1e-4, "held again");
    // A lane let go of leaves it at its base too.
    e.handle(EngineCommand::Edit(Edit::SetLane { track: 0, lane: 0, target: None, ramp: false }));
    assert!((scaled_by(&mut e, &mut dry, STEP) - 0.3).abs() < 1e-4, "its base once the lane is gone");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_instance_that_comes_gets_the_lane_s_value() {
    let dir = temp_dir("engine-replay");
    let file = install(&dir, "replay");
    let mut host = PluginHost::new();
    let target = LaneTarget::Plugin { at: PluginAt::Slot(0), plugin: gain_ref(), id: 10 };
    let (mut e, mut rx) = plugin_engine(&held_note(&[
        Edit::SetLane { track: 0, lane: 0, target: Some(target), ramp: false },
        Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 0, value: Some(0.75) },
    ]));
    let (mut dry, _dry_rx) = plugin_engine(&held_note(&[]));
    e.handle(EngineCommand::SetSlot {
        chain: ChainId::Track(0),
        slot: 0,
        insert: Some(gain_insert(&mut host, &file).0),
    });
    render(&mut e, 1000, 64);
    render(&mut dry, 1000, 64);
    e.handle(EngineCommand::Play);
    dry.handle(EngineCommand::Play);
    assert!((scaled_by(&mut e, &mut dry, 2000) - 0.75).abs() < 1e-4);
    // Another instance (at 0.5) in its place.
    let other = create(&mut host, &file, GAIN, Role::Effect).unwrap();
    let token = other.token();
    e.handle(EngineCommand::SetSlotPlugin { chain: ChainId::Track(0), slot: 0, instance: Box::new(other) });
    assert_eq!(came_back(&mut rx).iter().map(|b| b.0).collect::<Vec<_>>(), ["plugin"]);
    assert!((scaled_by(&mut e, &mut dry, 2000) - 0.75).abs() < 1e-4, "a new instance");
    // One back from a restart that forgot it.
    e.handle(EngineCommand::Lend(token));
    let mut lent = came_back(&mut rx).pop().unwrap().2.unwrap();
    lent.restart().unwrap();
    assert!(lent.set_param(10, 0.1, 9));
    e.handle(EngineCommand::Return(lent));
    assert!((scaled_by(&mut e, &mut dry, 2000) - 0.75).abs() < 1e-4, "back from a restart");
    // And a new insert, which fades in over the old one.
    e.handle(EngineCommand::SetSlot {
        chain: ChainId::Track(0),
        slot: 0,
        insert: Some(gain_insert(&mut host, &file).0),
    });
    render(&mut e, 1000, 64);
    render(&mut dry, 1000, 64);
    assert!((scaled_by(&mut e, &mut dry, 2000) - 0.75).abs() < 1e-4, "a new insert");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_removed_track_leaves_a_master_plugin_at_its_lane_s_base() {
    let dir = temp_dir("engine-remove-lane");
    let file = install(&dir, "remove-lane");
    let mut host = PluginHost::new();
    let target = LaneTarget::Plugin { at: PluginAt::MasterSlot(0), plugin: gain_ref(), id: 10 };
    // Track 0 holds the lane; track 1 is heard.
    let mut edits = vec![Edit::AddTrack(InstrumentKind::Kick)];
    edits.extend(held_note(&[]).into_iter().map(|e| match e {
        Edit::SetStep { pattern, step, value, .. } => Edit::SetStep { track: 1, pattern, step, value },
        e => e,
    }));
    let (mut dry, _dry_rx) = plugin_engine(&edits);
    edits.extend([
        Edit::SetLane { track: 0, lane: 0, target: Some(target), ramp: false },
        Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 0, value: Some(0.75) },
        Edit::SetLaneBase { track: 0, lane: 0, value: Some(0.2) },
    ]);
    let (mut e, _rx) = plugin_engine(&edits);
    e.handle(EngineCommand::SetSlot { chain: ChainId::Master, slot: 0, insert: Some(gain_insert(&mut host, &file).0) });
    for x in [&mut e, &mut dry] {
        render(x, 1000, 64);
        x.handle(EngineCommand::Play);
    }
    assert!((scaled_by(&mut e, &mut dry, 2000) - 0.75).abs() < 1e-4);
    for x in [&mut e, &mut dry] {
        x.handle(EngineCommand::Edit(Edit::RemoveTrack(0)));
    }
    assert!((scaled_by(&mut e, &mut dry, 2000) - 0.2).abs() < 1e-4, "at its base");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_path_waits_for_a_late_plugin() {
    let dir = temp_dir("engine-latency");
    let file = install(&dir, "latency");
    let mut host = PluginHost::new();
    // Track 0 is silent; track 1 plays, as late as track 0's plugin.
    let edits = [
        Edit::AddTrack(InstrumentKind::Plugin(sine_ref())),
        Edit::AddTrack(InstrumentKind::Synth),
        Edit::SetStep { track: 1, pattern: 0, step: 0, value: Some(Step::new(48, 100, 4)) },
    ];
    let (mut r, _r_rx) = plugin_engine(&edits);
    render(&mut r, 2000, 64);
    r.handle(EngineCommand::Play);
    let reference = render(&mut r, 4000, 64);
    let at = first_sound(&reference).unwrap();
    let base = r.latency();
    let insert = || gain_insert(&mut PluginHost::new(), &file).0;
    for (case, late) in
        [("insert", 64), ("bypassed insert", 64), ("instrument", 64), ("attached", 64), ("restarted", 128)]
    {
        let (mut e, mut rx) = plugin_engine(&edits);
        match case {
            "instrument" => {
                let sine = create(&mut host, &file, SINE, Role::Instrument).unwrap();
                e.handle(EngineCommand::SetPlugin { track: 0, instance: Box::new(sine) });
            }
            "attached" => {
                let empty = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), RATE as f32, 120.0));
                e.handle(EngineCommand::SetSlot { chain: ChainId::Track(0), slot: 0, insert: Some(empty) });
                render(&mut e, 1000, 64);
                assert_eq!(e.latency(), base, "none yet");
                let gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
                e.handle(EngineCommand::SetSlotPlugin { chain: ChainId::Track(0), slot: 0, instance: Box::new(gain) });
            }
            "restarted" => {
                let (insert, token) = gain_insert(&mut host, &file);
                e.handle(EngineCommand::SetSlot { chain: ChainId::Track(0), slot: 0, insert: Some(insert) });
                render(&mut e, 1000, 64);
                e.handle(EngineCommand::Lend(token));
                let mut lent = came_back(&mut rx).pop().unwrap().2.unwrap();
                render(&mut e, 1000, 64);
                assert_eq!(e.latency(), base, "lent out");
                lent.restart().unwrap();
                e.handle(EngineCommand::Return(lent));
            }
            _ => e.handle(EngineCommand::SetSlot { chain: ChainId::Track(0), slot: 0, insert: Some(insert()) }),
        }
        if case == "bypassed insert" {
            e.handle(EngineCommand::Edit(Edit::SetSlotParam {
                chain: ChainId::Track(0),
                slot: 0,
                index: 0,
                value: 1.0,
            }));
        }
        render(&mut e, 2000, 64);
        assert_eq!(e.latency(), base + late, "{case}");
        e.handle(EngineCommand::Play);
        let out = render(&mut e, 4000, 64);
        assert_eq!(first_sound(&out), Some(at + late), "{case}");
        for i in 0..2 * (4000 - late) {
            assert!((out[i + 2 * late] - reference[i]).abs() <= 1e-6, "{case}: sample {i}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bypassed_late_plugin_passes_its_input_as_late() {
    let dir = temp_dir("engine-bypass-late");
    let file = install(&dir, "latency");
    let (mut r, _r_rx) = plugin_engine(&held_note(&[]));
    let (mut e, _rx) = plugin_engine(&held_note(&[]));
    e.handle(EngineCommand::SetSlot {
        chain: ChainId::Track(0),
        slot: 0,
        insert: Some(gain_insert(&mut PluginHost::new(), &file).0),
    });
    for x in [&mut r, &mut e] {
        render(x, 2000, 64);
        x.handle(EngineCommand::Play);
    }
    let (wet, dry) = (render(&mut e, 4000, 64), render(&mut r, 4000, 64));
    assert_eq!(first_sound(&wet), first_sound(&dry).map(|f| f + 64));
    for i in 0..2 * (4000 - 64) {
        assert!((wet[i + 128] - 0.5 * dry[i]).abs() <= 1e-6, "halved, 64 frames late: sample {i}");
    }
    // Bypassed, its input comes out as late as its output did; it fades
    // from one to the other in 10 ms.
    e.handle(EngineCommand::Edit(Edit::SetSlotParam { chain: ChainId::Track(0), slot: 0, index: 0, value: 1.0 }));
    let (wet, dry) = (render(&mut e, 4000, 64), render(&mut r, 4000, 64));
    assert_eq!(e.latency(), r.latency() + 64, "still late");
    let late = |i: usize| if i >= 128 { dry[i - 128] } else { 0.0 };
    for (i, w) in wet.iter().enumerate().skip(2 * 500) {
        assert!((w - late(i)).abs() <= 1e-6, "sample {i}: {w} {}", late(i));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_late_plugin_that_fails_passes_its_input_as_late() {
    let dir = temp_dir("engine-late-fail");
    let file = install(&dir, "latefail");
    let (mut r, _r_rx) = plugin_engine(&held_note(&[]));
    let (mut e, _rx) = plugin_engine(&held_note(&[]));
    e.handle(EngineCommand::SetSlot {
        chain: ChainId::Track(0),
        slot: 0,
        insert: Some(gain_insert(&mut PluginHost::new(), &file).0),
    });
    for x in [&mut r, &mut e] {
        render(x, 2000, 64);
        x.handle(EngineCommand::Play);
    }
    // It failed in the warm-up; the track is still in time.
    assert_eq!(e.latency(), r.latency() + 64);
    let (wet, dry) = (render(&mut e, 4000, 64), render(&mut r, 4000, 64));
    assert!(dry.iter().any(|x| x.abs() > 1e-3));
    for (i, w) in wet.iter().enumerate() {
        let late = if i >= 128 { dry[i - 128] } else { 0.0 };
        assert!((w - late).abs() <= 1e-6, "its input, 64 frames late: sample {i}: {w} {late}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_later_than_is_made_up_for_is_refused_or_said() {
    let dir = temp_dir("engine-too-late");
    let file = install(&dir, "latency");
    let mut host = PluginHost::new();
    // At 100 Hz the 64 frames are 640 ms.
    // SAFETY: the test plugin, loaded only through plugin hosts here.
    let made = unsafe { host.create(&file, GAIN, Role::Effect, Mode::Realtime, 100.0, 64) };
    assert_eq!(made.unwrap_err(), "comes 640 ms late, more than the 500 ms made up for");
    assert_eq!((MAX_LATENCY_SECONDS, max_latency(100.0)), (0.5, 50));
    // At 200 Hz each is 320 ms: four on one track are more than its
    // aligner makes up for, besides the gates.
    let rate = 200.0;
    let shared = Arc::new(Shared::new());
    let edits = [Edit::AddTrack(InstrumentKind::Synth), Edit::AddTrack(InstrumentKind::Synth)];
    let mut e = Engine::with_edits(rate as f32, &edits, Arc::clone(&shared), None);
    let (tx, mut rx) = spsc::channel(RETIRE_QUEUE);
    e.attach_retirement(tx);
    assert!(4 * 64 > tinvo::align::longest(rate as f32));
    for slot in 0..4 {
        // SAFETY: as above.
        let gain = unsafe { host.create(&file, GAIN, Role::Effect, Mode::Realtime, rate, 64) }.unwrap();
        let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain_ref()), rate as f32, 120.0));
        assert!(insert.attach(Box::new(gain)).unwrap().is_none());
        e.handle(EngineCommand::SetSlot { chain: ChainId::Track(0), slot, insert: Some(insert) });
        render(&mut e, 64, 16);
        let over = shared.latency_over.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(over, usize::from(slot + 1) * 64 > tinvo::align::longest(rate as f32), "{} inserts", slot + 1);
    }
    assert!(shared.latency_over.load(std::sync::atomic::Ordering::Relaxed));
    for slot in 1..4 {
        e.handle(EngineCommand::SetSlot { chain: ChainId::Track(0), slot, insert: None });
    }
    render(&mut e, 64, 16);
    assert!(64 <= tinvo::align::longest(rate as f32));
    assert!(!shared.latency_over.load(std::sync::atomic::Ordering::Relaxed), "in time again");
    drop(came_back(&mut rx));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The test plugin installed as `stem`, alone in a folder, for renders.
fn catalog_plugins(dir: &Path, stem: &str) -> CatalogPlugins {
    let folder = dir.join(stem);
    std::fs::create_dir_all(&folder).unwrap();
    install(&folder, stem);
    let catalog = scan::scan(&[folder], None, &options(Duration::from_secs(30)));
    // SAFETY: the test plugin, loaded only through plugin hosts here, which
    // takes the thread that made its host as main.
    unsafe { CatalogPlugins::new(catalog) }
}

/// A sine track playing one note.
fn sine_song() -> Project {
    let mut p = Project::new();
    p.apply(Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))).unwrap();
    p.apply(Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(69, 100, 4)) }).unwrap();
    p
}

fn export(p: &Project, plugins: &mut dyn PluginSource) -> Result<Vec<f32>, String> {
    let opts = RenderOptions { loops: 1, ..Default::default() };
    let mut out = Vec::new();
    render_into(p, &[], &opts, plugins, &mut |b| {
        out.extend_from_slice(b);
        Ok(())
    })?;
    Ok(out)
}

#[test]
fn an_export_plays_its_plugins_offline() {
    let dir = temp_dir("render");
    let mut plugins = catalog_plugins(&dir, "render");
    let mut p = sine_song();
    let dry = export(&p, &mut plugins).unwrap();
    assert!(dry.iter().any(|x| x.abs() > 0.05), "the sine plays");
    for f in dry.chunks_exact(2) {
        assert!((f[1] - 0.5 * f[0]).abs() <= 1e-6, "its right channel half its left: {f:?}");
    }
    // A plugin on the master halves it.
    p.apply(Edit::SetSlot { chain: ChainId::Master, slot: 0, kind: Some(EffectKind::Plugin(gain_ref())) }).unwrap();
    let wet = export(&p, &mut plugins).unwrap();
    let n = dry.len().min(wet.len());
    assert!(n > 2 * 6000);
    for i in 2 * 1000..n {
        assert!((wet[i] - 0.5 * dry[i]).abs() <= 1e-5, "sample {i}: {} {}", wet[i], dry[i]);
    }
    // One that asks to be restarted while it renders is, between chunks:
    // the note playing ends with it, the next one plays as it would have.
    let mut twice = sine_song();
    twice.apply(Edit::SetStep { track: 0, pattern: 0, step: 8, value: Some(Step::new(69, 100, 4)) }).unwrap();
    let dry = export(&twice, &mut plugins).unwrap();
    let restarted = export(&twice, &mut catalog_plugins(&dir, "restart")).unwrap();
    let (chunk, step8) = (2 * 4096, 2 * 8 * 6000);
    assert_eq!(restarted.len(), dry.len());
    assert_eq!(restarted[..chunk], dry[..chunk], "as it was until it asked");
    assert!(restarted[chunk..step8].iter().all(|&x| x == 0.0), "its note ended");
    assert!(dry[chunk..step8].iter().any(|&x| x != 0.0));
    assert_eq!(restarted[step8..], dry[step8..], "plays on");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_export_names_the_plugin_that_cannot_play() {
    let dir = temp_dir("render-fail");
    let p = sine_song();
    let mut missing = Project::new();
    missing.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
    let none = PluginRef::new("org.tinvo.test.none").unwrap();
    missing.apply(Edit::SetSlot { chain: ChainId::Track(0), slot: 4, kind: Some(EffectKind::Plugin(none)) }).unwrap();
    let mut plugins = catalog_plugins(&dir, "fine");
    assert_eq!(
        export(&missing, &mut plugins).unwrap_err(),
        "org.tinvo.test.none cannot load: not found in the plugin catalog"
    );
    assert_eq!(export(&p, &mut NoPlugins).unwrap_err(), "org.tinvo.test.sine cannot load: no plugin catalog was read");
    for (stem, why) in [
        ("noactivate", "Test Sine cannot load: "),
        ("hardrt", "Test Sine cannot load: "),
        ("offlinerefuse", "Test Sine cannot load: "),
        ("nostart", "Test Sine failed while rendering"),
        ("procerror", "Test Sine failed while rendering"),
    ] {
        let e = export(&p, &mut catalog_plugins(&dir, stem)).unwrap_err();
        assert!(e.starts_with(why), "{stem}: {e}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_export_lines_up_a_late_plugin_and_refuses_one_that_changes() {
    let dir = temp_dir("render-latency");
    // The sine through a gain, and a synth through a bypassed gain, both
    // on the first beat: the paths 128 and 64 frames late with the test
    // plugin late.
    let mut p = sine_song();
    p.apply(Edit::SetSlot { chain: ChainId::Track(0), slot: 0, kind: Some(EffectKind::Plugin(gain_ref())) }).unwrap();
    p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
    p.apply(Edit::SetStep { track: 1, pattern: 0, step: 0, value: Some(Step::new(48, 100, 4)) }).unwrap();
    p.apply(Edit::SetSlot { chain: ChainId::Track(1), slot: 0, kind: Some(EffectKind::Plugin(gain_ref())) }).unwrap();
    p.apply(Edit::SetSlotParam { chain: ChainId::Track(1), slot: 0, index: 0, value: 1.0 }).unwrap();
    let dry = export(&p, &mut catalog_plugins(&dir, "fixture")).unwrap();
    let late = export(&p, &mut catalog_plugins(&dir, "latency")).unwrap();
    assert_eq!(late.len(), dry.len(), "rendered ahead as far as it is late");
    assert_eq!(first_sound(&late), Some(0), "on the beat");
    for (i, (l, d)) in late.iter().zip(&dry).enumerate() {
        assert!((l - d).abs() <= 1e-6, "sample {i}: {l} {d}");
    }
    let p = sine_song();
    let e = export(&p, &mut catalog_plugins(&dir, "relatency")).unwrap_err();
    assert_eq!(e, "Test Sine changed its latency while rendering");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tinvo_render_plays_plugins_and_names_a_missing_one() {
    let dir = temp_dir("cli-render");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    install(&plugins, "fixture");
    let home = dir.join("home");
    let render = |project: &str, out: &str, extra: &[&str]| {
        let song = dir.join(project);
        Command::new(WORKER)
            .arg("render")
            .arg(&song)
            .arg(dir.join(out))
            .args(extra)
            .env("CLAP_PATH", &plugins)
            .env("HOME", &home)
            .env("XDG_STATE_HOME", home.join("state"))
            .env("LOCALAPPDATA", home.join("local"))
            .env("COMMONPROGRAMFILES", home.join("common"))
            .output()
            .unwrap()
    };
    let song = "tinvo 16\ntrack plugin org.tinvo.test.sine\nname Sine\nstep 0 69 100 4 100 1 0 -\nend\n";
    std::fs::write(dir.join("song.tvo"), song).unwrap();
    let out = render("song.tvo", "song.wav", &["--loops", "1", "--stems"]);
    assert!(out.status.success(), "{out:?}");
    assert!(dir.join("song.wav").exists() && dir.join("song-01-Sine.wav").exists());
    // A plugin that is not installed: nothing is written, and what was
    // there is left as it was.
    let broken = format!("tinvo 16\nmaster_insert 0 plugin org.tinvo.test.none\n{}", &song["tinvo 16\n".len()..]);
    std::fs::write(dir.join("broken.tvo"), broken).unwrap();
    std::fs::write(dir.join("broken.wav"), "before").unwrap();
    let out = render("broken.tvo", "broken.wav", &["--stems"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("org.tinvo.test.none cannot load: not found in the plugin catalog"), "{err}");
    assert_eq!(std::fs::read_to_string(dir.join("broken.wav")).unwrap(), "before");
    assert!(!dir.join("broken-01-Sine.wav").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The editor with the test plugin installed as `stem` and an engine of
/// its own, which `pump` feeds what the editor sends.
struct Editor {
    app: App,
    engine: Box<Engine>,
    commands: Consumer<EngineCommand>,
}

impl Editor {
    fn new(dir: &Path, stem: &str, project: Project) -> Editor {
        let folder = dir.join(stem);
        std::fs::create_dir_all(&folder).unwrap();
        install(&folder, stem);
        let catalog = scan::scan(&[folder], None, &options(Duration::from_secs(30)));
        // SAFETY: the test plugin, loaded only through plugin hosts here,
        // which takes the thread that made its host as main.
        let app = unsafe { App::new(project, None, None, RATE as u32, "t".into()).with_plugin_catalog(catalog) };
        let (engine, commands) = Editor::engine(&app);
        let mut ed = Editor { app, engine, commands };
        ed.reconnect();
        ed
    }

    fn engine(app: &App) -> (Box<Engine>, Consumer<EngineCommand>) {
        let shared = Arc::new(Shared::new());
        let engine = Engine::with_edits(RATE as f32, &app.project.to_edits(), shared, None);
        let (_, commands) = spsc::channel(1);
        (engine, commands)
    }

    /// A new engine for the project as it is, as a reopened device gets.
    fn reconnect(&mut self) {
        let shared = Arc::new(Shared::new());
        self.engine = Engine::with_edits(RATE as f32, &self.app.project.to_edits(), shared.clone(), None);
        let (tx, commands) = spsc::channel(1024);
        let (rtx, rrx) = spsc::channel(RETIRE_QUEUE);
        self.engine.attach_retirement(rtx);
        self.commands = commands;
        self.app.reconnect(EngineLink::new(tx, shared).with_retirement(rrx, 0), RATE as u32, "t".into());
    }

    /// Hands the engine what the editor sent: the plugin commands, by name.
    fn pump(&mut self) -> Vec<&'static str> {
        let mut seen = Vec::new();
        while let Some(c) = self.commands.pop() {
            seen.extend(match &c {
                EngineCommand::SetPlugin { .. } => Some("set_plugin"),
                EngineCommand::SetSlotPlugin { .. } => Some("set_slot_plugin"),
                EngineCommand::Lend(_) => Some("lend"),
                EngineCommand::Return(_) => Some("return"),
                _ => None,
            });
            self.engine.handle(c);
        }
        seen
    }

    fn key(&mut self, k: Key) {
        self.app.handle_key(k);
    }
}

#[test]
fn the_editor_gives_its_engine_an_instance_for_every_plugin() {
    let dir = temp_dir("editor-attach");
    let mut p = Project::new();
    p.apply(Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))).unwrap();
    p.apply(Edit::SetSlot { chain: ChainId::Master, slot: 0, kind: Some(EffectKind::Plugin(gain_ref())) }).unwrap();
    let none = PluginRef::new("org.tinvo.test.none").unwrap();
    p.apply(Edit::SetSlot { chain: ChainId::Track(0), slot: 3, kind: Some(EffectKind::Plugin(none)) }).unwrap();
    let mut ed = Editor::new(&dir, "attach", p);
    assert_eq!(ed.pump(), ["set_plugin", "set_slot_plugin"]);
    // Named by the catalog, kept by the list until the next try.
    assert_eq!(
        ed.app.plugin_problems(),
        ["Plugin slot 4: org.tinvo.test.none cannot load: not found in the plugin catalog"]
    );
    assert!(ed.app.status_text().unwrap().contains("org.tinvo.test.none cannot load"));
    ed.engine.handle(EngineCommand::Edit(Edit::SetStep {
        track: 0,
        pattern: 0,
        step: 0,
        value: Some(Step::new(69, 100, 4)),
    }));
    ed.engine.handle(EngineCommand::Play);
    assert!(first_sound(&render(&mut ed.engine, 2048, 256)).is_some(), "the sine plays");
    // A new engine (another device, another rate) gets instances of its own.
    ed.reconnect();
    assert_eq!(ed.pump(), ["set_plugin", "set_slot_plugin"]);
    assert_eq!(ed.app.plugin_problems().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_editor_s_plugin_list_picks_and_undo_brings_back() {
    let dir = temp_dir("editor-pick");
    let mut ed = Editor::new(&dir, "pick", Project::new());
    assert!(ed.pump().is_empty());
    // a, then 8: the instruments; Enter adds a track playing the first.
    ed.key(Key::Char('a'));
    ed.key(Key::Char('8'));
    assert_eq!(ed.app.mode, EditorMode::Plugins(PluginPicker { target: PluginFor::Track, cursor: 0 }));
    ed.key(Key::Enter);
    assert_eq!(ed.app.mode, EditorMode::Normal);
    assert_eq!(ed.app.project.tracks[0].kind, InstrumentKind::Plugin(sine_ref()));
    assert_eq!(ed.app.status_text(), Some("Added a Test Sine track"));
    assert_eq!(ed.pump(), ["set_plugin"]);
    // The effects, for a slot: the gain, by name.
    ed.app.mode = EditorMode::Plugins(PluginPicker { target: PluginFor::Slot(ChainId::Track(0), 2), cursor: 0 });
    ed.key(Key::Enter);
    assert_eq!(ed.app.status_text(), Some("Plugin slot 3: Test Gain"));
    assert_eq!(ed.pump(), ["set_slot_plugin"]);
    // Picked again, the same plugin is loaded again in place, the slot's
    // settings kept.
    let bypass = Edit::SetSlotParam { chain: ChainId::Track(0), slot: 2, index: 0, value: 1.0 };
    ed.app.project.apply(bypass).unwrap();
    ed.app.mode = EditorMode::Plugins(PluginPicker { target: PluginFor::Slot(ChainId::Track(0), 2), cursor: 0 });
    ed.key(Key::Enter);
    assert_eq!(ed.app.status_text(), Some("Plugin slot 3: Test Gain, loaded again"));
    assert_eq!(ed.pump(), ["set_slot_plugin"]);
    let slot = ed.app.project.chain(ChainId::Track(0)).unwrap()[2].unwrap();
    assert_eq!(slot.params[0], 1.0, "still bypassed");
    // Undone and redone, each place gets a fresh instance.
    ed.key(Key::Char('u'));
    ed.key(Key::Char('u'));
    assert!(ed.app.project.tracks.is_empty());
    assert!(ed.pump().is_empty());
    ed.key(Key::Char('U'));
    ed.key(Key::Char('U'));
    assert_eq!(ed.pump(), ["set_plugin", "set_slot_plugin"]);
    // Deleted and brought back, the track gets a fresh instance.
    ed.key(Key::Char('D'));
    ed.key(Key::Char('y'));
    assert!(ed.app.project.tracks.is_empty());
    ed.key(Key::Char('u'));
    assert_eq!(ed.pump(), ["set_plugin", "set_slot_plugin"]);
    // Esc closes the list.
    ed.key(Key::Char('&'));
    assert!(matches!(ed.app.mode, EditorMode::Plugins(_)));
    ed.key(Key::Esc);
    assert_eq!(ed.app.mode, EditorMode::Normal);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_picked_plugin_that_cannot_load_changes_nothing() {
    let dir = temp_dir("editor-noload");
    let mut ed = Editor::new(&dir, "noactivate", Project::new());
    ed.key(Key::Char('a'));
    ed.key(Key::Char('8'));
    ed.key(Key::Enter);
    assert!(ed.app.project.tracks.is_empty());
    let status = ed.app.status_text().unwrap();
    assert!(status.starts_with("Test Sine cannot load: "), "{status}");
    assert!(ed.pump().is_empty());
    ed.key(Key::Char('u'));
    assert!(ed.app.status_text() == Some("Nothing to undo"), "no edit was made");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_an_engine_a_picked_plugin_is_still_made_first() {
    let dir = temp_dir("editor-offline");
    let folder = dir.join("noactivate");
    std::fs::create_dir_all(&folder).unwrap();
    install(&folder, "noactivate");
    let catalog = scan::scan(&[folder], None, &options(Duration::from_secs(30)));
    // SAFETY: as in `Editor::new`.
    let mut app = unsafe { App::new(Project::new(), None, None, RATE as u32, "t".into()).with_plugin_catalog(catalog) };
    for k in [Key::Char('a'), Key::Char('8'), Key::Enter] {
        app.handle_key(k);
    }
    assert!(app.project.tracks.is_empty());
    let [problem] = app.plugin_problems() else { panic!("{:?}", app.plugin_problems()) };
    assert!(problem.starts_with("Test Sine cannot load: "), "kept for the list: {problem}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_editor_lets_go_of_the_instances_that_came_back() {
    let dir = temp_dir("editor-gone");
    let mut p = Project::new();
    p.apply(Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))).unwrap();
    let mut ed = Editor::new(&dir, "gone", p);
    assert_eq!(ed.pump(), ["set_plugin"]);
    assert!(ed.app.plugins_playing());
    ed.key(Key::Char('D'));
    ed.key(Key::Char('y'));
    assert!(ed.app.project.tracks.is_empty());
    ed.pump();
    render(&mut ed.engine, 8192, 256);
    ed.app.tick();
    assert!(!ed.app.plugins_playing(), "no plugin left to keep the editor busy");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_plugin_list_s_cursor_stays_on_its_rows() {
    let dir = temp_dir("editor-cursor");
    let mut ed = Editor::new(&dir, "cursor", Project::new());
    ed.app.mode = EditorMode::Plugins(PluginPicker { target: PluginFor::Track, cursor: 50 });
    ed.key(Key::Enter);
    assert_eq!(ed.app.project.tracks[0].kind, InstrumentKind::Plugin(sine_ref()), "the last row picked");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_editor_restarts_a_plugin_that_asks_and_one_that_fails() {
    let dir = temp_dir("editor-restart");
    let mut p = Project::new();
    p.apply(Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))).unwrap();
    let mut ed = Editor::new(&dir, "restart", p);
    assert_eq!(ed.pump(), ["set_plugin"]);
    render(&mut ed.engine, 64, 64);
    // Asked for in its first call: lent, restarted here, given back.
    ed.app.tick();
    assert_eq!(ed.pump(), ["lend"]);
    ed.app.tick();
    assert_eq!(ed.pump(), ["return"]);
    assert!(ed.app.plugin_problems().is_empty());
    render(&mut ed.engine, 64, 64);

    let mut p = Project::new();
    p.apply(Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))).unwrap();
    let mut ed = Editor::new(&dir, "procerror", p);
    assert_eq!(ed.pump(), ["set_plugin"]);
    // It fails on its second call: restarted once...
    render(&mut ed.engine, 128, 64);
    ed.app.tick();
    assert_eq!(ed.pump(), ["lend"]);
    ed.app.tick();
    assert_eq!(ed.pump(), ["return"]);
    assert_eq!(ed.app.plugin_problems(), ["Test Sine failed while playing; restarting it"]);
    // Restarted, it plays on.
    render(&mut ed.engine, 128, 64);
    ed.app.tick();
    assert!(ed.pump().is_empty());
    assert_eq!(ed.app.plugin_problems().len(), 1);

    // One that fails, asking for a restart, every time and from the main
    // thread after: restarted once, its failure and its requests served as
    // one, then left silent however often it asks.
    let mut p = Project::new();
    p.apply(Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))).unwrap();
    let mut ed = Editor::new(&dir, "procerrors", p);
    assert_eq!(ed.pump(), ["set_plugin"]);
    render(&mut ed.engine, 128, 64);
    ed.app.tick();
    assert_eq!(ed.pump(), ["lend"]);
    ed.app.tick();
    assert_eq!(ed.pump(), ["return"]);
    assert_eq!(ed.app.plugin_problems(), ["Test Sine failed while playing; restarting it"]);
    // Asked again once back, restarted again: lent once more, and what it
    // asks while lent is answered by that restart.
    ed.app.tick();
    assert_eq!(ed.pump(), ["lend"]);
    ed.app.tick();
    assert_eq!(ed.pump(), ["return"]);
    for _ in 0..3 {
        render(&mut ed.engine, 128, 64);
        ed.app.tick();
        assert!(ed.pump().is_empty(), "not lent again");
    }
    assert_eq!(
        ed.app.plugin_problems(),
        ["Test Sine failed while playing; restarting it", "Test Sine failed again and stays silent"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_editor_exports_through_plugins_of_its_own() {
    let dir = temp_dir("editor-export");
    let mut p = Project::new();
    p.apply(Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))).unwrap();
    p.apply(Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(69, 100, 4)) }).unwrap();
    let mut ed = Editor::new(&dir, "export", p.clone());
    ed.pump();
    ed.app.export_loops = 1;
    let out = dir.join("out.wav");
    ed.app.export_to(&out);
    assert!(ed.app.status_text().unwrap().starts_with("Exported"), "{:?}", ed.app.status_text());
    assert!(std::fs::metadata(&out).unwrap().len() > 1000);
    // The editor's own instances are untouched: nothing to serve.
    ed.app.tick();
    assert!(ed.pump().is_empty());
    // Without a catalog there is nothing to play them with.
    let mut bare = App::new(p, None, None, RATE as u32, "t".into());
    bare.export_to(&dir.join("bare.wav"));
    assert_eq!(bare.status_text(), Some("Not exported: no plugin catalog was read"));
    assert!(!dir.join("bare.wav").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_an_engine_the_editor_names_the_plugins_not_in_its_catalog() {
    let dir = temp_dir("editor-missing");
    let folder = dir.join("missing");
    std::fs::create_dir_all(&folder).unwrap();
    install(&folder, "missing");
    let catalog = scan::scan(&[folder], None, &options(Duration::from_secs(30)));
    let mut p = Project::new();
    p.apply(Edit::AddTrack(InstrumentKind::Plugin(sine_ref()))).unwrap();
    p.apply(Edit::AddTrack(InstrumentKind::Plugin(PluginRef::new("org.tinvo.test.gone").unwrap()))).unwrap();
    // SAFETY: as in `Editor::new`.
    let app = unsafe { App::new(p, None, None, RATE as u32, "t".into()).with_plugin_catalog(catalog) };
    assert_eq!(app.plugin_problems(), ["Plugin: org.tinvo.test.gone is not in the plugin catalog"]);
    assert!(app.status_text().unwrap().contains("(& lists plugins)"), "{:?}", app.status_text());
    let _ = std::fs::remove_dir_all(&dir);
}
