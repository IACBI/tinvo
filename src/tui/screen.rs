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
}

impl Color {
    fn fg(self) -> u8 {
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
        }
    }

    fn bg(self) -> u8 {
        // Background codes are the foreground codes + 10.
        self.fg() + 10
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
    style: Style,
}

const BLANK: Cell = Cell { ch: ' ', style: Style::DEFAULT };

pub struct Screen {
    width: u16,
    height: u16,
    back: Vec<Cell>,
    front: Vec<Cell>,
    full_redraw: bool,
}

impl Screen {
    pub fn new(width: u16, height: u16) -> Self {
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
    /// shown as `?`. Returns the number of columns written.
    pub fn put(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        if y >= self.height {
            return 0;
        }
        let mut col = x;
        for ch in text.chars() {
            if col >= self.width {
                break;
            }
            let ch = if ch.is_control() { '?' } else { ch };
            self.back[usize::from(y) * usize::from(self.width) + usize::from(col)] = Cell { ch, style };
            col += 1;
        }
        col - x.min(col)
    }

    /// Fills `len` cells starting at (x, y) with `ch`.
    pub fn fill(&mut self, x: u16, y: u16, len: u16, ch: char, style: Style) {
        for i in 0..len {
            let c = x.saturating_add(i);
            if c >= self.width || y >= self.height {
                break;
            }
            self.back[usize::from(y) * usize::from(self.width) + usize::from(c)] = Cell { ch, style };
        }
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
                if cursor != Some((x, y)) {
                    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                }
                if style != Some(cell.style) {
                    let s = cell.style;
                    let _ = write!(out, "\x1b[0;{};{}", s.fg.fg(), s.bg.bg());
                    if s.bold {
                        out.push_str(";1");
                    }
                    out.push('m');
                    style = Some(s);
                }
                out.push(cell.ch);
                cursor = Some((x + 1, y));
            }
        }
        if style.is_some() {
            out.push_str("\x1b[0m");
        }
        self.front.copy_from_slice(&self.back);
        self.full_redraw = false;
    }

    /// The character at (x, y) in the frame being drawn (for tests).
    pub fn char_at(&self, x: u16, y: u16) -> char {
        self.back[usize::from(y) * usize::from(self.width) + usize::from(x)].ch
    }

    /// One row of the frame being drawn, as text (for tests).
    pub fn row_text(&self, y: u16) -> String {
        let w = usize::from(self.width);
        let start = usize::from(y) * w;
        self.back[start..start + w].iter().map(|c| c.ch).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
