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
}
