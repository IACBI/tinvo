//! CLAP plugins: the plain C plugin interface TINVO hosts, reached through
//! hand-written FFI like the audio backends.
//!
//! [`scan`] finds the plugin files in the folders CLAP names and in
//! `CLAP_PATH`, and reads what each offers in a child process of its own
//! (`tinvo scan-clap`), so a plugin that crashes or hangs while it starts
//! costs only that child. The results are kept in a cache in TINVO's state
//! directory until a file changes. [`library`] loads a plugin file and
//! pairs its `init` with one `deinit`. [`ffi`] holds the C types.

pub mod ffi;
pub mod host;
pub mod library;
pub mod scan;
pub mod slot;

/// What a plugin is for, from its descriptor's features: it can be more
/// than one, or none TINVO knows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Roles(u8);

impl Roles {
    pub const INSTRUMENT: Roles = Roles(1);
    pub const EFFECT: Roles = Roles(2);
    pub const NOTE_EFFECT: Roles = Roles(4);
    pub const ANALYZER: Roles = Roles(8);

    /// The roles `features` name (CLAP's `instrument`, `audio-effect`,
    /// `note-effect` and `analyzer`).
    pub fn from_features(features: &[String]) -> Roles {
        let mut r = Roles::default();
        for f in features {
            r.0 |= match f.as_str() {
                "instrument" => Roles::INSTRUMENT.0,
                "audio-effect" => Roles::EFFECT.0,
                "note-effect" => Roles::NOTE_EFFECT.0,
                "analyzer" => Roles::ANALYZER.0,
                _ => 0,
            };
        }
        r
    }

    pub fn contains(self, other: Roles) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }

    /// The roles in words: "instrument", "effect, analyzer", or "other".
    pub fn label(self) -> String {
        let names = [
            (Roles::INSTRUMENT, "instrument"),
            (Roles::EFFECT, "effect"),
            (Roles::NOTE_EFFECT, "note effect"),
            (Roles::ANALYZER, "analyzer"),
        ];
        let words: Vec<&str> = names.iter().filter(|(r, _)| self.contains(*r)).map(|&(_, n)| n).collect();
        if words.is_empty() { "other".into() } else { words.join(", ") }
    }
}

/// One plugin a plugin file offers, as its descriptor gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginInfo {
    /// Its unique id, such as `com.u-he.diva`.
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub description: String,
    pub features: Vec<String>,
    pub roles: Roles,
}

/// The longest plugin id kept, in bytes.
pub const MAX_ID: usize = 1023;

/// Whether a plugin can have `id` as TINVO keeps ids: not blank, at most
/// [`MAX_ID`] bytes, no control characters (it goes on a line of its own
/// in project and cache files). The scanner, the cache and
/// [`PluginRef::new`] all take ids by this.
pub fn is_valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= MAX_ID && !id.chars().any(char::is_control)
}
/// The most plugin ids one run of TINVO keeps ([`PluginRef::new`]).
pub const MAX_REFS: usize = 4096;

/// Every plugin id named in this run, in the order first named; each is
/// leaked once, so a [`PluginRef`] can lend it out for good.
static IDS: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

/// A plugin, by its id, as a number: so the kinds of instrument and effect
/// that name one stay `Copy`. The same id always gives the same number in
/// one run; numbers mean nothing across runs (projects keep the id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginRef(u32);

impl PluginRef {
    /// The number for `id`: `None` for an id no plugin can have
    /// ([`is_valid_id`]), or once [`MAX_REFS`] different ids have been
    /// named. Locks: off the audio thread.
    pub fn new(id: &str) -> Option<PluginRef> {
        if !is_valid_id(id) {
            return None;
        }
        let mut ids = IDS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = ids.iter().position(|&s| s == id) {
            return Some(PluginRef(i as u32));
        }
        if ids.len() >= MAX_REFS {
            return None;
        }
        ids.push(Box::leak(id.into()));
        Some(PluginRef(ids.len() as u32 - 1))
    }

    /// Its plugin's id. Locks: off the audio thread.
    pub fn id(self) -> &'static str {
        let ids = IDS.lock().unwrap_or_else(|e| e.into_inner());
        // Only `new` makes one, from a place it filled.
        ids[self.0 as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_come_from_the_features() {
        let f = |list: &[&str]| Roles::from_features(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(f(&["instrument", "synthesizer", "stereo"]).label(), "instrument");
        assert_eq!(f(&["audio-effect", "analyzer"]).label(), "effect, analyzer");
        assert_eq!(f(&["note-effect"]).label(), "note effect");
        assert_eq!(f(&["utility"]).label(), "other");
        assert!(f(&["instrument", "audio-effect"]).contains(Roles::EFFECT));
        assert!(!Roles::default().contains(Roles::default()));
    }

    #[test]
    fn a_plugin_id_is_one_number_for_good() {
        let a = PluginRef::new("org.tinvo.test.ref-a").unwrap();
        assert_eq!(PluginRef::new("org.tinvo.test.ref-a"), Some(a));
        let b = PluginRef::new("org.tinvo.test.ref-b").unwrap();
        assert_ne!(a, b);
        assert_eq!((a.id(), b.id()), ("org.tinvo.test.ref-a", "org.tinvo.test.ref-b"));
        let long = "x".repeat(MAX_ID);
        assert_eq!(PluginRef::new(&long).map(PluginRef::id), Some(long.as_str()));
        for bad in [String::new(), " \t".into(), "x".repeat(MAX_ID + 1), "a\nb".into(), "a\u{7f}".into()] {
            assert_eq!(PluginRef::new(&bad), None, "{bad:?}");
        }
    }
}
