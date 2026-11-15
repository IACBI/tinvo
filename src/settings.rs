//! The settings file: key bindings, colours and options.
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
//!
//! [options]
//! mouse = off
//! record_mode = replace
//! ```
//!
//! An action listed here keeps only the keys listed (`pattern_shorter`
//! above has none); the others keep their defaults. `base` picks the
//! built-in theme the colours start from. `mouse = off` leaves the mouse
//! to the terminal; the other options ([`OPTIONS`]) set how notes are
//! recorded. `tinvo settings` prints every action and role with
//! what it has, and the editor's settings page changes them in place
//! ([`update_text`]), keeping the file's comments and order.

use crate::keymap::{self, Action, Context, Keymap};
use crate::theme::{self, Role, Theme};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "tinvo.conf";
/// The largest settings file read.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// Everything the settings file sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub keys: Keymap,
    /// The built-in theme the colours start from.
    pub base: &'static str,
    pub theme: Theme,
    /// Whether the editor takes mouse input. Off, the terminal keeps the
    /// mouse for selecting text.
    pub mouse: bool,
    pub recording: Recording,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            keys: Keymap::default(),
            base: theme::BUILT_IN[0],
            theme: Theme::default(),
            mouse: true,
            recording: Recording::default(),
        }
    }
}

/// How notes played while the pattern plays are recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recording {
    /// Each pass replaces the steps the playhead passes instead of adding
    /// to them.
    pub replace: bool,
    /// Notes land on their step instead of keeping their timing.
    pub quantize: bool,
    /// Bars counted in with a click when recording starts from a stop.
    pub count_in: u8,
}

/// The settings under `[options]`: a name, its values (the first is the
/// default) and what it does.
pub const OPTIONS: [(&str, &[&str], &str); 4] = [
    (
        "mouse",
        &["on", "off"],
        "on: clicks, drags and the wheel work in the editor (Shift+drag still selects text in most terminals); off: the terminal keeps the mouse",
    ),
    (
        "record_mode",
        &["overdub", "replace"],
        "recording notes: overdub adds them to the steps; replace clears each step the playhead passes, keeping what that pass played",
    ),
    (
        "record_timing",
        &["keep", "quantize"],
        "keep: recorded notes keep their timing as micro-timing; quantize: they land on the step",
    ),
    ("count_in", &["off", "1", "2"], "bars counted in with a click when recording starts from a stop"),
];

impl Settings {
    /// The value option `i` of [`OPTIONS`] has, as an index into its values.
    pub fn option(&self, i: usize) -> usize {
        match i {
            0 => usize::from(!self.mouse),
            1 => usize::from(self.recording.replace),
            2 => usize::from(self.recording.quantize),
            _ => usize::from(self.recording.count_in),
        }
    }

    pub fn set_option(&mut self, i: usize, value: usize) {
        match i {
            0 => self.mouse = value == 0,
            1 => self.recording.replace = value == 1,
            2 => self.recording.quantize = value == 1,
            _ => self.recording.count_in = value.min(OPTIONS[3].1.len() - 1) as u8,
        }
    }
}

/// "a or b", "a, b or c" (with `word` "or").
fn listing(values: &[&str], word: &str) -> String {
    match values {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} {word} {last}", rest.join(", ")),
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
    match crate::fsutil::read_text(path, MAX_FILE_BYTES) {
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
    Options,
}

/// Reads a settings file's text; a mistake is reported with its line.
pub fn parse(text: &str) -> Result<Settings, String> {
    let mut section = Section::None;
    let mut keys = Keymap::default();
    let mut bound: Vec<Action> = Vec::new();
    let mut base: Option<&'static str> = None;
    let mut settings = Settings::default();
    let mut options_set = [false; OPTIONS.len()];
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
                "options" => Section::Options,
                other => {
                    return Err(at(format!("unknown section [{other}] (there are [keys], [theme] and [options])")));
                }
            };
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(at("expected 'name = value'".into()));
        };
        let (name, value) = (name.trim(), value.trim());
        match section {
            Section::None => return Err(at("a setting must come after [keys], [theme] or [options]".into())),
            Section::Options => {
                let names: Vec<&str> = OPTIONS.iter().map(|o| o.0).collect();
                let Some(i) = OPTIONS.iter().position(|o| o.0 == name) else {
                    return Err(at(format!("unknown option '{name}' (there are {})", listing(&names, "and"))));
                };
                if std::mem::replace(&mut options_set[i], true) {
                    return Err(at(format!("{name} is set twice")));
                }
                let values = OPTIONS[i].1;
                let v = values
                    .iter()
                    .position(|&v| v == value)
                    .ok_or_else(|| at(format!("{name} is {}, not '{value}'", listing(values, "or"))))?;
                settings.set_option(i, v);
            }
            Section::Keys => {
                let a = Action::from_id(name).ok_or_else(|| at(format!("unknown action '{name}'")))?;
                if bound.contains(&a) {
                    return Err(at(format!("{name} is set twice")));
                }
                bound.push(a);
                let list = value.split_whitespace().take(keymap::MAX_KEYS + 1);
                let list = list.map(keymap::parse_key).collect::<Result<Vec<_>, _>>().map_err(at)?;
                if list.len() > keymap::MAX_KEYS {
                    return Err(at(format!("{name} has more than {} keys", keymap::MAX_KEYS)));
                }
                keys.bind(a, list);
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
    Ok(Settings { keys, base, theme, ..settings })
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
        out += "\n[options]\n";
        for (i, (name, values, help)) in OPTIONS.iter().enumerate() {
            out += &format!("# {help} ({}).\n{name} = {}\n", listing(values, "or"), values[self.option(i)]);
        }
        out
    }
}

impl Settings {
    /// The colour roles set apart from the base theme.
    fn own_colours(&self) -> Vec<Role> {
        let base = Theme::built_in(self.base).unwrap_or_default();
        Role::ALL.iter().copied().filter(|&r| self.theme.get(r) != base.get(r)).collect()
    }

    /// Starts the colours from the built-in theme `base`, keeping the roles
    /// set apart from the old one.
    pub fn set_base(&mut self, base: &'static str) {
        let own: Vec<(Role, crate::tui::Style)> =
            self.own_colours().into_iter().map(|r| (r, self.theme.get(r))).collect();
        self.base = base;
        self.theme = Theme::built_in(base).unwrap_or_default();
        for (r, st) in own {
            self.theme.set(r, st);
        }
    }
}

/// `theirs` (the settings file as it is now) with the changes from `base`
/// (as it was loaded) to `ours` (what this session made of it): a setting
/// this session did not change keeps what another session may have written
/// meanwhile. A colour counts as changed only if it differs from what a
/// change of base theme alone gives.
pub fn merge(base: &Settings, ours: &Settings, theirs: &Settings) -> Settings {
    let mut out = theirs.clone();
    let mut rebased = base.clone();
    if ours.base != base.base {
        out.set_base(ours.base);
        rebased.set_base(ours.base);
    }
    for &a in Action::ALL.iter() {
        if ours.keys.keys(a) != base.keys.keys(a) {
            out.keys.bind(a, ours.keys.keys(a).to_vec());
        }
    }
    for &r in Role::ALL.iter() {
        if ours.theme.get(r) != rebased.theme.get(r) {
            out.theme.set(r, ours.theme.get(r));
        }
    }
    for i in 0..OPTIONS.len() {
        if ours.option(i) != base.option(i) {
            out.set_option(i, ours.option(i));
        }
    }
    out
}

/// What a setting's line says, as the file writes it.
fn keys_value(keys: &Keymap, a: Action) -> String {
    keys.keys(a).iter().map(|&k| keymap::key_name(k)).collect::<Vec<_>>().join(" ")
}

/// The settings file `old` (its text; empty for none) changed to give
/// `new`: the line of each setting that changes is rewritten in place, a
/// setting with no line that now differs from its default gets one at the
/// end of its section (and the section is added if missing), and every
/// other line, comments included, stays as it was. Fails if `old` does not
/// read (it changed since it was loaded) or, as a safeguard, if the result
/// would not read back as `new`.
pub fn update_text(old: &str, new: &Settings) -> Result<String, String> {
    parse(old).map_err(|e| format!("the settings file has a mistake ({e}); it was not changed"))?;
    let base_theme = Theme::built_in(new.base).unwrap_or_default();
    let mut lines: Vec<String> = old.lines().map(str::to_string).collect();
    // Which settings have a line, and where each section's lines end.
    let mut has_key = vec![false; Action::ALL.len()];
    let mut has_role = vec![false; Role::ALL.len()];
    let mut has_base = false;
    let mut has_option = [false; OPTIONS.len()];
    let mut ends: [Option<usize>; 3] = [None; 3];
    let mut section = Section::None;
    for (i, line) in lines.iter_mut().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(name) = t.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = match name.trim() {
                "keys" => Section::Keys,
                "theme" => Section::Theme,
                _ => Section::Options,
            };
        } else if let Some((name, _)) = t.split_once('=') {
            let name = name.trim().to_string();
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            let value = match section {
                Section::Keys => Action::from_id(&name).map(|a| {
                    has_key[a as usize] = true;
                    keys_value(&new.keys, a)
                }),
                Section::Theme if name == "base" => {
                    has_base = true;
                    Some(new.base.to_string())
                }
                Section::Theme => Role::from_id(&name).map(|r| {
                    has_role[r as usize] = true;
                    theme::style_name(new.theme.get(r))
                }),
                Section::Options => OPTIONS.iter().position(|o| o.0 == name).map(|i| {
                    has_option[i] = true;
                    OPTIONS[i].1[new.option(i)].to_string()
                }),
                Section::None => None,
            };
            if let Some(v) = value {
                *line = if v.is_empty() { format!("{indent}{name} =") } else { format!("{indent}{name} = {v}") };
            }
        }
        let slot = match section {
            Section::Keys => 0,
            Section::Theme => 1,
            Section::Options => 2,
            Section::None => continue,
        };
        ends[slot] = Some(i + 1);
    }
    // Lines for settings without one that now differ from their default.
    let mut add: [Vec<String>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for &a in Action::ALL {
        if !has_key[a as usize] && new.keys.keys(a) != a.defaults() {
            let v = keys_value(&new.keys, a);
            add[0].push(if v.is_empty() { format!("{} =", a.id()) } else { format!("{} = {v}", a.id()) });
        }
    }
    if !has_base && new.base != theme::BUILT_IN[0] {
        add[1].push(format!("base = {}", new.base));
    }
    for &r in Role::ALL {
        if !has_role[r as usize] && new.theme.get(r) != base_theme.get(r) {
            add[1].push(format!("{} = {}", r.id(), theme::style_name(new.theme.get(r))));
        }
    }
    for (i, (name, values, _)) in OPTIONS.iter().enumerate() {
        if !has_option[i] && new.option(i) != 0 {
            add[2].push(format!("{name} = {}", values[new.option(i)]));
        }
    }
    // From the last section in the file back, so earlier positions hold.
    let mut order: Vec<usize> = (0..3).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(ends[i]));
    for i in order {
        let new_lines = std::mem::take(&mut add[i]);
        if new_lines.is_empty() {
            continue;
        }
        match ends[i] {
            Some(at) => {
                lines.splice(at..at, new_lines);
            }
            None => {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(["[keys]", "[theme]", "[options]"][i].to_string());
                lines.extend(new_lines);
            }
        }
    }
    let mut text = lines.join("\n");
    if !lines.is_empty() {
        text.push('\n');
    }
    match parse(&text) {
        Ok(read) if read == *new => Ok(text),
        Ok(_) => Err("the settings could not be written so that they read back the same".into()),
        Err(e) => Err(format!("the settings could not be written ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{Color, Key, Style};

    #[test]
    fn changes_are_written_in_place_keeping_the_rest() {
        let old = "# my settings\n[keys]\n# moving\n  left = h left\nundo = u\n\n[theme]\nbase = light\n# warm\ncursor = black on 214 bold\n";
        let mut new = parse(old).unwrap();
        new.keys.bind(Action::Left, vec![Key::Char('j')]);
        new.keys.bind(Action::Redo, vec![]);
        new.theme.set(Role::Title, Style::fg(Color::Red));
        new.mouse = false;
        let text = update_text(old, &new).unwrap();
        assert_eq!(
            text,
            "# my settings\n[keys]\n# moving\n  left = j\nundo = u\nredo =\n\n[theme]\nbase = light\n# warm\ncursor = black on 214 bold\ntitle = red\n\n[options]\nmouse = off\n"
        );
        assert_eq!(parse(&text), Ok(new.clone()));
        // Nothing changed, nothing moves.
        assert_eq!(update_text(&text, &new).unwrap(), text);
        // Back to a default: the line stays, saying the default.
        let mut back = new.clone();
        back.keys.bind(Action::Left, Action::Left.defaults().to_vec());
        assert!(update_text(&text, &back).unwrap().contains("\n  left = left\n"));
    }

    #[test]
    fn a_new_file_holds_only_what_differs() {
        assert_eq!(update_text("", &Settings::default()).unwrap(), "");
        let mut s = Settings::default();
        s.keys.bind(Action::Undo, vec![Key::Char('z'), Key::Ctrl('z')]);
        s.set_base("light");
        let text = update_text("", &s).unwrap();
        assert_eq!(text, "[keys]\nundo = z ctrl+z\n\n[theme]\nbase = light\n");
        assert_eq!(parse(&text), Ok(s));
    }

    #[test]
    fn a_new_base_keeps_the_colours_set_apart() {
        let mut s = Settings::default();
        s.theme.set(Role::Cursor, Style::fg(Color::Green));
        s.set_base("light");
        let light = Theme::built_in("light").unwrap();
        assert_eq!(s.theme.get(Role::Cursor), Style::fg(Color::Green));
        assert_eq!(s.theme.get(Role::Title), light.get(Role::Title));
        let text = update_text("[theme]\ncursor = green\n", &s).unwrap();
        assert_eq!(parse(&text), Ok(s.clone()));
        s.set_base("default");
        assert_eq!(s.theme, {
            let mut t = Theme::default();
            t.set(Role::Cursor, Style::fg(Color::Green));
            t
        });
    }

    #[test]
    fn a_merge_keeps_what_another_session_saved() {
        let base = Settings::default();
        let mut ours = base.clone();
        ours.keys.bind(Action::Undo, vec![Key::Char('z')]);
        ours.set_base("light");
        ours.theme.set(Role::Cursor, Style::fg(Color::Red));
        let mut theirs = base.clone();
        theirs.mouse = false;
        theirs.keys.bind(Action::Redo, vec![Key::Char('Z')]);
        theirs.theme.set(Role::Title, Style::fg(Color::Green));
        theirs.keys.bind(Action::Undo, vec![Key::Char('y')]);
        let out = merge(&base, &ours, &theirs);
        assert!(!out.mouse, "untouched here, so theirs");
        assert_eq!(out.keys.keys(Action::Redo), [Key::Char('Z')]);
        assert_eq!(out.keys.keys(Action::Undo), [Key::Char('z')], "changed here, so ours");
        assert_eq!(out.base, "light");
        assert_eq!(out.theme.get(Role::Cursor), Style::fg(Color::Red));
        assert_eq!(out.theme.get(Role::Title), Style::fg(Color::Green), "their own colour survives the new base");
        let light = Theme::built_in("light").unwrap();
        assert_eq!(out.theme.get(Role::Header), light.get(Role::Header));
        assert_eq!(merge(&base, &base, &theirs), theirs, "no change here leaves theirs");
        assert_eq!(merge(&base, &ours, &base), ours, "no change there gives ours");
    }

    #[test]
    fn a_file_that_no_longer_reads_is_left_alone() {
        let err = update_text("[keys]\nundoo = u\n", &Settings::default()).unwrap_err();
        assert!(err.contains("line 2: unknown action 'undoo'") && err.contains("not changed"), "{err}");
    }

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
        assert!(s.mouse);
        assert!(!parse("[options]\nmouse = off\n").unwrap().mouse);
    }

    #[test]
    fn mistakes_name_their_line() {
        for (text, want) in [
            ("undo = u", "line 1: a setting must come after [keys], [theme] or [options]"),
            ("[options]\nmouse = yes", "line 2: mouse is on or off, not 'yes'"),
            (
                "[options]\nmice = on",
                "line 2: unknown option 'mice' (there are mouse, record_mode, record_timing and count_in)",
            ),
            ("[options]\ncount_in = 3", "line 2: count_in is off, 1 or 2, not '3'"),
            ("[options]\nrecord_mode = keep", "line 2: record_mode is overdub or replace, not 'keep'"),
            ("[options]\nmouse = on\nmouse = off", "line 3: mouse is set twice"),
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
            ("[keys]\nundo = 1 2 3 4 5 6 7 8 9", "line 2: undo has more than 8 keys"),
        ] {
            let err = parse(text).unwrap_err();
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }

    #[test]
    fn a_settings_file_too_large_is_refused() {
        let dir = std::env::temp_dir().join(format!("tinvo-conf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let mut text = "[keys]\nundo = u\n".to_string();
        text.push_str(&"#".repeat(MAX_FILE_BYTES as usize - text.len()));
        std::fs::write(&path, &text).unwrap();
        assert!(load(&path).unwrap().1);
        text.push('#');
        std::fs::write(&path, &text).unwrap();
        assert!(load(&path).unwrap_err().ends_with("larger than 1 MiB"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_written_file_reads_back_the_same() {
        let defaults = Settings::default();
        assert_eq!(parse(&defaults.to_text()), Ok(defaults));
        let custom = parse(
            "[keys]\nleft = h left\nright = l right\npattern_shorter = ctrl+l\nchance_up = = +\nplay = space f5\n\
             [theme]\nbase = light\nplay_column = on #203040\n[options]\nmouse = off\n",
        )
        .unwrap();
        let text = custom.to_text();
        assert!(text.contains("\nmouse = off\n"), "{text}");
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
