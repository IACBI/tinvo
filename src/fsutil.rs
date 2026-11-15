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

/// Creates a new, empty temporary file in the directory of `path`.
pub fn create_temp_beside(path: &Path) -> io::Result<(File, PathBuf)> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    for _ in 0..64 {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{}-{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let tmp = PathBuf::from(name);
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((f, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free temporary file name"))
}

/// Renames `from` to `to` unless `to` exists (an `AlreadyExists` error
/// then). Where the file system has hard links this is atomic, since a link
/// to a name already taken fails; elsewhere it looks first, which leaves a
/// moment for another program to take the name.
pub fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            // The file is at `to` now; a leftover second name is harmless.
            let _ = std::fs::remove_file(from);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) if std::fs::symlink_metadata(to).is_ok() => Err(io::ErrorKind::AlreadyExists.into()),
        Err(_) => std::fs::rename(from, to),
    }
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

/// Reads `path` as UTF-8 text of at most `max` bytes. A longer file is an
/// error once `max + 1` bytes are read, so a huge file, or one that never
/// ends (a FIFO, a device), cannot take all memory.
pub fn read_text(path: &Path, max: u64) -> io::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        let size = if max >= 1 << 20 { format!("{} MiB", max >> 20) } else { format!("{max} bytes") };
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("larger than {size}")));
    }
    String::from_utf8(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8 text"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
