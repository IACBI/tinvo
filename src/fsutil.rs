//! Atomic file replacement, and reading text files of bounded size.
//!
//! A file is written to a fresh temporary file next to it and renamed over
//! it at the end, so a reader never sees a half-written file and a failed
//! write leaves the old one untouched. Each temporary file has a unique name
//! and is created with `create_new`, which refuses to open anything already
//! there, including a symbolic link planted at that name.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// `path` as an error names it: on one line, its control characters escaped,
/// as a name may hold a line break or an escape sequence.
pub fn shown(path: &Path) -> String {
    crate::tui::printable_line(&path.display().to_string())
}

/// Creates a new, empty temporary file in the directory of `path`. Its name
/// does not grow with `path`'s, so a file whose name is as long as a name
/// may be can still be written.
pub fn create_temp_beside(path: &Path) -> io::Result<(File, PathBuf)> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    for _ in 0..64 {
        let name = format!(".tinvo-{}-{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
        let tmp = path.with_file_name(name);
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((f, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free temporary file name"))
}

/// Renames `from` to `to` unless `to` exists (an `AlreadyExists` error
/// then); a file another program creates at `to` meanwhile is never
/// replaced. Where the file system has hard links this is atomic, since a
/// link to a name already taken fails; elsewhere `from` is copied to `to`.
pub fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            // The file is at `to` now; a leftover second name is harmless.
            let _ = std::fs::remove_file(from);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => copy_new(from, to),
    }
}

/// Moves `from` to `to` by copying it into a file created with
/// `create_new`, which takes the name only if it is free. Unlike a rename,
/// a reader can see `to` half written, and a failed copy removes it.
fn copy_new(from: &Path, to: &Path) -> io::Result<()> {
    let mut source = File::open(from)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(to)?;
    let copied = io::copy(&mut source, &mut file).and_then(|_| file.sync_all());
    drop(file);
    drop(source);
    if let Err(e) = copied {
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
    let _ = std::fs::remove_file(from);
    Ok(())
}

/// Replaces `path` with `bytes` atomically.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (mut file, tmp) = create_temp_beside(path)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The `O_NONBLOCK` flag of each platform's <fcntl.h>, where it is known;
/// elsewhere a named pipe opens waiting for a writer.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
const NONBLOCK: Option<i32> = Some(0x0004);
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "s390x",
        target_arch = "loongarch64"
    )
))]
const NONBLOCK: Option<i32> = Some(0o4000);
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "riscv64",
            target_arch = "powerpc",
            target_arch = "powerpc64",
            target_arch = "s390x",
            target_arch = "loongarch64"
        )
    )
)))]
#[cfg_attr(not(unix), allow(dead_code))]
const NONBLOCK: Option<i32> = None;

/// Whether [`open_without_waiting`] opens a named pipe without waiting on
/// this platform.
#[cfg(test)]
pub const OPENS_WITHOUT_WAITING: bool = NONBLOCK.is_some();

/// Opens `path` for reading without waiting for a writer, were it a named
/// pipe (`O_NONBLOCK`, which reading a plain file ignores). Callers check
/// what was opened, since the name may have changed since they last looked.
pub fn open_without_waiting(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    if let Some(flag) = NONBLOCK {
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, flag);
    }
    options.open(path)
}

/// Reads at most `max` bytes from the plain file `path`. A longer file is
/// an error once `max + 1` bytes are read, so a huge file cannot take all
/// memory; anything else (a folder, a FIFO, a device) is refused, opened
/// without waiting for it where the platform allows.
pub fn read_bytes(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    // Checked by name before opening too: where a pipe cannot be opened
    // without waiting, and for a device, opening it could wait.
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
    }
    let mut file = open_without_waiting(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
    }
    let too_large = || {
        let size = if max >= 1 << 20 { format!("{} MiB", max >> 20) } else { format!("{max} bytes") };
        io::Error::new(io::ErrorKind::InvalidData, format!("larger than {size}"))
    };
    // Known too large, it is not read; the reading stays bounded too, as
    // the file may grow meanwhile.
    if meta.len() > max {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(too_large());
    }
    Ok(bytes)
}

/// Reads the plain file `path` as UTF-8 text of at most `max` bytes, as
/// [`read_bytes`] does.
pub fn read_text(path: &Path, max: u64) -> io::Result<String> {
    String::from_utf8(read_bytes(path, max)?).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8 text"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_named_on_one_line() {
        assert_eq!(shown(Path::new("a\nb\u{1b}[2J.wav")), "a\\nb\\u{1b}[2J.wav");
    }

    #[test]
    fn a_file_with_a_name_as_long_as_names_go_is_written() {
        let dir = std::env::temp_dir().join(format!("tinvo-fsutil-long-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.tvo", "a".repeat(250)));
        write_atomic(&path, b"tinvo 20\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"tinvo 20\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn text_is_read_up_to_its_limit() {
        let dir = std::env::temp_dir().join(format!("tinvo-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.txt");
        std::fs::write(&path, "çok").unwrap();
        assert_eq!(read_text(&path, 4).unwrap(), "çok");
        let e = read_text(&path, 3).unwrap_err();
        assert_eq!((e.kind(), e.to_string()), (io::ErrorKind::InvalidData, "larger than 3 bytes".into()));
        std::fs::write(&path, [0xff, b'a']).unwrap();
        assert_eq!(read_text(&path, 4).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(read_text(&dir.join("none"), 4).unwrap_err().kind(), io::ErrorKind::NotFound);
        // Only a plain file is read; a pipe opens without waiting for a
        // writer, where the platform allows. (Windows refuses to open a
        // folder at all.)
        assert!(read_text(&dir, 4).is_err());
        #[cfg(unix)]
        assert_eq!(read_text(&dir, 4).unwrap_err().to_string(), "not a file");
        #[cfg(windows)]
        assert!(read_text(Path::new("NUL"), 4).is_err(), "a device");
        #[cfg(unix)]
        if OPENS_WITHOUT_WAITING {
            let fifo = dir.join("pipe");
            assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
            assert!(!open_without_waiting(&fifo).unwrap().metadata().unwrap().is_file());
            assert_eq!(read_text(&fifo, 4).unwrap_err().to_string(), "not a file");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replaces_atomically_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("tinvo-fsutil-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        // Two temporary files for one target never share a name.
        let (_, a) = create_temp_beside(&path).unwrap();
        let (_, b) = create_temp_beside(&path).unwrap();
        assert_ne!(a, b);
        std::fs::remove_file(a).unwrap();
        std::fs::remove_file(b).unwrap();
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("a.txt")]);
        // A failed rename (the target is a directory) cleans up.
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(write_atomic(&sub, b"x").is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_name_is_taken_only_while_it_is_free() {
        let dir = std::env::temp_dir().join(format!("tinvo-rename-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (from, to) = (dir.join("from"), dir.join("to"));
        for publish in [rename_new, copy_new] {
            std::fs::write(&from, "new").unwrap();
            std::fs::write(&to, "theirs").unwrap();
            assert_eq!(publish(&from, &to).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read_to_string(&to).unwrap(), "theirs");
            std::fs::remove_file(&to).unwrap();
            publish(&from, &to).unwrap();
            assert_eq!(std::fs::read_to_string(&to).unwrap(), "new");
            assert!(!from.exists());
            std::fs::remove_file(&to).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
