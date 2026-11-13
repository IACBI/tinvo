//! The settings file: key bindings and colours.
//!
//! ```text
//! # Lines starting with # are comments; a # later in a line is not.
//! [keys]
//! undo = u ctrl+z
//! left = h left
//! pattern_shorter =
//!
//! [theme]
//! base = light
//! cursor = black on bright-cyan bold
//! ```
//!
//! An action listed here keeps only the keys listed (`pattern_shorter`
//! above has none); the others keep their defaults. `base` picks the
//! built-in theme the colours start from. `tinvo settings` prints every
//! action and role with what it has.

use crate::keymap::{self, Action, Context, Keymap};
use crate::theme::{self, Role, Theme};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "tinvo.conf";

/// Everything the settings file sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub keys: Keymap,
    /// The built-in theme the colours start from.
    pub base: &'static str,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { keys: Keymap::default(), base: theme::BUILT_IN[0], theme: Theme::default() }
    }
}

/// Where the settings file is looked for: `%APPDATA%\tinvo` on Windows,
/// `~/Library/Application Support/tinvo` on macOS, and
/// `$XDG_CONFIG_HOME/tinvo` (by default `~/.config/tinvo`) elsewhere.
pub fn default_path() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        var("APPDATA")?
    } else if cfg!(target_os = "macos") {
        var("HOME")?.join("Library").join("Application Support")
    } else {
        // The XDG base directory specification ignores relative paths.
        var("XDG_CONFIG_HOME").filter(|p| p.is_absolute()).or_else(|| Some(var("HOME")?.join(".config")))?
    };
    Some(base.join("tinvo").join(FILE_NAME))
}

/// Reads the settings at `path`; a file that does not exist gives the
/// defaults, and `false` for whether one was read.
pub fn load(path: &Path) -> Result<(Settings, bool), String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map(|s| (s, true)).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Settings::default(), false)),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Keys,
    Theme,
}

pub fn parse(text: &str) -> Result<Settings, String> {
    let mut section = Section::None;
    let mut keys = Keymap::default();
    let mut bound: Vec<Action> = Vec::new();
    let mut base: Option<&'static str> = None;
    let mut styles: Vec<(Role, crate::tui::Style)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let at = |e: String| format!("line {}: {e}", n + 1);
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = match name.trim() {
                "keys" => Section::Keys,
                "theme" => Section::Theme,
                other => return Err(at(format!("unknown section [{other}] (there are [keys] and [theme])"))),
            };
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(at("expected 'name = value'".into()));
        };
        let (name, value) = (name.trim(), value.trim());
        match section {
            Section::None => return Err(at("a setting must come after [keys] or [theme]".into())),
            Section::Keys => {
                let a = Action::from_id(name).ok_or_else(|| at(format!("unknown action '{name}'")))?;
                if bound.contains(&a) {
                    return Err(at(format!("{name} is set twice")));
                }
                bound.push(a);
                let list = value.split_whitespace().map(keymap::parse_key).collect::<Result<Vec<_>, _>>();
                keys.bind(a, list.map_err(at)?);
            }
            Section::Theme if name == "base" => {
                if base.is_some() {
                    return Err(at("base is set twice".into()));
                }
                let found = theme::BUILT_IN.iter().find(|&&b| b == value);
                base = Some(found.ok_or_else(|| {
                    at(format!("unknown theme '{value}' (built in: {})", theme::BUILT_IN.join(", ")))
                })?);
            }
            Section::Theme => {
                let r = Role::from_id(name).ok_or_else(|| at(format!("unknown colour role '{name}'")))?;
                if styles.iter().any(|(o, _)| *o == r) {
                    return Err(at(format!("{name} is set twice")));
                }
                styles.push((r, theme::parse_style(value).map_err(at)?));
            }
        }
    }
    keys.check()?;
    let base = base.unwrap_or(theme::BUILT_IN[0]);
    let mut theme = Theme::built_in(base).unwrap_or_default();
    for (r, s) in styles {
        theme.set(r, s);
    }
    Ok(Settings { keys, base, theme })
}

impl Settings {
    /// The whole settings file for these settings, with every action and
    /// role and what each does; [`parse`] reads it back to the same.
    pub fn to_text(&self) -> String {
        let mut out = String::from(
            "# TINVO settings. Lines starting with # are comments; a # later in a line is not.\n\
             # Keys: one character for itself, or space, tab, shift+tab, enter, esc,\n\
             # backspace, delete, up, down, left, right, home, end, pageup, pagedown,\n\
             # f1-f12, ctrl+a-z (not ctrl+h, i, j or m). An action given no key has none.\n\
             # Keys may not repeat where the actions are active together.\n\
             # A setting left out keeps its default, so a settings file needs only the\n\
             # lines that differ; it also keeps defaults that a later TINVO changes.\n\n[keys]\n",
        );
        for ctx in Context::ALL {
            out += &format!("\n# {}\n", ctx.title());
            for a in Action::ALL.iter().filter(|a| a.context() == ctx) {
                let keys: Vec<String> = self.keys.keys(*a).iter().map(|&k| keymap::key_name(k)).collect();
                out += &format!("# {}\n{} = {}\n", a.help(), a.id(), keys.join(" "));
            }
        }
        out += &format!(
            "\n# Colours: a name (default, black, red, green, yellow, blue, magenta, cyan,\n\
             # white, gray, or bright- and one of the colours from red to white),\n\
             # an xterm palette index 0-255, or #rrggbb (needs a terminal with 24-bit colour).\n\
             # A style is [colour] [on colour] [bold]. base is a built-in theme: {}.\n\n[theme]\nbase = {}\n",
            theme::BUILT_IN.join(", "),
            self.base
        );
        for r in Role::ALL {
            out += &format!("# {}\n{} = {}\n", r.help(), r.id(), theme::style_name(self.theme.get(*r)));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{Color, Key, Style};

    #[test]
    fn an_empty_file_gives_the_defaults() {
        assert_eq!(parse(""), Ok(Settings::default()));
        assert_eq!(parse("# only a comment\n\n[keys]\n[theme]\n"), Ok(Settings::default()));
    }

    #[test]
    fn keys_and_colours_are_set() {
        let s = parse(
            "[keys]\nleft = h Left\npattern_shorter =\n  undo = ctrl+z  \n\n\
             [theme]\nbase = light\ncursor = black on 214 bold\n",
        )
        .unwrap();
        assert_eq!(s.keys.keys(Action::Left), [Key::Char('h'), Key::Left]);
        assert!(s.keys.keys(Action::PatternShorter).is_empty());
        assert_eq!(s.keys.keys(Action::Undo), [Key::Ctrl('z')]);
        assert_eq!(s.keys.keys(Action::Redo), Action::Redo.defaults());
        assert_eq!(s.base, "light");
        assert_eq!(s.theme.get(Role::Cursor), Style::fg(Color::Black).on(Color::Indexed(214)).bold());
        assert_eq!(s.theme.get(Role::Title), Theme::built_in("light").unwrap().get(Role::Title));
    }

    #[test]
    fn mistakes_name_their_line() {
        for (text, want) in [
            ("undo = u", "line 1: a setting must come after [keys] or [theme]"),
            ("[colors]", "line 1: unknown section [colors]"),
            ("[keys]\n\nundoo = u", "line 3: unknown action 'undoo'"),
            ("[keys]\nundo = u\nundo = z", "line 3: undo is set twice"),
            ("[keys]\nundo = ctrl+m", "line 2: ctrl+m cannot be told apart"),
            ("[keys]\nundo", "line 2: expected 'name = value'"),
            ("[keys]\nundo = n", "n is bound to both undo and next_pattern"),
            ("[keys]\nundo = u # comment", "line 2: unknown key 'comment'"),
            ("[theme]\nbase = dark", "line 2: unknown theme 'dark'"),
            ("[theme]\ncursor = pink", "line 2: unknown colour 'pink'"),
            ("[theme]\ncursr = red", "line 2: unknown colour role 'cursr'"),
            ("[theme]\nbase = light\nbase = light", "line 3: base is set twice"),
        ] {
            let err = parse(text).unwrap_err();
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }

    #[test]
    fn the_written_file_reads_back_the_same() {
        let defaults = Settings::default();
        assert_eq!(parse(&defaults.to_text()), Ok(defaults));
        let custom = parse(
            "[keys]\nleft = h left\nright = l right\npattern_shorter = ctrl+l\nchance_up = = +\nplay = space f5\n\
             [theme]\nbase = light\nplay_column = on #203040\n",
        )
        .unwrap();
        let text = custom.to_text();
        assert!(text.contains("\nchance_up = = +\n"), "{text}");
        assert!(text.contains("\nplay = space f5\n"), "{text}");
        assert!(text.contains("\nplay_column = default on #203040\n"), "{text}");
        assert_eq!(parse(&text), Ok(custom));
    }

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let dir = std::env::temp_dir().join(format!("tinvo-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let _ = std::fs::remove_file(&path);
        assert_eq!(load(&path), Ok((Settings::default(), false)));
        std::fs::write(&path, "[keys]\nundo = z\n").unwrap();
        let (s, found) = load(&path).unwrap();
        assert!(found && s.keys.is(Action::Undo, Key::Char('z')));
        std::fs::write(&path, "[keys]\nundo = zz\n").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.starts_with(&path.display().to_string()) && err.contains("line 2: unknown key 'zz'"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
