//! Keeps two TINVO sessions off one project.
//!
//! A session holds `song.tvo.lock` next to the project for as long as it
//! works on it: an exclusive `flock` on Linux and macOS, a handle that
//! shares no writing on Windows. The operating system lets go of both when
//! the process ends, however it ends, so a crash never leaves a project
//! locked. The file names the process holding it, for the message another
//! session shows.
//!
//! On Windows the file is opened to be deleted when its handle closes. On
//! Linux and macOS the holder deletes it before letting go, and a session
//! that wins the lock checks that the file it locked is still the one at
//! the path, so a lock on a file just deleted counts for nothing.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const SUFFIX: &str = ".lock";

/// The lock file of the project at `project`.
pub fn path_for(project: &Path) -> Option<PathBuf> {
    let mut name = project.file_name()?.to_owned();
    name.push(SUFFIX);
    Some(project.with_file_name(name))
}

/// A held lock; dropping it lets go.
#[derive(Debug)]
pub struct Lock {
    /// The project it is for.
    project: PathBuf,
    path: PathBuf,
    /// Held open for as long as the lock is.
    file: Option<File>,
}

/// Why a lock was not taken.
#[derive(Debug)]
pub enum Error {
    /// Another session holds it; the process id it wrote, if readable.
    Held(Option<u32>),
    /// The lock file could not be made or opened.
    Io(io::Error),
}

impl Error {
    /// The error for a person, about the project at `project`.
    pub fn describe(&self, project: &Path) -> String {
        match self {
            Error::Held(Some(pid)) => {
                format!("{} is open in another TINVO session (process {pid}); close it there first", project.display())
            }
            Error::Held(None) => {
                format!("{} is open in another TINVO session; close it there first", project.display())
            }
            Error::Io(e) => format!("cannot lock {}: {e}", project.display()),
        }
    }
}

impl Lock {
    /// Takes the lock of the project at `project`. A lock this process
    /// already holds counts as held: one session per project.
    pub fn acquire(project: &Path) -> Result<Lock, Error> {
        let path = path_for(project)
            .ok_or_else(|| Error::Io(io::Error::new(io::ErrorKind::InvalidInput, "the path names no file")))?;
        let mut file = sys::open_locked(&path)?;
        // Who holds it, for the message another session shows. The lock is
        // held whether or not this is written.
        let _ = file
            .set_len(0)
            .and_then(|()| writeln!(file, "tinvo {} pid {}", env!("CARGO_PKG_VERSION"), std::process::id()));
        Ok(Lock { project: project.to_path_buf(), path, file: Some(file) })
    }

    /// The project this lock is for.
    pub fn project(&self) -> &Path {
        &self.project
    }

    /// Whether this is the lock of the project at `project`, also when its
    /// path is spelled another way.
    pub fn is_for(&self, project: &Path) -> bool {
        if project == self.project {
            return true;
        }
        let canonical = |p: &Path| std::fs::canonicalize(p).ok();
        path_for(project).and_then(|p| canonical(&p)).is_some_and(|p| Some(p) == canonical(&self.path))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Deleted while still held, so no other session can lock the file in
        // between (on Windows it goes when the handle closes).
        if cfg!(unix) {
            let _ = std::fs::remove_file(&self.path);
        }
        drop(self.file.take());
    }
}

/// The process id a lock file names.
fn owner(path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(path).ok()?.take(256).read_to_string(&mut text).ok()?;
    let mut words = text.split_whitespace();
    words.position(|w| w == "pid")?;
    words.next()?.parse().ok()
}

#[cfg(unix)]
mod sys {
    use super::{Error, owner};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::raw::c_int;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    const LOCK_EX: c_int = 2;
    const LOCK_NB: c_int = 4;

    unsafe extern "C" {
        fn flock(fd: c_int, operation: c_int) -> c_int;
    }

    pub fn open_locked(path: &Path) -> Result<File, Error> {
        // A holder deletes the file just before letting go: a session that
        // locked the deleted file tries again on the new one.
        for _ in 0..64 {
            let file =
                OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(Error::Io)?;
            // SAFETY: the descriptor is open for the call's duration.
            if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
                let e = io::Error::last_os_error();
                return Err(match e.kind() {
                    io::ErrorKind::WouldBlock => Error::Held(owner(path)),
                    _ => Error::Io(e),
                });
            }
            let held = file.metadata().map_err(Error::Io)?;
            match std::fs::metadata(path) {
                Ok(m) if m.dev() == held.dev() && m.ino() == held.ino() => return Ok(file),
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Err(Error::Io(io::Error::other("the lock file keeps being replaced")))
    }
}

#[cfg(windows)]
mod sys {
    use super::{Error, owner};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use std::time::Duration;

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;

    pub fn open_locked(path: &Path) -> Result<File, Error> {
        let mut last = None;
        for _ in 0..20 {
            // Sharing no writing is the lock: another session asking to
            // write is refused while this handle is open. Reading is shared
            // so it can say who holds the lock, and deleting because
            // delete-on-close requires every handle to share it.
            let opened = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
                .open(path);
            match opened {
                Ok(f) => return Ok(f),
                Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => return Err(Error::Held(owner(path))),
                // The holder is letting go: the file is waiting to be
                // deleted, and cannot be opened until it is.
                Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Err(Error::Io(last.unwrap_or_else(|| io::Error::other("the lock file cannot be opened"))))
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use super::Error;
    use std::fs::{File, OpenOptions};
    use std::path::Path;

    /// No locking here: the file only names the session.
    pub fn open_locked(path: &Path) -> Result<File, Error> {
        OpenOptions::new().write(true).create(true).truncate(true).open(path).map_err(Error::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tinvo-lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lock_files_sit_next_to_the_project() {
        let p = Path::new("music").join("song.tvo");
        assert_eq!(path_for(&p), Some(Path::new("music").join("song.tvo.lock")));
        assert_eq!(path_for(Path::new("")), None);
    }

    #[test]
    fn one_session_holds_a_project_until_it_lets_go() {
        let dir = temp_dir("hold");
        let song = dir.join("song.tvo");
        let lock = Lock::acquire(&song).unwrap();
        assert_eq!(lock.project(), song);
        let text = std::fs::read_to_string(dir.join("song.tvo.lock")).unwrap();
        assert!(text.starts_with("tinvo "), "{text}");
        // The same project, also by another spelling of its path.
        for again in [song.clone(), dir.join(".").join("song.tvo")] {
            match Lock::acquire(&again) {
                Err(Error::Held(pid)) => assert_eq!(pid, Some(std::process::id())),
                other => panic!("{other:?}"),
            }
        }
        // Another project is free.
        let other = Lock::acquire(&dir.join("other.tvo")).unwrap();
        drop(other);
        drop(lock);
        assert!(!dir.join("song.tvo.lock").exists(), "letting go deletes the file");
        let again = Lock::acquire(&song).unwrap();
        drop(again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lock_file_left_by_a_crash_does_not_hold() {
        let dir = temp_dir("stale");
        let song = dir.join("song.tvo");
        std::fs::write(dir.join("song.tvo.lock"), "tinvo 0.1.0 pid 1\n").unwrap();
        let lock = Lock::acquire(&song).unwrap();
        let text = std::fs::read_to_string(dir.join("song.tvo.lock")).unwrap();
        assert!(text.contains(&format!("pid {}", std::process::id())), "{text}");
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lock_held_in_another_process_names_it() {
        // This test binary, run again, takes the lock and waits.
        let Ok(dir) = std::env::var("TINVO_LOCK_CHILD") else {
            let dir = temp_dir("child");
            let ready = dir.join("ready");
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "lock::tests::a_lock_held_in_another_process_names_it", "--nocapture"])
                .env("TINVO_LOCK_CHILD", &dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let start = std::time::Instant::now();
            while !ready.exists() {
                assert!(start.elapsed().as_secs() < 30, "the child never took the lock");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            match Lock::acquire(&dir.join("song.tvo")) {
                Err(Error::Held(pid)) => assert_eq!(pid, Some(child.id())),
                other => panic!("{other:?}"),
            }
            let msg = Error::Held(Some(child.id())).describe(Path::new("song.tvo"));
            assert!(msg.starts_with("song.tvo is open in another TINVO session (process "), "{msg}");
            // Killed, it never lets go itself; the system does.
            child.kill().unwrap();
            child.wait().unwrap();
            let lock = Lock::acquire(&dir.join("song.tvo")).expect("free once the holder is gone");
            drop(lock);
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let dir = PathBuf::from(dir);
        let _lock = Lock::acquire(&dir.join("song.tvo")).unwrap();
        std::fs::write(dir.join("ready"), "").unwrap();
        // The parent kills this process; its stdin stays open until then.
        let _ = io::stdin().read(&mut [0u8; 1]);
    }
}
