//! Real CLAP plugin files: the test plugin in `examples/clap_fixture.rs`,
//! copied under the names that make it crash, hang or fail, read by TINVO's
//! own `scan-clap` child processes; and its instances played in this
//! process, on their own and in the engine.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tinvo::clap::ffi::ClapPlugin;
use tinvo::clap::host::{Instance, MAX_EVENTS, Mode, PluginHost, Request, Role};
use tinvo::clap::scan::{self, Catalog, Options};
use tinvo::clap::{PluginRef, Roles};
use tinvo::engine::{ChainId, Command as EngineCommand, Edit, Engine, RETIRE_QUEUE, Retired, Shared, Step};
use tinvo::insert::{EffectKind, Insert};
use tinvo::instrument::InstrumentKind;
use tinvo::rt::spsc::{self, Consumer};

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
    let d = std::env::temp_dir().join(format!("tinvo-clap-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    let (tx, mut commands) = spsc::channel(128);
    let (mut back, retired) = spsc::channel(256);
    let mut link = EngineLink::new(tx, Arc::new(Shared::new())).with_retirement(retired, tinvo::engine::HELD_FOR_GOOD);
    // Samples on their way, till two of the queue's 256 places are left.
    let sample = Arc::new(tinvo::sample::Sample::from_mono(&[0.5; 16], 48_000).unwrap());
    for _ in tinvo::engine::HELD_FOR_GOOD..254 {
        link.send(EngineCommand::SetSample { track: 0, sample: Some(sample.clone()) });
    }
    let sent = |c: &mut Consumer<EngineCommand>| std::iter::from_fn(|| c.pop()).collect::<Vec<_>>();
    assert_eq!(sent(&mut commands).len(), 254 - tinvo::engine::HELD_FOR_GOOD);
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
fn a_plugin_insert_s_tail_keeps_its_effects_going() {
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
