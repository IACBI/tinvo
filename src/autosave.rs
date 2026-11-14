//! Crash-safe autosave and recovery.
//!
//! While a project has unsaved edits, the editor writes it every
//! [`INTERVAL`] to an autosave file: `song.tvo.autosave` next to
//! `song.tvo`, or `untitled.tvo.autosave` in TINVO's state directory for a
//! project that has never been saved (`untitled-2.tvo.autosave` and so on
//! while other untitled sessions run). The write is atomic (a temporary file,
//! synced, then renamed), so the autosave is always a whole project. Saving
//! the project or quitting normally deletes it, so an autosave found at
//! start-up means the last session ended without saving: crashed, killed, or
//! lost its terminal. The editor then offers to recover it.

use crate::project::Project;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// How often unsaved edits are written; at most this much work is lost.
pub const INTERVAL: Duration = Duration::from_secs(5);

const SUFFIX: &str = ".autosave";
const UNTITLED: &str = "untitled.tvo";

/// TINVO's per-user state directory: `%LOCALAPPDATA%\tinvo` on Windows,
/// `~/Library/Application Support/tinvo` on macOS, and
/// `$XDG_STATE_HOME/tinvo` (by default `~/.local/state/tinvo`) elsewhere.
pub fn state_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")?
    } else if cfg!(target_os = "macos") {
        var("HOME")?.join("Library").join("Application Support")
    } else {
        // The XDG base directory specification ignores relative paths.
        var("XDG_STATE_HOME").filter(|p| p.is_absolute()).or_else(|| Some(var("HOME")?.join(".local").join("state")))?
    };
    Some(base.join("tinvo"))
}

/// How many untitled sessions can run at once, each autosaving apart.
pub const UNTITLED_SLOTS: u32 = 16;

/// Where untitled session `slot` (from 1) keeps its autosave and lock, as
/// if its project were saved there: `untitled.tvo` in the state directory
/// for the first, then `untitled-2.tvo` and so on.
pub fn untitled(slot: u32) -> Option<PathBuf> {
    Some(untitled_in(&state_dir()?, slot))
}

/// [`untitled`] in the directory `dir`.
pub fn untitled_in(dir: &Path, slot: u32) -> PathBuf {
    dir.join(if slot <= 1 { UNTITLED.to_string() } else { format!("untitled-{slot}.tvo") })
}

/// The autosave file for a project at `project` (`None`: never saved, the
/// first untitled session's).
pub fn path_for(project: Option<&Path>) -> Option<PathBuf> {
    match project {
        Some(p) => {
            let mut name = p.file_name()?.to_owned();
            name.push(SUFFIX);
            Some(p.with_file_name(name))
        }
        None => path_for(Some(&untitled(1)?)),
    }
}

/// An autosave left behind by a session that did not end normally.
pub struct Found {
    pub path: PathBuf,
    /// The project it holds, or why it cannot be read.
    pub project: Result<Project, String>,
    /// When it was written, if the file system says.
    pub modified: Option<SystemTime>,
}

/// Looks for the autosave of the project at `project`.
pub fn find(project: Option<&Path>) -> Option<Found> {
    let path = path_for(project)?;
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(Found { project: Project::load(&path), modified: meta.modified().ok(), path })
}

/// Moves an autosave that cannot be read out of the way, so the next
/// autosave does not overwrite it; returns where it went.
pub fn set_aside(path: &Path) -> io::Result<PathBuf> {
    let mut name = path.as_os_str().to_owned();
    name.push(".unreadable");
    let to = PathBuf::from(name);
    std::fs::rename(path, &to)?;
    Ok(to)
}

/// Writes the autosave of one editing session.
#[derive(Debug, Default)]
pub struct Autosave {
    /// Where it goes; `None` disables it (no state directory, or in tests).
    path: Option<PathBuf>,
    /// The edit revision last written.
    written: u64,
    last: Option<Instant>,
}

impl Autosave {
    /// Autosaves for the project at `project`; `revision` is the revision
    /// already on disk (in the project or a recovered autosave).
    pub fn new(project: Option<&Path>, revision: u64) -> Self {
        Self { path: path_for(project), written: revision, last: None }
    }

    /// An autosave writing to exactly `path`.
    pub fn at(path: PathBuf) -> Self {
        Self { path: Some(path), written: 0, last: None }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether `revision` has edits the autosave does not hold yet, and the
    /// last write is at least [`INTERVAL`] old.
    pub fn due(&self, revision: u64, now: Instant) -> bool {
        self.path.is_some()
            && revision != self.written
            && self.last.is_none_or(|t| now.saturating_duration_since(t) >= INTERVAL)
    }

    /// Writes `project` at `revision`. A failure is retried at the next
    /// interval.
    pub fn write(&mut self, project: &Project, revision: u64, now: Instant) -> io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        self.last = Some(now);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        project.save(path)?;
        self.written = revision;
        Ok(())
    }

    /// The project was saved at `revision` to `project`: the autosave is
    /// stale, so it is deleted, and later ones go next to the saved file.
    pub fn saved(&mut self, project: &Path, revision: u64) {
        self.discard();
        self.path = path_for(Some(project));
        self.written = revision;
        // The saved file may have an autosave of its own from an earlier
        // session; it is older than what was just saved.
        self.discard();
    }

    /// Deletes the autosave file, for a normal end of the session.
    pub fn discard(&mut self) {
        if let Some(p) = &self.path {
            let _ = std::fs::remove_file(p);
        }
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tinvo-autosave-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn autosaves_sit_next_to_the_project_or_in_the_state_directory() {
        let p = Path::new("music").join("song.tvo");
        assert_eq!(path_for(Some(&p)), Some(Path::new("music").join("song.tvo.autosave")));
        assert_eq!(path_for(Some(Path::new("song.tvo"))), Some(PathBuf::from("song.tvo.autosave")));
        if let Some(dir) = state_dir() {
            assert!(dir.is_absolute() && dir.ends_with("tinvo"), "{dir:?}");
            assert_eq!(path_for(None), Some(dir.join("untitled.tvo.autosave")));
            assert_eq!(untitled(1), Some(dir.join("untitled.tvo")));
            assert_eq!(path_for(untitled(3).as_deref()), Some(dir.join("untitled-3.tvo.autosave")));
        }
    }

    #[test]
    fn writes_only_new_revisions_and_at_most_once_per_interval() {
        let dir = temp_dir("due");
        let mut a = Autosave::at(dir.join("nested").join("song.tvo.autosave"));
        let t0 = Instant::now();
        assert!(!a.due(0, t0), "nothing new");
        assert!(a.due(1, t0));
        a.write(&Project::demo(), 1, t0).unwrap();
        assert!(a.path().unwrap().is_file(), "the directory is created");
        assert!(!a.due(1, t0 + INTERVAL), "already written");
        assert!(!a.due(2, t0 + INTERVAL / 2), "too soon");
        assert!(a.due(2, t0 + INTERVAL));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_autosave_holds_the_whole_project_and_is_found_again() {
        let dir = temp_dir("find");
        let song = dir.join("song.tvo");
        let mut project = Project::demo();
        project.tempo = 97.0;
        let mut a = Autosave::new(Some(&song), 0);
        a.write(&project, 3, Instant::now()).unwrap();
        let found = find(Some(&song)).expect("autosave found");
        assert_eq!(found.path, dir.join("song.tvo.autosave"));
        assert_eq!(found.project.unwrap().to_text(), project.to_text());
        assert!(found.modified.is_some());
        // Saving the project removes it; so does discarding.
        a.saved(&song, 3);
        assert!(find(Some(&song)).is_none());
        a.write(&project, 4, Instant::now()).unwrap();
        a.discard();
        assert!(find(Some(&song)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_under_a_new_name_moves_the_autosave() {
        let dir = temp_dir("move");
        let (old, new) = (dir.join("a.tvo"), dir.join("b.tvo"));
        // A stale autosave of the target from an earlier session.
        std::fs::write(dir.join("b.tvo.autosave"), "stale").unwrap();
        let mut a = Autosave::new(Some(&old), 0);
        a.write(&Project::demo(), 1, Instant::now()).unwrap();
        a.saved(&new, 1);
        assert!(!dir.join("a.tvo.autosave").exists() && !dir.join("b.tvo.autosave").exists());
        assert_eq!(a.path(), Some(dir.join("b.tvo.autosave").as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_autosave_is_reported_and_set_aside() {
        let dir = temp_dir("bad");
        let song = dir.join("song.tvo");
        std::fs::write(dir.join("song.tvo.autosave"), "not a project").unwrap();
        let found = find(Some(&song)).unwrap();
        assert!(found.project.is_err());
        let moved = set_aside(&found.path).unwrap();
        assert_eq!(moved, dir.join("song.tvo.autosave.unreadable"));
        assert!(find(Some(&song)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
