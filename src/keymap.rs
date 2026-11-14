//! Which key does what: the editor's actions, the keys they start with, and
//! the user's bindings from the settings file.
//!
//! Every action belongs to a context, the part of the editor where it
//! applies. Contexts that are active at the same time cannot share a key,
//! so a key always means one thing wherever the cursor is. Questions (y/n),
//! the track-type menu after `add_track`, and text prompts keep fixed keys.

use crate::tui::Key;

/// Where an action applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    /// Everywhere in the editor.
    Global,
    /// On the selected track, outside the song view.
    Track,
    /// Moving and acting on the cursor's cell, in every view and list.
    Nav,
    /// On the step under the cursor, in the grid and the piano roll.
    Step,
    Roll,
    Auto,
    Song,
    Params,
}

impl Context {
    pub const ALL: [Context; 8] = [
        Context::Global,
        Context::Track,
        Context::Nav,
        Context::Step,
        Context::Roll,
        Context::Auto,
        Context::Song,
        Context::Params,
    ];

    /// A heading for the help screen and the settings listing.
    pub fn title(self) -> &'static str {
        match self {
            Context::Global => "Anywhere",
            Context::Track => "The selected track",
            Context::Nav => "The cursor (grid, piano roll, automation, song, parameters, lists)",
            Context::Step => "The step under the cursor (grid and piano roll)",
            Context::Roll => "Piano roll",
            Context::Auto => "Automation view",
            Context::Song => "Song view",
            Context::Params => "Parameter panel",
        }
    }
}

/// The sets of contexts that are active together; within one, a key may
/// have only one action.
const TOGETHER: [&[Context]; 4] = [
    &[Context::Global, Context::Track, Context::Nav, Context::Step, Context::Roll],
    &[Context::Global, Context::Track, Context::Nav, Context::Auto],
    &[Context::Global, Context::Track, Context::Nav, Context::Params],
    &[Context::Global, Context::Song, Context::Nav],
];

macro_rules! actions {
    ($($name:ident $id:literal $ctx:ident [$($key:expr),*] $help:literal;)*) => {
        /// Something a key can do.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Action { $($name),* }

        impl Action {
            pub const ALL: &'static [Action] = &[$(Action::$name),*];

            /// The action's name in the settings file.
            pub fn id(self) -> &'static str {
                match self { $(Action::$name => $id),* }
            }

            pub fn context(self) -> Context {
                match self { $(Action::$name => Context::$ctx),* }
            }

            pub fn help(self) -> &'static str {
                match self { $(Action::$name => $help),* }
            }

            /// The keys the action has unless the settings say otherwise.
            pub fn defaults(self) -> &'static [Key] {
                match self { $(Action::$name => const { &[$($key),*] }),* }
            }
        }
    };
}

use Key::{BackTab, Backspace, Ctrl, Delete, Down, End, Enter, F, Home, Left, PageDown, PageUp, Right, Tab, Up};
const fn c(ch: char) -> Key {
    Key::Char(ch)
}

actions! {
    Play "play" Global [c(' ')] "play / stop";
    Focus "focus" Global [Tab, BackTab] "switch between the grid and the parameters";
    Help "help" Global [c('?'), F(1)] "this list";
    Quit "quit" Global [c('q'), Ctrl('c'), Ctrl('q')] "quit (asks if there are unsaved changes)";
    Save "save" Global [Ctrl('s'), c('w')] "save";
    SaveAs "save_as" Global [c('W')] "save under a new name";
    Export "export" Global [c('e')] "export WAV (←→ song / pattern / loop region, Tab format, ↑↓ repeats)";
    Undo "undo" Global [c('u'), Ctrl('z')] "undo";
    Redo "redo" Global [c('U'), Ctrl('y')] "redo";
    AddTrack "add_track" Global [c('a')] "add a track (then 1-7 picks its type)";
    TempoDown "tempo_down" Global [c('t')] "tempo down";
    TempoUp "tempo_up" Global [c('T')] "tempo up";
    SwingDown "swing_down" Global [c('g')] "swing down";
    SwingUp "swing_up" Global [c('G')] "swing up (50-75 %)";
    PatternShorter "pattern_shorter" Global [c('l')] "pattern length down";
    PatternLonger "pattern_longer" Global [c('L')] "pattern length up";
    NextPattern "next_pattern" Global [c('n')] "next pattern (A-P); while a pattern plays, it plays next";
    PrevPattern "prev_pattern" Global [c('N')] "previous pattern";
    SongView "song_view" Global [c('S')] "song view / back to the grid";
    Copy "copy" Global [c('y')] "copy the pattern";
    Paste "paste" Global [c('Y')] "paste the pattern";
    ClearPattern "clear_pattern" Global [c('X')] "clear the pattern";
    Page "page" Global [c('f')] "parameter panel page: instrument, channel strip, buses and master";
    Devices "devices" Global [c('O')] "audio output and input devices (Tab: low-latency output; ←→: output / input)";
    MidiInputs "midi_inputs" Global [c('M')] "MIDI input";
    StepEntry "step_entry" Global [c('R')] "step entry: MIDI notes write the step at the cursor (held together: a chord)";
    Record "record" Global [c('I')] "record into the sampler track; again stops and loads the take";
    DeleteTrack "delete_track" Track [c('D')] "delete the track";
    RenameTrack "rename_track" Track [c('r')] "rename the track";
    Mute "mute" Track [c('m')] "mute the track";
    Solo "solo" Track [c('s')] "solo the track";
    Preview "preview" Track [c('p')] "preview the track's note";
    LoadSample "load_sample" Track [c('o')] "load a WAV file into a sampler track";
    Automate "automate" Track [c('A')] "in the panel: give the parameter a lane; in the grid: show the lanes";
    PianoRoll "piano_roll" Track [c('v')] "piano roll for a melodic track / back to the grid";
    Left "left" Nav [Left] "step left; in the panel: value down; in lists: other page";
    Right "right" Nav [Right] "step right; in the panel: value up; in lists: other page";
    Up "up" Nav [Up] "track, pitch, lane, parameter or song row above";
    Down "down" Nav [Down] "track, pitch, lane, parameter or song row below";
    First "first" Nav [Home] "first step or song entry";
    Last "last" Nav [End] "last step or song entry";
    Enter "enter" Nav [Enter, c('x')] "toggle the step or note, add a point, repeat a song entry, choose in a list";
    Clear "clear" Nav [Delete, Backspace] "clear the step or point, remove the song entry";
    PitchDown "pitch_down" Step [c(',')] "pitch down a semitone";
    PitchUp "pitch_up" Step [c('.')] "pitch up a semitone";
    OctaveDown "octave_down" Step [c('<')] "pitch down an octave";
    OctaveUp "octave_up" Step [c('>')] "pitch up an octave";
    Chord "chord" Step [c('c')] "cycle the chord: maj, min, 7, m7, maj7, sus4 (green in the grid)";
    Earlier "earlier" Step [c('(')] "move the step earlier (1/24 step, up to half a step)";
    Later "later" Step [c(')')] "move the step later";
    VelocityDown "velocity_down" Step [c('[')] "velocity down";
    VelocityUp "velocity_up" Step [c(']')] "velocity up";
    Shorter "shorter" Step [c('{')] "length down";
    Longer "longer" Step [c('}')] "length up";
    ChanceDown "chance_down" Step [c('-')] "chance down (yellow: below 100 %)";
    ChanceUp "chance_up" Step [c('=')] "chance up";
    Ratchet "ratchet" Step [c('/')] "ratchet: 1-4 hits per step";
    RollOctaveUp "roll_octave_up" Roll [PageUp] "cursor pitch up an octave";
    RollOctaveDown "roll_octave_down" Roll [PageDown] "cursor pitch down an octave";
    PointDown "point_down" Auto [c('[')] "point value down";
    PointUp "point_up" Auto [c(']')] "point value up";
    PointCoarseDown "point_coarse_down" Auto [c('{')] "point value down, coarse";
    PointCoarseUp "point_coarse_up" Auto [c('}')] "point value up, coarse";
    Ramp "ramp" Auto [c('/')] "hold each point or ramp between them";
    RemoveLane "remove_lane" Auto [c('-')] "remove the lane";
    LoopStart "loop_start" Song [c('[')] "start the loop region here";
    LoopEnd "loop_end" Song [c(']')] "end the loop region here";
    NoLoop "no_loop" Song [c('-')] "no loop region";
    SongPrev "song_prev" Song [c(',')] "previous pattern of the entry, or of the track in it (its own, the entry's, or a rest)";
    SongNext "song_next" Song [c('.')] "next pattern of the entry, or of the track in it";
    Marker "marker" Song [c('m')] "name a marker on the song entry (an empty name removes it)";
    PrevMarker "prev_marker" Song [PageUp] "to the previous marker";
    NextMarker "next_marker" Song [PageDown] "to the next marker";
    LoopSection "loop_section" Song [c('=')] "loop the section: from the marker before the entry to the next one";
    CoarseUp "coarse_up" Params [PageUp] "value up, coarse";
    CoarseDown "coarse_down" Params [PageDown] "value down, coarse";
    Default "default" Params [c('0')] "back to the default value";
}

impl Action {
    pub fn from_id(id: &str) -> Option<Action> {
        Action::ALL.iter().copied().find(|a| a.id() == id)
    }
}

/// The keys of every action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    keys: Vec<Vec<Key>>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap { keys: Action::ALL.iter().map(|a| a.defaults().to_vec()).collect() }
    }
}

impl Keymap {
    fn index(a: Action) -> usize {
        a as usize
    }

    pub fn keys(&self, a: Action) -> &[Key] {
        &self.keys[Self::index(a)]
    }

    /// Gives `a` exactly `keys`; none leaves it without a key.
    pub fn bind(&mut self, a: Action, keys: Vec<Key>) {
        self.keys[Self::index(a)] = keys;
    }

    /// The action `key` starts among those of `contexts`.
    pub fn action(&self, contexts: &[Context], key: Key) -> Option<Action> {
        Action::ALL.iter().copied().find(|&a| contexts.contains(&a.context()) && self.keys(a).contains(&key))
    }

    /// Whether `key` starts `a`.
    pub fn is(&self, a: Action, key: Key) -> bool {
        self.keys(a).contains(&key)
    }

    /// The first key of `a`, for hints, or "(unbound)".
    pub fn label(&self, a: Action) -> String {
        self.keys(a).first().map_or("(unbound)".to_string(), |&k| key_label(k))
    }

    /// The first key of each action, run together when every one is an
    /// arrow (`←→`), otherwise separated by spaces (`[ ]`).
    pub fn labels(&self, actions: &[Action]) -> String {
        let names: Vec<String> = actions.iter().map(|&a| self.label(a)).collect();
        let arrows = names.iter().all(|n| matches!(n.as_str(), "←" | "→" | "↑" | "↓"));
        names.join(if arrows { "" } else { " " })
    }

    /// Every key of `a`, for the help screen.
    pub fn all_labels(&self, a: Action) -> String {
        let keys = self.keys(a);
        if keys.is_empty() {
            return "(unbound)".to_string();
        }
        keys.iter().map(|&k| key_label(k)).collect::<Vec<_>>().join(" ")
    }

    /// Fails if a key is given twice where the actions it would start are
    /// active together.
    pub fn check(&self) -> Result<(), String> {
        let mut clashes = Vec::new();
        for (i, &a) in Action::ALL.iter().enumerate() {
            for &b in &Action::ALL[i + 1..] {
                let together =
                    a == b || TOGETHER.iter().any(|set| set.contains(&a.context()) && set.contains(&b.context()));
                if !together {
                    continue;
                }
                for &k in self.keys(a) {
                    if self.keys(b).contains(&k) {
                        clashes.push(format!("{} is bound to both {} and {}", key_name(k), a.id(), b.id()));
                    }
                }
            }
            let keys = self.keys(a);
            for (j, k) in keys.iter().enumerate() {
                if keys[..j].contains(k) {
                    clashes.push(format!("{} is given twice to {}", key_name(*k), a.id()));
                }
            }
        }
        if clashes.is_empty() { Ok(()) } else { Err(clashes.join("; ")) }
    }
}

/// How a key is shown on screen.
pub fn key_label(k: Key) -> String {
    match k {
        Key::Char(' ') => "Space".into(),
        Key::Char(ch) => ch.to_string(),
        Key::Ctrl(ch) => format!("Ctrl+{}", ch.to_ascii_uppercase()),
        Key::Up => "↑".into(),
        Key::Down => "↓".into(),
        Key::Left => "←".into(),
        Key::Right => "→".into(),
        Key::Enter => "Enter".into(),
        Key::Esc => "Esc".into(),
        Key::Backspace => "Backspace".into(),
        Key::Tab => "Tab".into(),
        Key::BackTab => "Shift+Tab".into(),
        Key::Delete => "Del".into(),
        Key::Home => "Home".into(),
        Key::End => "End".into(),
        Key::PageUp => "PgUp".into(),
        Key::PageDown => "PgDn".into(),
        Key::F(n) => format!("F{n}"),
    }
}

const NAMED: [(&str, Key); 15] = [
    ("space", Key::Char(' ')),
    ("up", Key::Up),
    ("down", Key::Down),
    ("left", Key::Left),
    ("right", Key::Right),
    ("enter", Key::Enter),
    ("esc", Key::Esc),
    ("backspace", Key::Backspace),
    ("tab", Key::Tab),
    ("shift+tab", Key::BackTab),
    ("delete", Key::Delete),
    ("home", Key::Home),
    ("end", Key::End),
    ("pageup", Key::PageUp),
    ("pagedown", Key::PageDown),
];

/// How a key is written in the settings file; [`parse_key`] reads it back.
pub fn key_name(k: Key) -> String {
    match k {
        Key::Char(ch) if ch != ' ' => ch.to_string(),
        Key::Ctrl(ch) => format!("ctrl+{ch}"),
        Key::F(n) => format!("f{n}"),
        _ => NAMED.iter().find(|(_, key)| *key == k).map_or("?".into(), |(n, _)| n.to_string()),
    }
}

/// Reads a key as the settings file writes it: one character for itself,
/// or a name such as `space`, `pageup`, `ctrl+z` or `f5`, in any case.
pub fn parse_key(s: &str) -> Result<Key, String> {
    let mut chars = s.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        return if ch.is_control() || ch.is_whitespace() {
            Err(format!("'{}' cannot be bound; write the key's name", ch.escape_default()))
        } else {
            Ok(Key::Char(ch))
        };
    }
    let lower = s.to_ascii_lowercase();
    if let Some(&(_, k)) = NAMED.iter().find(|(n, _)| *n == lower) {
        return Ok(k);
    }
    if let Some(rest) = lower.strip_prefix("ctrl+") {
        return match rest.as_bytes() {
            // These arrive as Backspace, Tab and Enter.
            [b'h' | b'i' | b'j' | b'm'] => Err(format!("{s} cannot be told apart from another key")),
            [l @ b'a'..=b'z'] => Ok(Key::Ctrl(char::from(*l))),
            _ => Err(format!("unknown key '{s}' (ctrl+ takes a letter)")),
        };
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        if (1..=12).contains(&n) {
            return Ok(Key::F(n));
        }
    }
    Err(format!("unknown key '{s}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_have_no_clashes_and_unique_names() {
        Keymap::default().check().unwrap();
        for (i, a) in Action::ALL.iter().enumerate() {
            assert_eq!(Action::from_id(a.id()), Some(*a));
            assert!(Action::ALL[..i].iter().all(|b| b.id() != a.id()), "{} twice", a.id());
            assert!(!a.help().is_empty() && !a.defaults().is_empty(), "{}", a.id());
            assert_eq!(Keymap::index(*a), i);
        }
    }

    #[test]
    fn a_key_finds_the_action_of_its_context() {
        let k = Keymap::default();
        let step = [Context::Global, Context::Track, Context::Nav, Context::Step];
        assert_eq!(k.action(&step, Key::Char('[')), Some(Action::VelocityDown));
        assert_eq!(k.action(&[Context::Song, Context::Nav], Key::Char('[')), Some(Action::LoopStart));
        assert_eq!(k.action(&[Context::Auto], Key::Char('[')), Some(Action::PointDown));
        assert_eq!(k.action(&[Context::Global], Key::Ctrl('z')), Some(Action::Undo));
        assert_eq!(k.action(&[Context::Global], Key::Char('[')), None);
    }

    #[test]
    fn rebinding_replaces_the_keys_and_clashes_are_refused() {
        let mut k = Keymap::default();
        k.bind(Action::Left, vec![Key::Char('h'), Key::Left]);
        k.check().unwrap();
        assert_eq!(k.action(&[Context::Nav], Key::Char('h')), Some(Action::Left));
        // l is the pattern length, which is active wherever the cursor is.
        k.bind(Action::Right, vec![Key::Char('l')]);
        let err = k.check().unwrap_err();
        assert!(err.contains("l is bound to both pattern_shorter and right"), "{err}");
        k.bind(Action::PatternShorter, vec![Key::Char('h')]);
        assert!(k.check().unwrap_err().contains("h is bound to both pattern_shorter and left"));
        k.bind(Action::PatternShorter, vec![]);
        k.check().unwrap();
        assert_eq!(k.label(Action::PatternShorter), "(unbound)");
        // The song view and the grid are never active together.
        k.bind(Action::LoopStart, vec![Key::Char('(')]);
        k.check().unwrap();
        k.bind(Action::Undo, vec![Key::Char('u'), Key::Char('u')]);
        assert!(k.check().unwrap_err().contains("u is given twice to undo"));
    }

    #[test]
    fn keys_are_named_and_read_back() {
        for a in Action::ALL {
            for &key in a.defaults() {
                assert_eq!(parse_key(&key_name(key)), Ok(key), "{key:?}");
            }
        }
        for n in 1..=12 {
            assert_eq!(parse_key(&key_name(Key::F(n))), Ok(Key::F(n)));
        }
        assert_eq!(parse_key("Ctrl+K"), Ok(Key::Ctrl('k')));
        assert_eq!(parse_key("PageUp"), Ok(Key::PageUp));
        assert_eq!(parse_key("ö"), Ok(Key::Char('ö')));
        assert_eq!(parse_key("U"), Ok(Key::Char('U')));
        assert_eq!(parse_key("esc"), Ok(Key::Esc));
        for bad in ["ctrl+m", "ctrl+i", "ctrl+1", "f13", "f0", "pgup", "\t", "spacebar"] {
            assert!(parse_key(bad).is_err(), "{bad}");
        }
        assert_eq!(key_label(Key::Ctrl('z')), "Ctrl+Z");
        let k = Keymap::default();
        assert_eq!(k.labels(&[Action::Left, Action::Right]), "←→");
        assert_eq!(k.labels(&[Action::VelocityDown, Action::VelocityUp]), "[ ]");
        assert_eq!(k.all_labels(Action::Undo), "u Ctrl+Z");
    }
}
