//! The editor's colours: a style for each role something on screen plays,
//! from a built-in theme and the settings file.

use crate::tui::{Color, Style};

const fn fg(c: Color) -> Style {
    Style::fg(c)
}

const fn bg(c: Color) -> Style {
    Style::DEFAULT.on(c)
}

macro_rules! roles {
    ($($name:ident $id:literal [$dark:expr] [$light:expr] $help:literal;)*) => {
        /// Something on screen that has its own style.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Role { $($name),* }

        impl Role {
            pub const ALL: &'static [Role] = &[$(Role::$name),*];

            /// The role's name in the settings file.
            pub fn id(self) -> &'static str {
                match self { $(Role::$name => $id),* }
            }

            pub fn help(self) -> &'static str {
                match self { $(Role::$name => $help),* }
            }

            fn dark(self) -> Style {
                match self { $(Role::$name => $dark),* }
            }

            fn light(self) -> Style {
                match self { $(Role::$name => $light),* }
            }
        }
    };
}

use Color::{
    Black, Blue, BrightCyan, BrightGreen, BrightMagenta, BrightRed, BrightWhite, BrightYellow, Cyan, Gray, Green,
    Magenta, Red, White, Yellow,
};

roles! {
        Text "text" [Style::DEFAULT] [Style::DEFAULT] "ordinary text";
        Dim "dim" [fg(Gray)] [fg(Gray)] "separators, column heads, empty steps";
        Beat "beat" [fg(White)] [fg(Black)] "empty steps on a beat, beat numbers, information lines";
        Title "title" [fg(BrightWhite).bold()] [fg(Black).bold()] "the title of a view";
    Header "header" [fg(BrightWhite).on(Blue)] [fg(Black).on(White)] "the top bar";
    HeaderPlay "header_play" [fg(BrightGreen).on(Blue).bold()] [fg(Blue).on(White).bold()] "PLAY in the top bar";
    Playhead "playhead" [fg(BrightGreen)] [fg(Blue)] "the marker above the playing step";
    PlayColumn "play_column" [bg(Blue)] [bg(BrightCyan)] "the playing step (background only)";
        Cursor "cursor" [fg(Black).on(BrightWhite)] [fg(BrightWhite).on(Black)] "the cursor";
        CursorIdle "cursor_idle" [fg(Black).on(Gray)] [fg(BrightWhite).on(Gray)] "the grid cursor while the panel has the keys";
    CursorColumn "cursor_column" [bg(Black)] [bg(White)] "the cursor's step in the piano roll (background only)";
        Selected "selected" [fg(BrightYellow)] [fg(Blue)] "the marker of the selected track or lane";
        SelectedName "selected_name" [fg(BrightWhite).bold()] [fg(Black).bold()] "the name of the selected track or lane";
        ParamSelected "param_selected" [fg(BrightWhite)] [fg(Blue).bold()] "the selected parameter while the grid has the keys";
        Note "note" [fg(BrightMagenta)] [fg(Magenta)] "a step with one note";
    Chord "chord" [fg(BrightGreen)] [fg(Red)] "a step with a chord";
    Drum "drum" [fg(BrightCyan)] [fg(Blue)] "a drum hit";
    Chance "chance" [fg(Yellow)] [fg(Black)] "a step that plays by chance";
        Held "held" [fg(Magenta)] [fg(BrightMagenta)] "a note still sounding, in the piano roll";
        Muted "muted" [fg(Gray)] [fg(Gray)] "the steps of a muted track (foreground only)";
        BlackKey "black_key" [fg(Black)] [fg(White)] "empty cells on black-key rows of the piano roll";
        PitchCursor "pitch_cursor" [fg(BrightYellow).bold()] [fg(Blue).bold()] "the cursor's pitch in the piano roll";
        PitchC "pitch_c" [fg(BrightWhite)] [fg(Black).bold()] "the C of each octave in the piano roll";
        Point "point" [fg(BrightYellow).bold()] [fg(Blue).bold()] "an automation point";
    Value "value" [fg(Yellow)] [fg(Gray)] "an automated value between points";
    Entry "entry" [fg(BrightCyan)] [fg(Black)] "a song entry";
    EntryLoop "entry_loop" [fg(BrightGreen)] [fg(Blue).bold()] "a song entry in the loop region";
    Marker "marker" [fg(BrightMagenta).bold()] [fg(Magenta).bold()] "a song marker and its name";
        Warning "warning" [fg(Yellow)] [fg(Red)] "what to do when a view is empty";
        Mute "mute" [fg(Black).on(Red)] [fg(BrightWhite).on(Red)] "M of a muted track";
        Solo "solo" [fg(Black).on(Yellow)] [fg(Black).on(Yellow)] "S of a soloed track";
        PanelFocus "panel_focus" [fg(BrightYellow).bold()] [fg(Blue).bold()] "the panel's title while it has the keys";
        Panel "panel" [fg(White)] [fg(Black)] "the panel's title otherwise";
        StatusBar "status_bar" [bg(Black)] [bg(White)] "the bottom line (background only)";
        Prompt "prompt" [fg(BrightWhite).on(Black).bold()] [fg(Black).on(White).bold()] "a question or prompt on the bottom line";
        Input "input" [fg(BrightWhite).on(Black)] [fg(Black).on(White)] "text being typed";
        Message "message" [fg(BrightYellow).on(Black)] [fg(Blue).on(White).bold()] "news on the bottom line";
        Hint "hint" [fg(Gray).on(Black)] [fg(Black).on(White)] "the key hints on the bottom line";
        ListCursor "list_cursor" [fg(Black).on(BrightCyan)] [fg(Black).on(BrightCyan)] "the cursor in a list";
        TabOn "tab_on" [fg(Black).on(BrightWhite).bold()] [fg(BrightWhite).on(Black).bold()] "the open page of the device list";
        HelpKey "help_key" [fg(BrightYellow).bold()] [fg(Blue).bold()] "keys on the help screen";
        AnswerKey "answer_key" [fg(BrightCyan).bold()] [fg(Blue).bold()] "the answers on the recovery screen";
    MeterLow "meter_low" [fg(BrightGreen)] [fg(Blue)] "a level meter below -6 dBFS (foreground only)";
    MeterHigh "meter_high" [fg(BrightYellow)] [fg(Magenta)] "a level meter from -6 dBFS (foreground only)";
    MeterClip "meter_clip" [fg(BrightRed)] [fg(Red)] "a level meter at full scale (foreground only)";
        MeterEmpty "meter_empty" [fg(Gray)] [fg(Gray)] "the empty part of a level meter (foreground only)";
}

impl Role {
    pub fn from_id(id: &str) -> Option<Role> {
        Role::ALL.iter().copied().find(|r| r.id() == id)
    }
}

/// The built-in themes, by name.
pub const BUILT_IN: [&str; 2] = ["default", "light"];

/// A style for every role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    styles: Vec<Style>,
}

impl Default for Theme {
    fn default() -> Self {
        Theme { styles: Role::ALL.iter().map(|r| r.dark()).collect() }
    }
}

impl Theme {
    /// A built-in theme: `default`, for dark terminals, or `light`.
    pub fn built_in(name: &str) -> Option<Theme> {
        match name {
            "default" => Some(Theme::default()),
            "light" => Some(Theme { styles: Role::ALL.iter().map(|r| r.light()).collect() }),
            _ => None,
        }
    }

    pub fn get(&self, r: Role) -> Style {
        self.styles[r as usize]
    }

    pub fn set(&mut self, r: Role, s: Style) {
        self.styles[r as usize] = s;
    }
}

const COLORS: [(&str, Color); 17] = [
    ("default", Color::Default),
    ("black", Black),
    ("red", Red),
    ("green", Green),
    ("yellow", Yellow),
    ("blue", Blue),
    ("magenta", Magenta),
    ("cyan", Cyan),
    ("white", White),
    ("gray", Gray),
    ("bright-red", BrightRed),
    ("bright-green", BrightGreen),
    ("bright-yellow", BrightYellow),
    ("bright-blue", Color::BrightBlue),
    ("bright-magenta", BrightMagenta),
    ("bright-cyan", BrightCyan),
    ("bright-white", BrightWhite),
];

/// Reads a colour: a name such as `bright-cyan`, an xterm palette index
/// (0-255), or `#rrggbb`.
pub fn parse_color(s: &str) -> Result<Color, String> {
    let lower = s.to_ascii_lowercase();
    if let Some(&(_, c)) = COLORS.iter().find(|(n, _)| *n == lower || (lower == "grey" && *n == "gray")) {
        return Ok(c);
    }
    if let Some(hex) = lower.strip_prefix('#') {
        if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
            return Ok(Color::Rgb(byte(0), byte(2), byte(4)));
        }
    } else if !lower.is_empty() && lower.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = lower.parse::<u8>() {
            return Ok(Color::Indexed(n));
        }
    }
    Err(format!("unknown colour '{s}' (a name, 0-255 or #rrggbb)"))
}

/// How a colour is written in the settings file.
pub fn color_name(c: Color) -> String {
    match c {
        Color::Indexed(n) => n.to_string(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        c => COLORS.iter().find(|(_, k)| *k == c).map_or("default".into(), |(n, _)| n.to_string()),
    }
}

/// Reads a style: `[FG] [on BG] [bold]`, such as `black on bright-white`.
pub fn parse_style(s: &str) -> Result<Style, String> {
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut style = Style::DEFAULT;
    let mut rest = &words[..];
    if let [first, tail @ ..] = rest {
        if !first.eq_ignore_ascii_case("on") && !first.eq_ignore_ascii_case("bold") {
            style.fg = parse_color(first)?;
            rest = tail;
        }
    }
    if let [on, tail @ ..] = rest {
        if on.eq_ignore_ascii_case("on") {
            let [color, tail @ ..] = tail else { return Err("'on' needs a colour".into()) };
            style.bg = parse_color(color)?;
            rest = tail;
        }
    }
    if let [b, tail @ ..] = rest {
        if b.eq_ignore_ascii_case("bold") {
            style.bold = true;
            rest = tail;
        }
    }
    match rest {
        [] if !words.is_empty() => Ok(style),
        [] => Err("a style needs a colour, 'on' and a colour, or 'bold'".into()),
        [extra, ..] => Err(format!("unexpected '{extra}' (a style is [colour] [on colour] [bold])")),
    }
}

/// How a style is written in the settings file; [`parse_style`] reads it
/// back.
pub fn style_name(s: Style) -> String {
    let mut out = color_name(s.fg);
    if s.bg != Color::Default {
        out += " on ";
        out += &color_name(s.bg);
    }
    if s.bold {
        out += " bold";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_named_once_and_themes_cover_them() {
        for (i, r) in Role::ALL.iter().enumerate() {
            assert_eq!(Role::from_id(r.id()), Some(*r));
            assert_eq!(*r as usize, i);
            assert!(Role::ALL[..i].iter().all(|o| o.id() != r.id()), "{} twice", r.id());
        }
        for name in BUILT_IN {
            let t = Theme::built_in(name).unwrap();
            assert_eq!(t.styles.len(), Role::ALL.len());
        }
        assert!(Theme::built_in("dark").is_none());
        assert_ne!(Theme::built_in("light"), Theme::built_in("default"));
    }

    #[test]
    fn styles_are_read_and_written_back() {
        for name in BUILT_IN {
            let t = Theme::built_in(name).unwrap();
            for r in Role::ALL {
                assert_eq!(parse_style(&style_name(t.get(*r))), Ok(t.get(*r)), "{name} {}", r.id());
            }
        }
        let s = parse_style("Black on #FF8000 bold").unwrap();
        assert_eq!(s, Style::fg(Black).on(Color::Rgb(255, 128, 0)).bold());
        assert_eq!(style_name(s), "black on #ff8000 bold");
        assert_eq!(parse_style("on 236"), Ok(Style::DEFAULT.on(Color::Indexed(236))));
        assert_eq!(parse_style("bold"), Ok(Style::DEFAULT.bold()));
        assert_eq!(parse_style("grey"), Ok(Style::fg(Gray)));
        for bad in ["", "on", "pink", "256", "#12345", "red bold blue", "red on", "#gg0000"] {
            assert!(parse_style(bad).is_err(), "{bad:?}");
        }
    }
}
