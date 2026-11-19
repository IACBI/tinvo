//! Finding CLAP plugins and reading what each file offers.
//!
//! The folders searched are the ones CLAP's `entry.h` names for the system,
//! then each folder in `CLAP_PATH`. They are searched recursively for
//! `.clap` files (on macOS, `.clap` bundles), without following links to
//! folders, within limits of depth and size. Each file new or changed since
//! the cache was written is read by a child process, `tinvo scan-clap FILE
//! RESULT`: it loads the file, lists its factory's descriptors, calls its
//! `deinit` and only then writes the result file. A child that crashes, hangs past
//! its time or writes no readable result marks the file as not loadable,
//! with the reason, until the file changes or a rescan is asked for. Plugin
//! code never runs in the process that asked.

use super::{PluginInfo, Roles};
use crate::fsutil;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// How deep below a root the search goes.
pub const MAX_DEPTH: usize = 16;
/// How many folder entries one search looks at.
pub const MAX_VISITED: usize = 100_000;
/// How many plugin files one search takes.
pub const MAX_FILES: usize = 10_000;
/// How long one file's child process may take.
pub const PER_FILE: Duration = Duration::from_secs(10);
/// How long one scan may take in all; files left over are read next time.
pub const BUDGET: Duration = Duration::from_secs(120);

/// The first line of a child's result file.
const RESULT_HEADER: &str = "tinvo-clap-scan 1";
/// The first line of the cache: its format, and the system and processor
/// the files were read on (a plugin file can hold code for several).
fn cache_header() -> String {
    // 2: plugin ids are taken whole or not at all, so a cache of 1 may
    // hold ids cut short.
    format!("tinvo-plugins 2 {} {}", std::env::consts::OS, std::env::consts::ARCH)
}
const MAX_RESULT_BYTES: usize = 1 << 20;
const MAX_CACHE_BYTES: u64 = 16 << 20;
/// The exit code of a child that read its file but could not write the
/// result: the scanner's failure, not the plugin's, so tried again later.
const RESULT_UNWRITTEN: u8 = 113;
/// How many files of a macOS bundle's `Contents/MacOS` its fingerprint
/// takes in.
const MAX_BUNDLE_FILES: usize = 64;

/// The folders CLAP names for this system, then those in `CLAP_PATH`.
pub fn default_roots() -> Vec<PathBuf> {
    roots_from(|name| std::env::var_os(name))
}

/// [`default_roots`], with the environment read through `var`.
fn roots_from(var: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let get = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut roots = Vec::new();
    if cfg!(windows) {
        roots.extend(get("COMMONPROGRAMFILES").map(|p| p.join("CLAP")));
        roots.extend(get("LOCALAPPDATA").map(|p| p.join("Programs").join("Common").join("CLAP")));
    } else if cfg!(target_os = "macos") {
        roots.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        roots.extend(get("HOME").map(|p| p.join("Library").join("Audio").join("Plug-Ins").join("CLAP")));
    } else {
        roots.extend(get("HOME").map(|p| p.join(".clap")));
        roots.push(PathBuf::from("/usr/lib/clap"));
    }
    if let Some(list) = var("CLAP_PATH") {
        roots.extend(std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    roots
}

/// What a search found.
#[derive(Debug, Default)]
pub struct Found {
    /// The plugin files, each once, in the order found.
    pub files: Vec<PathBuf>,
    /// Folders and entries not searched to the end (a folder that could not
    /// be read, one too deep, or a limit reached): what the cache knows
    /// under them is kept.
    pub incomplete: Vec<PathBuf>,
    /// What went wrong, for the user.
    pub notes: Vec<String>,
}

/// Whether `path` is named like a plugin file.
fn is_clap(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("clap"))
}

/// Searches `roots` for plugin files.
pub fn find(roots: &[PathBuf]) -> Found {
    let mut found = Found::default();
    let mut seen_files = HashSet::new();
    let mut seen_roots = HashSet::new();
    let mut visited = 0;
    let mut limited = false;
    for root in roots {
        let Ok(root) = std::path::absolute(root) else { continue };
        if !seen_roots.insert(root.clone()) {
            continue;
        }
        if limited {
            found.incomplete.push(root);
            continue;
        }
        let mut stack = vec![(root.clone(), 0)];
        'walk: while let Some((dir, depth)) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir == root => break,
                Err(e) => {
                    found.notes.push(format!("{} could not be read: {e}", dir.display()));
                    found.incomplete.push(dir);
                    continue;
                }
            };
            // Counted as they are read, so a huge folder is not read whole.
            let mut names = Vec::new();
            for entry in entries {
                if visited == MAX_VISITED {
                    limited = true;
                    break;
                }
                visited += 1;
                match entry {
                    Ok(e) => names.push(e),
                    Err(e) => {
                        found.notes.push(format!("{} could not be read to the end: {e}", dir.display()));
                        found.incomplete.push(dir.clone());
                        break;
                    }
                }
            }
            names.sort_by_key(|e| e.file_name());
            let before = stack.len();
            for entry in names {
                if found.files.len() == MAX_FILES {
                    limited = true;
                    break 'walk;
                }
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    found.incomplete.push(path);
                    continue;
                };
                // Links to folders are not followed, so no search loops; a
                // link to a plugin file is taken.
                let (is_dir, is_file) = if kind.is_symlink() {
                    (false, std::fs::metadata(&path).is_ok_and(|m| m.is_file()))
                } else {
                    (kind.is_dir(), kind.is_file())
                };
                let candidate =
                    if cfg!(target_os = "macos") { is_dir && is_clap(&path) } else { is_file && is_clap(&path) };
                if candidate {
                    if path.to_str().is_none() {
                        found.notes.push(format!("{} is skipped: its path is not valid Unicode", path.display()));
                        continue;
                    }
                    let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    if seen_files.insert(key) {
                        found.files.push(path);
                    }
                } else if is_dir {
                    if depth < MAX_DEPTH {
                        stack.push((path, depth + 1));
                    } else {
                        found.incomplete.push(path);
                    }
                }
            }
            if limited {
                break;
            }
            // Folders in name order: the stack takes them last first.
            stack[before..].reverse();
        }
        if limited {
            found.notes.push(format!(
                "the search stopped after {MAX_VISITED} folder entries or {MAX_FILES} plugin files, in {}",
                root.display()
            ));
            found.incomplete.push(root);
        }
    }
    found
}

/// What identifies a version of the plugin file at `path`: the size and
/// modification time of the file (on Unix its inode and change time too),
/// or on macOS of a bundle's `Info.plist` and the files in its
/// `Contents/MacOS`. A file replaced by one of the same size and times is
/// not noticed; `tinvo plugins --rescan` reads it again.
pub fn fingerprint(path: &Path) -> Option<String> {
    let stamp = |p: &Path| {
        let m = std::fs::metadata(p).ok()?;
        let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        #[cfg(unix)]
        let more = {
            use std::os::unix::fs::MetadataExt;
            format!(":{}:{}.{}", m.ino(), m.ctime(), m.ctime_nsec())
        };
        #[cfg(not(unix))]
        let more = "";
        Some(format!("{}:{t}{more}", m.len()))
    };
    if !path.is_dir() {
        return stamp(path);
    }
    let contents = path.join("Contents");
    let mut parts = vec![stamp(&contents.join("Info.plist")).unwrap_or_default()];
    if let Some(exe) = super::library::bundle_executable(path) {
        parts.push(format!("exe={}", stamp(&exe)?));
    }
    if let Ok(entries) = std::fs::read_dir(contents.join("MacOS")) {
        let mut files: Vec<PathBuf> = entries.take(4097).filter_map(Result::ok).map(|e| e.path()).collect();
        if files.len() > 4096 {
            // Too many to know which come first: never taken from the cache.
            let now = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            parts.push(format!("unlisted@{now}"));
        }
        files.sort();
        for f in files.iter().take(MAX_BUNDLE_FILES) {
            // An entry that cannot be read (a broken link) is for the child
            // to judge, not a reason to skip the bundle.
            let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            parts.push(format!("{name}={}", stamp(f).unwrap_or_else(|| "-".into())));
        }
    }
    Some(parts.join(";"))
}

/// Writes `text` as one field of a line: tabs, line breaks and backslashes
/// escaped.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Reads a field [`escape`] wrote; `None` for a stray backslash.
fn unescape(field: &str) -> Option<String> {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            't' => '\t',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(out)
}

/// A plugin as one line: `plugin`, its id, name, vendor, version and
/// description, then its features.
fn plugin_line(p: &PluginInfo) -> String {
    let mut fields = vec!["plugin".to_string()];
    fields.extend([&p.id, &p.name, &p.vendor, &p.version, &p.description].map(|f| escape(f)));
    fields.extend(p.features.iter().map(|f| escape(f)));
    fields.join("\t")
}

/// Reads a line [`plugin_line`] wrote, without its first word.
fn parse_plugin(fields: &[&str]) -> Option<PluginInfo> {
    let mut text = fields.iter().map(|f| unescape(f));
    let mut next = || text.next().flatten();
    let (id, name, vendor, version, description) = (next()?, next()?, next()?, next()?, next()?);
    let features: Vec<String> = fields[5..].iter().map(|f| unescape(f)).collect::<Option<_>>()?;
    if !super::is_valid_id(&id) || name.is_empty() {
        return None;
    }
    let roles = Roles::from_features(&features);
    Some(PluginInfo { id, name, vendor, version, description, features, roles })
}

/// The hidden command `tinvo scan-clap FILE RESULT`, run as a child of a
/// scan: reads the plugin file at `file` and writes what it offers to
/// `result`. Returns the exit code.
///
/// # Safety
/// It runs the plugin's code in this process, which may do anything: call
/// it only as the child a scan starts, with nothing else running.
pub unsafe fn worker(file: &Path, result: &Path) -> u8 {
    quiet_crashes();
    contain_children();
    // SAFETY: the caller's promise; this process exists for this file.
    let read = unsafe { super::library::Dso::open(file) }.and_then(|dso| dso.plugins());
    // Written after the file's deinit: a crash there is a failure too.
    let mut text = format!("{RESULT_HEADER}\n");
    match read {
        Ok(plugins) => {
            for p in &plugins {
                text.push_str(&plugin_line(p));
                text.push('\n');
            }
        }
        Err(e) => text.push_str(&format!("error\t{}\n", escape(&e))),
    }
    text.push_str("end\n");
    if text.len() > MAX_RESULT_BYTES {
        text = format!("{RESULT_HEADER}\nerror\tdescribes its plugins in more than 1 MiB\nend\n");
    }
    match std::fs::write(result, text) {
        Ok(()) => 0,
        Err(_) => RESULT_UNWRITTEN,
    }
}

/// Keeps Windows from showing a dialog when a plugin crashes in the
/// child, which would hold it until its time runs out.
fn quiet_crashes() {
    #[cfg(windows)]
    {
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
        unsafe extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
        }
        // SAFETY: sets this process's error mode only.
        unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX) };
    }
}

/// On Windows, puts the child in a job that ends every process in it once
/// the child's last handle to it closes, as the child ends: a helper a
/// plugin starts does not outlive the scan. Done before any plugin code
/// runs. (On Unix the parent gives the child a process group of its own.)
fn contain_children() {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        /// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`, with its
        /// `JOBOBJECT_BASIC_LIMIT_INFORMATION` and `IO_COUNTERS` inline.
        #[repr(C)]
        #[derive(Default)]
        struct ExtendedLimits {
            per_process_user_time_limit: i64,
            per_job_user_time_limit: i64,
            limit_flags: u32,
            minimum_working_set_size: usize,
            maximum_working_set_size: usize,
            active_process_limit: u32,
            affinity: usize,
            priority_class: u32,
            scheduling_class: u32,
            io_counters: [u64; 6],
            process_memory_limit: usize,
            job_memory_limit: usize,
            peak_process_memory_used: usize,
            peak_job_memory_used: usize,
        }
        const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
        const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
        unsafe extern "system" {
            fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
            fn SetInformationJobObject(job: *mut c_void, class: i32, info: *const c_void, len: u32) -> i32;
            fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
            fn GetCurrentProcess() -> *mut c_void;
        }
        let limits = ExtendedLimits { limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, ..ExtendedLimits::default() };
        // SAFETY: a new unnamed job, given a properly laid out structure and
        // its size; its handle stays open, on purpose, until this process
        // ends. A failure leaves the child as it was.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if !job.is_null()
                && SetInformationJobObject(
                    job,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    (&raw const limits).cast(),
                    size_of::<ExtendedLimits>() as u32,
                ) != 0
            {
                AssignProcessToJobObject(job, GetCurrentProcess());
            }
        }
    }
}

/// Reads a child's result file: the plugins, or why the file did not load.
/// `None` for anything else, such as a result cut short or with lines after
/// its end.
fn parse_result(text: &str) -> Option<Result<Vec<PluginInfo>, String>> {
    let lines: Vec<&str> = text.lines().collect();
    let (&RESULT_HEADER, rest) = lines.split_first()? else { return None };
    let (&"end", body) = rest.split_last()? else { return None };
    if let [line] = body {
        if let Some(why) = line.strip_prefix("error\t") {
            return Some(Err(unescape(why)?));
        }
    }
    let mut plugins = Vec::new();
    for line in body {
        let fields: Vec<&str> = line.split('\t').collect();
        let ["plugin", rest @ ..] = &fields[..] else { return None };
        plugins.push(parse_plugin(rest)?);
    }
    Some(Ok(plugins))
}

/// What a scan learned about one plugin file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    /// [`fingerprint`] when it was read.
    pub fingerprint: String,
    /// The plugins it offers, or why it could not be read.
    pub result: Result<Vec<PluginInfo>, String>,
}

/// How a child reading one file ended.
enum Outcome {
    /// It read the file: the plugins, or why the file does not load.
    Read(Result<Vec<PluginInfo>, String>),
    /// It ran out of time and was ended: its own, or the scan's.
    TimedOut { by_budget: bool },
    /// Something went wrong on the scanner's side: the file is left for
    /// another time.
    Retry(String),
}

/// Adds `text` to the catalog's notes once.
fn note(catalog: &mut Catalog, text: String) {
    if !catalog.notes.contains(&text) {
        catalog.notes.push(text);
    }
}

/// A folder of the scan's own for the children's results, which only this
/// user can open (temporary folders are shared on some systems).
fn private_dir() -> std::io::Result<PathBuf> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    let base = std::env::temp_dir();
    let nanos = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let mut last = None;
    for n in 0..64 {
        let dir = base.join(format!("tinvo-clap-scan-{}-{nanos}-{n}", std::process::id()));
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free name")))
}

/// Whether this system lets a scan see that a child has ended without
/// reaping it. Until it is reaped, its process id, which is also its
/// group's, cannot be reused, so its group can be ended safely.
const GROUP_PINNED: bool = cfg!(any(target_os = "linux", target_os = "android", target_os = "macos"));

/// Whether `child` has ended; where [`GROUP_PINNED`], without reaping it.
fn has_ended(child: &mut Child) -> std::io::Result<bool> {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        use std::ffi::c_int;
        /// `siginfo_t`: 128 bytes on these systems.
        #[repr(C, align(8))]
        struct SigInfo([u8; 128]);
        unsafe extern "C" {
            fn waitid(idtype: c_int, id: u32, info: *mut SigInfo, options: c_int) -> c_int;
        }
        const P_PID: c_int = 1;
        const WNOHANG: c_int = 1;
        const WEXITED: c_int = 4;
        #[cfg(target_os = "macos")]
        const WNOWAIT: c_int = 0x20;
        #[cfg(not(target_os = "macos"))]
        const WNOWAIT: c_int = 0x0100_0000;
        // `si_pid` follows three ints, and on 64-bit Linux the padding that
        // aligns the union it is in.
        const SI_PID: usize = if cfg!(all(not(target_os = "macos"), target_pointer_width = "64")) { 16 } else { 12 };
        let mut info = SigInfo([0; 128]);
        // SAFETY: a child of this process and room for a `siginfo_t`; with
        // WNOHANG a child still running leaves `si_pid` 0.
        if unsafe { waitid(P_PID, child.id(), &mut info, WEXITED | WNOHANG | WNOWAIT) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pid = i32::from_ne_bytes(info.0[SI_PID..SI_PID + 4].try_into().unwrap_or_default());
        Ok(pid != 0)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        child.try_wait().map(|s| s.is_some())
    }
}

/// Ends every process left in a child's group on Unix, helpers a plugin
/// started included, when `pinned`: the child is not yet reaped, so the
/// group id is still its own.
fn end_group(child: &Child, pinned: bool) {
    #[cfg(unix)]
    if pinned {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        // SAFETY: signals the group the child leads (`process_group(0)`),
        // whose id the unreaped child keeps from reuse.
        unsafe { kill(-(child.id() as i32), SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = (child, pinned);
}

/// Reads the plugin file at `path` in a child process (`worker scan-clap`),
/// its result going to a file in `dir`. The child gets `per_file`, or less
/// if the scan must end at `scan_end` before.
fn scan_file(
    worker: &Path,
    path: &Path,
    dir: &Path,
    per_file: Duration,
    scan_end: Instant,
    cancel: &AtomicBool,
) -> Outcome {
    let (file, result) = match fsutil::create_temp_beside(&dir.join("result")) {
        Ok(r) => r,
        Err(e) => return Outcome::Retry(format!("no room for a scan's result: {e}")),
    };
    drop(file);
    if Instant::now() >= scan_end {
        let _ = std::fs::remove_file(&result);
        return Outcome::TimedOut { by_budget: true };
    }
    let mut command = Command::new(worker);
    command.arg("scan-clap").arg(path).arg(&result).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&result);
            return Outcome::Retry(format!("the scanner could not be started: {e}"));
        }
    };
    let own_deadline = Instant::now() + per_file;
    let deadline = own_deadline.min(scan_end);
    let status = loop {
        match has_ended(&mut child) {
            Ok(true) => {
                // Helpers it left behind, then the child itself.
                end_group(&child, GROUP_PINNED);
                break child.wait().map_err(|e| Some(e.to_string()));
            }
            Ok(false) if Instant::now() < deadline && !cancel.load(Ordering::Relaxed) => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Ok(false) => {
                end_group(&child, true);
                let _ = child.kill();
                let _ = child.wait();
                break Err(None);
            }
            Err(e) => {
                // Its group is left alone, as whether the child is still
                // this process's is unknown; one already reaped elsewhere
                // (ECHILD) is not signalled at all, as its id may be reused.
                const ECHILD: i32 = 10;
                if !(cfg!(unix) && e.raw_os_error() == Some(ECHILD)) {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                break Err(Some(e.to_string()));
            }
        }
    };
    let text = match &status {
        Ok(s) if s.success() => Some(fsutil::read_text(&result, MAX_RESULT_BYTES as u64)),
        _ => None,
    };
    let _ = std::fs::remove_file(&result);
    match status {
        Err(None) => Outcome::TimedOut { by_budget: scan_end < own_deadline || cancel.load(Ordering::Relaxed) },
        Err(Some(e)) => Outcome::Retry(format!("the scanner lost track of its child: {e}")),
        Ok(s) if s.code() == Some(i32::from(RESULT_UNWRITTEN)) => {
            Outcome::Retry("the scanner could not write a result".into())
        }
        Ok(s) if !s.success() => Outcome::Read(Err(format!("crashed while loading ({})", exit_reason(s)))),
        Ok(_) => match text {
            Some(Ok(text)) => {
                Outcome::Read(parse_result(&text).unwrap_or_else(|| Err("gave no readable result".into())))
            }
            Some(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                Outcome::Read(Err("gave no readable result".into()))
            }
            Some(Err(e)) => Outcome::Retry(format!("the scanner could not read a result: {e}")),
            None => Outcome::Retry("the scanner could not read a result".into()),
        },
    }
}

/// How a child ended: its exit code (on Windows in hex, as crash codes such
/// as `0xC0000005` are known), or on Unix the signal that ended it.
fn exit_reason(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return format!("signal {signal}");
    }
    match status.code() {
        Some(code) if cfg!(windows) => format!("exit code 0x{:08X}", code as u32),
        Some(code) => format!("exit code {code}"),
        None => "no exit code".into(),
    }
}

/// Reads the cache at `path`; nothing for a missing, damaged or foreign one.
pub fn read_cache(path: &Path) -> Vec<Entry> {
    let Ok(text) = fsutil::read_text(path, MAX_CACHE_BYTES) else { return Vec::new() };
    parse_cache(&text).unwrap_or_default()
}

fn parse_cache(text: &str) -> Option<Vec<Entry>> {
    let mut lines = text.lines();
    if lines.next()? != cache_header() {
        return None;
    }
    let mut entries: Vec<Entry> = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields[..] {
            ["file", path, fingerprint] => entries.push(Entry {
                path: PathBuf::from(unescape(path)?),
                fingerprint: unescape(fingerprint)?,
                result: Ok(Vec::new()),
            }),
            ["failed", why] => entries.last_mut()?.result = Err(unescape(why)?),
            ["plugin", ..] => match &mut entries.last_mut()?.result {
                Ok(list) => list.push(parse_plugin(&fields[1..])?),
                Err(_) => return None,
            },
            _ => return None,
        }
    }
    Some(entries)
}

fn cache_text(entries: &[Entry]) -> String {
    let mut text = format!("{}\n", cache_header());
    for e in entries {
        // Paths that are not Unicode are never found, so never cached.
        let Some(path) = e.path.to_str() else { continue };
        text.push_str(&format!("file\t{}\t{}\n", escape(path), escape(&e.fingerprint)));
        match &e.result {
            Ok(plugins) => {
                for p in plugins {
                    text.push_str(&plugin_line(p));
                    text.push('\n');
                }
            }
            Err(why) => text.push_str(&format!("failed\t{}\n", escape(why))),
        }
    }
    text
}

/// Writes the cache at `path`, its folder made if need be.
pub fn write_cache(path: &Path, entries: &[Entry]) -> Result<(), String> {
    let text = cache_text(entries);
    if text.len() as u64 > MAX_CACHE_BYTES {
        return Err("the plugin cache would be too large to keep".into());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fsutil::write_atomic(path, text.as_bytes()).map_err(|e| e.to_string())
}

/// How a scan runs.
#[derive(Clone, Debug)]
pub struct Options {
    /// The TINVO executable the children run.
    pub worker: PathBuf,
    /// How long one child may take.
    pub per_file: Duration,
    /// How long the whole scan may take, the search included. A child
    /// started near its end gets only what is left, and one ended by it
    /// leaves its file for the next scan.
    pub budget: Duration,
    /// Reads every file again, whatever the cache says.
    pub rescan: bool,
}

/// What a scan knows.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    /// Each plugin file, in the order found, then those kept from the cache
    /// under folders that could not be searched to the end.
    pub entries: Vec<Entry>,
    /// Files left for the next scan: its time ran out, a file changed while
    /// it was read, or the scanner failed.
    pub left: usize,
    pub notes: Vec<String>,
}

impl Catalog {
    /// Every plugin, with the file that offers it. A plugin id offered by
    /// more than one file is listed once, from the first.
    pub fn plugins(&self) -> Vec<(&Path, &PluginInfo)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for e in &self.entries {
            for p in e.result.iter().flatten() {
                if seen.insert(p.id.as_str()) {
                    out.push((e.path.as_path(), p));
                }
            }
        }
        out
    }

    /// The plugin with `id`, with the file that offers it (the first, if
    /// more than one does).
    pub fn find(&self, id: &str) -> Option<(&Path, &PluginInfo)> {
        self.entries.iter().find_map(|e| Some((e.path.as_path(), e.result.as_ref().ok()?.iter().find(|p| p.id == id)?)))
    }

    /// The files that could not be read, with why.
    pub fn failures(&self) -> Vec<(&Path, &str)> {
        self.entries.iter().filter_map(|e| e.result.as_ref().err().map(|w| (e.path.as_path(), w.as_str()))).collect()
    }

    /// Plugin ids offered by more than one file: the id, the file used and
    /// the one passed over.
    pub fn duplicates(&self) -> Vec<(&str, &Path, &Path)> {
        let mut first: HashMap<&str, &Path> = HashMap::new();
        let mut out = Vec::new();
        for e in &self.entries {
            for p in e.result.iter().flatten() {
                match first.get(p.id.as_str()) {
                    Some(&used) if used != e.path => out.push((p.id.as_str(), used, e.path.as_path())),
                    Some(_) => {}
                    None => {
                        first.insert(p.id.as_str(), &e.path);
                    }
                }
            }
        }
        out
    }
}

/// Scans CLAP's folders ([`default_roots`]) with TINVO's own program as
/// the scanner and the cache in TINVO's state folder: the catalog and the
/// folders searched.
/// Setting `cancel` ends it soon, leaving what is left for the next scan.
pub fn installed(rescan: bool, cancel: &AtomicBool) -> Result<(Catalog, Vec<PathBuf>), String> {
    let worker = std::env::current_exe().map_err(|e| format!("cannot find TINVO's own program to scan with: {e}"))?;
    let cache = crate::autosave::state_dir().map(|d| d.join("plugins.txt"));
    let roots = default_roots();
    let options = Options { worker, per_file: PER_FILE, budget: BUDGET, rescan };
    Ok((scan_until(&roots, cache.as_deref(), &options, cancel), roots))
}

/// Searches `roots` and reads each plugin file new or changed since the
/// cache at `cache` (if any) was written, then writes the cache again.
pub fn scan(roots: &[PathBuf], cache: Option<&Path>, options: &Options) -> Catalog {
    scan_until(roots, cache, options, &AtomicBool::new(false))
}

/// [`scan`], ended soon once `cancel` is set, as when its budget runs out.
pub fn scan_until(roots: &[PathBuf], cache: Option<&Path>, options: &Options, cancel: &AtomicBool) -> Catalog {
    let start = Instant::now();
    let scan_end = start.checked_add(options.budget).unwrap_or(start + Duration::from_secs(365 * 86_400));
    let found = find(roots);
    let mut catalog = Catalog { notes: found.notes, ..Catalog::default() };
    let old: HashMap<PathBuf, Entry> =
        cache.map(read_cache).unwrap_or_default().into_iter().map(|e| (e.path.clone(), e)).collect();
    let mut dir: Option<PathBuf> = None;
    for path in &found.files {
        let Some(fingerprint) = fingerprint(path) else {
            catalog.notes.push(format!("{} could not be read", path.display()));
            continue;
        };
        if let Some(e) = old.get(path).filter(|e| !options.rescan && e.fingerprint == fingerprint) {
            catalog.entries.push(e.clone());
            continue;
        }
        if Instant::now() >= scan_end || cancel.load(Ordering::Relaxed) {
            catalog.left += 1;
            continue;
        }
        if dir.is_none() {
            match private_dir() {
                Ok(d) => dir = Some(d),
                Err(e) => {
                    note(&mut catalog, format!("no folder for the scan's results: {e}"));
                    catalog.left += 1;
                    continue;
                }
            }
        }
        let dir = dir.as_deref().unwrap_or(Path::new("."));
        let result = match scan_file(&options.worker, path, dir, options.per_file, scan_end, cancel) {
            Outcome::Read(result) => result,
            Outcome::TimedOut { by_budget: false } => {
                Err(format!("did not finish loading within {} s", options.per_file.as_secs()))
            }
            Outcome::TimedOut { by_budget: true } => {
                catalog.left += 1;
                continue;
            }
            Outcome::Retry(why) => {
                note(&mut catalog, why);
                catalog.left += 1;
                continue;
            }
        };
        // A file that changed while it was read is read again next time.
        if self::fingerprint(path).as_ref() != Some(&fingerprint) {
            catalog.left += 1;
            continue;
        }
        catalog.entries.push(Entry { path: path.clone(), fingerprint, result });
    }
    if let Some(dir) = dir {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            note(&mut catalog, format!("the scan's folder {} could not be removed: {e}", dir.display()));
        }
    }
    // What the cache knew under a folder not searched to the end stays.
    let mut kept: Vec<Entry> = old
        .into_values()
        .filter(|e| !found.files.contains(&e.path) && found.incomplete.iter().any(|r| e.path.starts_with(r)))
        .collect();
    kept.sort_by(|a, b| a.path.cmp(&b.path));
    catalog.entries.extend(kept);
    if let Some(cache) = cache {
        if let Err(e) = write_cache(cache, &catalog.entries) {
            catalog.notes.push(format!("the plugin cache {} was not written: {e}", cache.display()));
        }
    }
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tinvo-clap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn info(id: &str) -> PluginInfo {
        PluginInfo {
            id: id.into(),
            name: "Odd\tname\\ with\nbreaks".into(),
            vendor: String::new(),
            version: "1.0".into(),
            description: "d".into(),
            features: vec!["audio-effect".into(), "a,b".into()],
            roles: Roles::EFFECT,
        }
    }

    #[test]
    fn the_roots_are_clap_s_own_then_clap_path() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(v))
        };
        let sep = if cfg!(windows) { ";" } else { ":" };
        let list: &'static str = Box::leak(format!("/a{sep}{sep}/b").into_boxed_str());
        let pairs: &'static [(&'static str, &'static str)] = Box::leak(Box::new([
            ("HOME", "/home/u"),
            ("COMMONPROGRAMFILES", "C:\\Common"),
            ("LOCALAPPDATA", "C:\\Local"),
            ("CLAP_PATH", list),
        ]));
        let roots = roots_from(env(pairs));
        let tail: Vec<PathBuf> = roots[roots.len() - 2..].to_vec();
        assert_eq!(tail, [PathBuf::from("/a"), PathBuf::from("/b")], "empty entries are skipped");
        if cfg!(windows) {
            assert_eq!(roots[0], Path::new("C:\\Common").join("CLAP"));
            assert_eq!(roots[1], Path::new("C:\\Local").join("Programs").join("Common").join("CLAP"));
        } else if cfg!(target_os = "macos") {
            assert_eq!(roots[0], Path::new("/Library/Audio/Plug-Ins/CLAP"));
            assert_eq!(roots[1], Path::new("/home/u/Library/Audio/Plug-Ins/CLAP"));
        } else {
            assert_eq!(roots[..2], [PathBuf::from("/home/u/.clap"), PathBuf::from("/usr/lib/clap")]);
        }
        // Without HOME or CLAP_PATH, only the system's folders.
        assert!(roots_from(|_| None).len() <= 1);
    }

    #[test]
    fn fields_and_lines_round_trip() {
        for text in ["", "plain", "tab\there", "a\\b", "line\nbreak\r", "\\t literally"] {
            assert_eq!(unescape(&escape(text)).as_deref(), Some(text));
            assert!(!escape(text).contains(['\t', '\n', '\r']));
        }
        assert_eq!(unescape("stray\\"), None);
        assert_eq!(unescape("\\x"), None);
        let p = info("org.example.x");
        let line = plugin_line(&p);
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields[0], "plugin");
        assert_eq!(parse_plugin(&fields[1..]), Some(p));
        assert_eq!(parse_plugin(&["", "name", "", "", ""]), None, "an id is needed");
        assert_eq!(parse_plugin(&["id", "name", ""]), None, "too few fields");
    }

    #[test]
    fn a_result_is_read_only_when_whole() {
        let p = info("org.example.x");
        let ok = format!("{RESULT_HEADER}\n{}\nend\n", plugin_line(&p));
        assert_eq!(parse_result(&ok), Some(Ok(vec![p.clone()])));
        assert_eq!(parse_result(&format!("{RESULT_HEADER}\nerror\twhy\\tnot\nend\n")), Some(Err("why\tnot".into())));
        assert_eq!(parse_result(&format!("{RESULT_HEADER}\nend\n")), Some(Ok(Vec::new())));
        assert_eq!(parse_result(&format!("{RESULT_HEADER}\n{}\n", plugin_line(&p))), None, "cut short");
        assert_eq!(parse_result(&format!("{RESULT_HEADER}\nhello\nend\n")), None);
        assert_eq!(parse_result("tinvo-clap-scan 2\nend\n"), None);
        let line = plugin_line(&p);
        assert_eq!(parse_result(&format!("{RESULT_HEADER}\nend\n{line}\n")), None, "lines after the end");
        assert_eq!(parse_result(&format!("{RESULT_HEADER}\n{line}\nerror\tno\nend\n")), None, "both");
        assert_eq!(parse_result(&format!("{RESULT_HEADER}\nerror\ta\nerror\tb\nend\n")), None, "two errors");
        assert_eq!(parse_result(&format!("{RESULT_HEADER}\nend\nend\n")), None);
        assert_eq!(parse_result(""), None);
    }

    #[test]
    fn the_cache_round_trips_and_refuses_another_system_s() {
        let dir = temp_dir("cache");
        let path = dir.join("sub").join("plugins.txt");
        let entries = vec![
            Entry { path: dir.join("a.clap"), fingerprint: "12:34".into(), result: Ok(vec![info("x"), info("y")]) },
            Entry { path: dir.join("b\tc.clap"), fingerprint: "1:2".into(), result: Err("crashed\nbadly".into()) },
            Entry { path: dir.join("empty.clap"), fingerprint: "0:0".into(), result: Ok(Vec::new()) },
        ];
        write_cache(&path, &entries).unwrap();
        assert_eq!(read_cache(&path), entries);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen(std::env::consts::ARCH, "other", 1)).unwrap();
        assert!(read_cache(&path).is_empty(), "files read for another processor are read again");
        std::fs::write(&path, format!("{}\nplugin\tx\ty\t\t\t\n", cache_header())).unwrap();
        assert!(read_cache(&path).is_empty(), "a plugin needs its file");
        assert!(read_cache(&dir.join("missing.txt")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_search_finds_plugin_files_once_and_in_order() {
        let dir = temp_dir("find");
        let touch = |p: &Path| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        };
        let bundle = |p: &Path| std::fs::create_dir_all(p.join("Contents").join("MacOS")).unwrap();
        if cfg!(target_os = "macos") {
            bundle(&dir.join("b.clap"));
            bundle(&dir.join("sub").join("a.clap"));
            touch(&dir.join("file.clap"));
        } else {
            touch(&dir.join("b.clap"));
            touch(&dir.join("sub").join("a.clap"));
            std::fs::create_dir_all(dir.join("folder.clap")).unwrap();
            touch(&dir.join("folder.clap").join("inner.clap"));
            touch(&dir.join("UPPER.CLAP"));
        }
        touch(&dir.join("notes.txt"));
        touch(&dir.join("readme.clap.txt"));
        let found = find(&[dir.clone(), dir.join("sub"), dir.join("missing")]);
        let names: Vec<String> =
            found.files.iter().map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect();
        if cfg!(target_os = "macos") {
            assert_eq!(names, ["b.clap", "sub/a.clap"], "bundles are not searched inside");
        } else {
            assert_eq!(names, ["UPPER.CLAP", "b.clap", "folder.clap/inner.clap", "sub/a.clap"]);
        }
        assert!(found.incomplete.is_empty(), "{:?}", found.notes);
        // Too deep a tree is searched to its limit, and the folder past it marked.
        let mut deep = dir.join("deep");
        for i in 0..=MAX_DEPTH {
            deep = deep.join(format!("{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        let found = find(&[dir.join("deep")]);
        assert_eq!(found.incomplete, [deep]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_keeps_only_what_the_search_could_not_reach() {
        let dir = temp_dir("prune");
        let root = dir.join("root");
        let mut deep = root.join("deep");
        for i in 0..=MAX_DEPTH {
            deep = deep.join(format!("{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join("near")).unwrap();
        let cache = dir.join("plugins.txt");
        let entry = |path: PathBuf| Entry { path, fingerprint: "1:1".into(), result: Ok(vec![info("x")]) };
        let beyond = entry(deep.join("far.clap"));
        let gone = entry(root.join("near").join("gone.clap"));
        let elsewhere = entry(dir.join("other").join("old.clap"));
        write_cache(&cache, &[gone, beyond.clone(), elsewhere]).unwrap();
        let options = Options { worker: dir.join("none"), per_file: PER_FILE, budget: Duration::ZERO, rescan: false };
        let catalog = scan(&[root.clone(), root], Some(&cache), &options);
        assert_eq!(catalog.entries, std::slice::from_ref(&beyond), "a file too deep to search stays; the others go");
        assert_eq!(read_cache(&cache), [beyond]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn links_to_folders_are_not_followed() {
        let dir = temp_dir("links");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("real").join("loop")).unwrap();
        let found = find(std::slice::from_ref(&dir));
        assert!(found.files.is_empty() && found.incomplete.is_empty(), "{found:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_catalog_lists_each_id_once() {
        let a = Entry { path: "a.clap".into(), fingerprint: String::new(), result: Ok(vec![info("x"), info("y")]) };
        let b = Entry { path: "b.clap".into(), fingerprint: String::new(), result: Ok(vec![info("y")]) };
        let c = Entry { path: "c.clap".into(), fingerprint: String::new(), result: Err("no".into()) };
        let catalog = Catalog { entries: vec![a, b, c], ..Catalog::default() };
        let ids: Vec<(&str, &str)> =
            catalog.plugins().iter().map(|(p, i)| (p.to_str().unwrap(), i.id.as_str())).collect();
        assert_eq!(ids, [("a.clap", "x"), ("a.clap", "y")]);
        assert_eq!(catalog.duplicates(), [("y", Path::new("a.clap"), Path::new("b.clap"))]);
        assert_eq!(catalog.failures(), [(Path::new("c.clap"), "no")]);
        let found = |id| catalog.find(id).map(|(p, i)| (p.to_str().unwrap(), i.id.as_str()));
        assert_eq!((found("y"), found("x"), found("z")), (Some(("a.clap", "y")), Some(("a.clap", "x")), None));
    }
}
