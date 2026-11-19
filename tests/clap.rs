//! Scanning real CLAP plugin files: the test plugin in
//! `examples/clap_fixture.rs`, copied under the names that make it crash,
//! hang or fail, read by TINVO's own `scan-clap` child processes.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tinvo::clap::Roles;
use tinvo::clap::scan::{self, Catalog, Options};

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
