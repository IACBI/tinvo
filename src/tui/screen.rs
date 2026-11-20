//! Double-buffered character grid with minimal ANSI diff output.

use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Default,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    Gray,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    /// One of the 256 colours of xterm's palette.
    Indexed(u8),
    /// A 24-bit colour, for terminals that support one.
    Rgb(u8, u8, u8),
}

impl Color {
    /// The SGR code of a palette colour as a foreground; the extended ones
    /// have no single code.
    fn code(self) -> u8 {
        match self {
            Color::Default => 39,
            Color::Black => 30,
            Color::Red => 31,
            Color::Green => 32,
            Color::Yellow => 33,
            Color::Blue => 34,
            Color::Magenta => 35,
            Color::Cyan => 36,
            Color::White => 37,
            Color::Gray => 90,
            Color::BrightRed => 91,
            Color::BrightGreen => 92,
            Color::BrightYellow => 93,
            Color::BrightBlue => 94,
            Color::BrightMagenta => 95,
            Color::BrightCyan => 96,
            Color::BrightWhite => 97,
            Color::Indexed(_) | Color::Rgb(..) => 39,
        }
    }

    /// Appends the SGR parameters that set this colour, as a background if
    /// `bg`.
    fn write_sgr(self, out: &mut String, bg: bool) {
        let base = if bg { 48 } else { 38 };
        let _ = match self {
            Color::Indexed(n) => write!(out, "{base};5;{n}"),
            Color::Rgb(r, g, b) => write!(out, "{base};2;{r};{g};{b}"),
            // Background codes are the foreground codes + 10.
            c => write!(out, "{}", c.code() + if bg { 10 } else { 0 }),
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
}

impl Style {
    pub const DEFAULT: Style = Style { fg: Color::Default, bg: Color::Default, bold: false };

    pub const fn fg(fg: Color) -> Self {
        Style { fg, bg: Color::Default, bold: false }
    }

    pub const fn on(self, bg: Color) -> Self {
        Style { bg, ..self }
    }

    pub const fn bold(self) -> Self {
        Style { bold: true, ..self }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    ch: char,
    /// A combining mark drawn over `ch` (`'\0'`: none), as an accent
    /// written apart from its letter is.
    mark: char,
    style: Style,
}

impl Cell {
    const fn new(ch: char, style: Style) -> Self {
        Cell { ch, mark: '\0', style }
    }
}

const BLANK: Cell = Cell::new(' ', Style::DEFAULT);

/// The most columns and rows a screen has: a terminal may report up to
/// 65,535 of each, which would be gigabytes of cells. One larger is drawn
/// in its top left corner.
pub const MAX_WIDTH: u16 = 1000;
pub const MAX_HEIGHT: u16 = 500;

/// The right half of a wide character, which the cell before it shows.
const WIDE_TAIL: char = '\0';

/// Whether `ch` is a combining mark, drawn over the character before it.
/// Other zero-width characters (joiners, direction marks, variation
/// selectors) are left out: some would reorder the line in the terminal.
fn is_mark(ch: char) -> bool {
    matches!(
        ch,
        '\u{300}'..='\u{36F}'
            | '\u{483}'..='\u{489}'
            | '\u{591}'..='\u{5BD}'
            | '\u{610}'..='\u{61A}'
            | '\u{64B}'..='\u{65F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Columns `text` takes when drawn with [`Screen::put`].
pub fn text_width(text: &str) -> usize {
    text.chars().map(|c| if c.is_control() { 1 } else { char_width(c) }).sum()
}

/// The longest start of `text` that takes at most `cols` columns, with
/// the combining marks of its last character.
pub fn fit(text: &str, cols: usize) -> &str {
    let mut used = 0;
    for (i, c) in text.char_indices() {
        used += if c.is_control() { 1 } else { char_width(c) };
        if used > cols {
            return &text[..i];
        }
    }
    text
}

/// `text` cut to `cols` columns and padded with spaces to fill them.
pub fn pad(text: &str, cols: usize) -> String {
    let cut = fit(text, cols);
    let mut s = cut.to_string();
    s.extend(std::iter::repeat_n(' ', cols - text_width(cut)));
    s
}

/// The longest end of `text` that takes at most `cols` columns.
pub fn fit_end(text: &str, cols: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        used += if c.is_control() { 1 } else { char_width(c) };
        if used > cols {
            break;
        }
        start = i;
    }
    // A mark goes with the character before it, which did not fit.
    let rest = &text[start..];
    let marks = rest.chars().take_while(|&c| char_width(c) == 0).map(char::len_utf8).sum::<usize>();
    &rest[marks..]
}

/// Columns `ch` takes in a terminal: 2 for East Asian wide characters and
/// most emoji, 0 for combining marks and other zero-width characters, else
/// 1. The common ranges only, not the whole of Unicode's tables.
pub fn char_width(ch: char) -> usize {
    match ch {
        '\u{300}'..='\u{36F}'
        | '\u{483}'..='\u{489}'
        | '\u{591}'..='\u{5BD}'
        | '\u{610}'..='\u{61A}'
        | '\u{64B}'..='\u{65F}'
        | '\u{1AB0}'..='\u{1AFF}'
        | '\u{1DC0}'..='\u{1DFF}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{20D0}'..='\u{20FF}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FE20}'..='\u{FE2F}'
        | '\u{FEFF}'
        | '\u{E0100}'..='\u{E01EF}' => 0,
        '\u{1100}'..='\u{115F}'
        | '\u{2E80}'..='\u{303E}'
        | '\u{3041}'..='\u{33FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{A000}'..='\u{A4CF}'
        | '\u{AC00}'..='\u{D7A3}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FE30}'..='\u{FE4F}'
        | '\u{FF00}'..='\u{FF60}'
        | '\u{FFE0}'..='\u{FFE6}'
        | '\u{1F300}'..='\u{1F64F}'
        | '\u{1F900}'..='\u{1F9FF}'
        | '\u{20000}'..='\u{2FFFD}'
        | '\u{30000}'..='\u{3FFFD}' => 2,
        _ => 1,
    }
}

pub struct Screen {
    width: u16,
    height: u16,
    back: Vec<Cell>,
    front: Vec<Cell>,
    full_redraw: bool,
}

impl Screen {
    pub fn new(width: u16, height: u16) -> Self {
        let (width, height) = (width.min(MAX_WIDTH), height.min(MAX_HEIGHT));
        let n = usize::from(width) * usize::from(height);
        Self { width, height, back: vec![BLANK; n], front: vec![BLANK; n], full_redraw: true }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// Resizes and schedules a full redraw (no-op if the size is unchanged).
    pub fn resize(&mut self, width: u16, height: u16) {
        let (width, height) = (width.min(MAX_WIDTH), height.min(MAX_HEIGHT));
        if (width, height) != (self.width, self.height) {
            *self = Self::new(width, height);
        }
    }

    /// Forces the next flush to repaint every cell.
    pub fn invalidate(&mut self) {
        self.full_redraw = true;
    }

    pub fn clear(&mut self) {
        self.back.fill(BLANK);
    }

    /// Writes `text` at (x, y), clipped to the screen. Control characters are
    /// shown as `?`; a wide character takes two cells, and one that does not
    /// fit is shown as a space; a combining mark goes over the character
    /// before it (one per character), and other zero-width characters are
    /// left out. Returns the number of columns written.
    pub fn put(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        if y >= self.height {
            return 0;
        }
        let mut col = x;
        // The cell of the last character written, which a mark goes over.
        let mut last: Option<usize> = None;
        for ch in text.chars() {
            let ch = if ch.is_control() { '?' } else { ch };
            let width = char_width(ch);
            if width > 0 && col >= self.width {
                break;
            }
            let start = col;
            match width {
                0 => {
                    if let Some(i) = last.filter(|_| is_mark(ch)) {
                        let cell = &mut self.back[i];
                        if cell.mark == '\0' && cell.ch != ' ' {
                            cell.mark = ch;
                        }
                    }
                    continue;
                }
                2 if col + 1 < self.width => {
                    self.set(col, y, Cell::new(ch, style));
                    self.set(col + 1, y, Cell::new(WIDE_TAIL, style));
                    col += 2;
                }
                2 => {
                    self.set(col, y, Cell::new(' ', style));
                    col += 1;
                }
                _ => {
                    self.set(col, y, Cell::new(ch, style));
                    col += 1;
                }
            }
            last = Some(usize::from(y) * usize::from(self.width) + usize::from(start));
        }
        col - x.min(col)
    }

    /// Fills `len` cells starting at (x, y) with `ch` (a one-column
    /// character).
    pub fn fill(&mut self, x: u16, y: u16, len: u16, ch: char, style: Style) {
        for i in 0..len {
            let c = x.saturating_add(i);
            if c >= self.width || y >= self.height {
                break;
            }
            self.set(c, y, Cell::new(ch, style));
        }
    }

    /// Writes one cell. Writing over half of a wide character blanks its
    /// other half, which the terminal would otherwise leave broken.
    fn set(&mut self, x: u16, y: u16, cell: Cell) {
        let w = usize::from(self.width);
        let i = usize::from(y) * w + usize::from(x);
        let old = self.back[i];
        if old.ch == WIDE_TAIL && cell.ch != WIDE_TAIL && x > 0 {
            // With any mark it carried.
            self.back[i - 1] = Cell::new(' ', self.back[i - 1].style);
        }
        if usize::from(x) + 1 < w && self.back[i + 1].ch == WIDE_TAIL && char_width(old.ch) == 2 {
            self.back[i + 1].ch = ' ';
        }
        self.back[i] = cell;
    }

    /// Appends the escape sequences that bring the terminal from the last
    /// flushed frame to the current one.
    pub fn flush_into(&mut self, out: &mut String) {
        let w = usize::from(self.width);
        let mut cursor: Option<(usize, usize)> = None;
        let mut style: Option<Style> = None;
        if self.full_redraw {
            out.push_str("\x1b[0m\x1b[2J");
        }
        for y in 0..usize::from(self.height) {
            for x in 0..w {
                let i = y * w + x;
                let cell = self.back[i];
                if !self.full_redraw && cell == self.front[i] {
                    continue;
                }
                // Drawn with the character before it.
                if cell.ch == WIDE_TAIL {
                    continue;
                }
                if cursor != Some((x, y)) {
                    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                }
                if style != Some(cell.style) {
                    let s = cell.style;
                    out.push_str("\x1b[0;");
                    s.fg.write_sgr(out, false);
                    out.push(';');
                    s.bg.write_sgr(out, true);
                    if s.bold {
                        out.push_str(";1");
                    }
                    out.push('m');
                    style = Some(s);
                }
                out.push(cell.ch);
                if cell.mark != '\0' {
                    out.push(cell.mark);
                }
                cursor = Some((x + char_width(cell.ch).max(1), y));
            }
        }
        if style.is_some() {
            out.push_str("\x1b[0m");
        }
        self.front.copy_from_slice(&self.back);
        self.full_redraw = false;
    }

    /// The character at (x, y) in the frame being drawn (for tests).
    #[cfg(test)]
    pub fn char_at(&self, x: u16, y: u16) -> char {
        self.back[usize::from(y) * usize::from(self.width) + usize::from(x)].ch
    }

    /// The style at (x, y) in the frame being drawn (for tests).
    #[cfg(test)]
    pub fn style_at(&self, x: u16, y: u16) -> Style {
        self.back[usize::from(y) * usize::from(self.width) + usize::from(x)].style
    }

    /// One row of the frame being drawn, as text with its marks (for
    /// tests, the integration tests' too).
    pub fn row_text(&self, y: u16) -> String {
        let w = usize::from(self.width);
        let start = usize::from(y) * w;
        self.back[start..start + w]
            .iter()
            .filter(|c| c.ch != WIDE_TAIL)
            .flat_map(|c| [c.ch, c.mark])
            .filter(|&c| c != '\0')
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_huge_terminal_gets_a_screen_of_bounded_size() {
        let mut s = Screen::new(u16::MAX, u16::MAX);
        assert_eq!((s.width(), s.height()), (MAX_WIDTH, MAX_HEIGHT));
        assert_eq!(s.back.len(), usize::from(MAX_WIDTH) * usize::from(MAX_HEIGHT));
        s.full_redraw = false;
        // Past the limit both ways it is the same screen, not a new one.
        s.resize(u16::MAX, MAX_HEIGHT + 1);
        assert!(!s.full_redraw);
        s.resize(80, 24);
        assert_eq!((s.width(), s.height(), s.back.len()), (80, 24, 80 * 24));
    }

    #[test]
    fn put_clips_and_filters() {
        let mut s = Screen::new(5, 2);
        assert_eq!(s.put(3, 0, "abcdef", Style::DEFAULT), 2);
        assert_eq!(s.row_text(0), "   ab");
        assert_eq!(s.put(0, 5, "x", Style::DEFAULT), 0);
        s.put(0, 1, "a\tb", Style::DEFAULT);
        assert_eq!(s.row_text(1), "a?b  ");
    }

    #[test]
    fn wide_characters_take_two_cells_and_marks_none() {
        let mut s = Screen::new(6, 2);
        assert_eq!(s.put(0, 0, "钢琴a", Style::DEFAULT), 5);
        assert_eq!(s.row_text(0), "钢琴a ");
        assert_eq!(s.char_at(4, 0), 'a', "the next character lands where the terminal puts it");
        assert_eq!(s.put(0, 1, "e\u{301}x", Style::DEFAULT), 2, "a combining mark takes no column");
        assert_eq!((s.char_at(0, 1), s.back[6].mark, s.char_at(1, 1)), ('e', '\u{301}', 'x'));
        // A wide character that does not fit is a space.
        assert_eq!(s.put(5, 0, "琴", Style::DEFAULT), 1);
        assert_eq!(s.row_text(0), "钢琴a ");
        let mut out = String::new();
        s.flush_into(&mut out);
        // Writing over half of a wide character blanks the other half.
        s.put(1, 0, "b", Style::DEFAULT);
        assert_eq!(s.row_text(0), " b琴a ");
        out.clear();
        s.flush_into(&mut out);
        assert_eq!(out, "\x1b[1;1H\x1b[0;39;49m b\x1b[0m");
        s.put(3, 0, "c", Style::DEFAULT);
        assert_eq!(s.row_text(0), " b ca ");
        out.clear();
        s.flush_into(&mut out);
        assert_eq!(out, "\x1b[1;3H\x1b[0;39;49m c\x1b[0m");
        // After a wide character the cursor is two columns on.
        s.put(0, 0, "钢x", Style::DEFAULT);
        out.clear();
        s.flush_into(&mut out);
        assert_eq!(out, "\x1b[1;1H\x1b[0;39;49m钢x\x1b[0m");
    }

    #[test]
    fn marks_are_drawn_over_their_letter() {
        let mut s = Screen::new(6, 1);
        let mut out = String::new();
        s.flush_into(&mut out);
        // A second mark, a mark with no letter before it, and a joiner are
        // left out.
        s.put(0, 0, "\u{301}Cafe\u{301}\u{302}\u{200D}!", Style::DEFAULT);
        out.clear();
        s.flush_into(&mut out);
        assert_eq!(out, "\x1b[1;1H\x1b[0;39;49mCafe\u{301}!\x1b[0m");
        // Writing over the letter takes its mark away too.
        s.put(3, 0, "x", Style::DEFAULT);
        out.clear();
        s.flush_into(&mut out);
        assert_eq!(out, "\x1b[1;4H\x1b[0;39;49mx\x1b[0m");
        // A mark past the right edge stays with the last letter that fit.
        let mut s = Screen::new(2, 1);
        s.put(0, 0, "ab\u{301}c", Style::DEFAULT);
        assert_eq!(s.back[1].mark, '\u{301}');
        // Writing over the second half of a wide letter blanks it, mark and all.
        let mut s = Screen::new(4, 1);
        s.put(0, 0, "钢\u{301}", Style::DEFAULT);
        s.put(1, 0, "x", Style::DEFAULT);
        assert_eq!(s.row_text(0), " x  ");
    }

    #[test]
    fn text_is_cut_and_padded_by_columns() {
        assert_eq!(text_width("a钢\u{301}b"), 4);
        assert_eq!(fit("钢琴ab", 3), "钢");
        assert_eq!(fit("e\u{301}x", 1), "e\u{301}");
        assert_eq!(fit("ab", 5), "ab");
        assert_eq!(pad("钢琴ab", 3), "钢 ");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(fit_end("ab钢琴", 5), "b钢琴");
        assert_eq!(fit_end("ab钢琴", 3), "琴");
        assert_eq!(fit_end("ae\u{301}", 1), "e\u{301}");
        assert_eq!(fit_end("钢\u{301}", 1), "");
        assert_eq!(fit_end("ae\u{301}", 2), "ae\u{301}");
    }

    #[test]
    fn diff_only_emits_changes() {
        let mut s = Screen::new(10, 3);
        s.put(0, 0, "hello", Style::DEFAULT);
        let mut first = String::new();
        s.flush_into(&mut first);
        assert!(first.contains("\x1b[2J") && first.contains("hello"));

        let mut none = String::new();
        s.flush_into(&mut none);
        assert!(none.is_empty(), "{none:?}");

        s.put(1, 0, "a", Style::fg(Color::Red));
        let mut one = String::new();
        s.flush_into(&mut one);
        assert_eq!(one, "\x1b[1;2H\x1b[0;31;49ma\x1b[0m");
    }

    #[test]
    fn extended_colours_use_their_own_sequences() {
        let mut s = Screen::new(4, 1);
        let mut out = String::new();
        s.flush_into(&mut out);
        s.put(0, 0, "a", Style::fg(Color::Indexed(208)).on(Color::Rgb(1, 2, 3)).bold());
        s.put(1, 0, "b", Style::fg(Color::BrightWhite).on(Color::Indexed(0)));
        out.clear();
        s.flush_into(&mut out);
        assert_eq!(out, "\x1b[1;1H\x1b[0;38;5;208;48;2;1;2;3;1ma\x1b[0;97;48;5;0mb\x1b[0m");
        assert_eq!(s.style_at(1, 0).bg, Color::Indexed(0));
    }

    #[test]
    fn resize_forces_full_redraw() {
        let mut s = Screen::new(4, 2);
        let mut out = String::new();
        s.flush_into(&mut out);
        s.resize(6, 2);
        out.clear();
        s.flush_into(&mut out);
        assert!(out.contains("\x1b[2J"));
        assert_eq!(s.width(), 6);
    }
}
