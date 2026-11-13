//! Key representation and decoding of VT/xterm input byte sequences.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Ctrl + letter, lower-case.
    Ctrl(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Backspace,
    Tab,
    BackTab,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Press(Button),
    /// The pointer moved to another cell with a button held.
    Drag(Button),
    /// A button was let go (terminals do not all say which).
    Release,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

/// A mouse event at a cell, counted from 0 at the top left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    pub x: u16,
    pub y: u16,
}

/// Result of decoding the start of an input byte buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoded {
    /// A key, and how many bytes it used.
    Key(Key, usize),
    /// A mouse event, and how many bytes it used.
    Mouse(Mouse, usize),
    /// The buffer holds the beginning of a sequence; more bytes are needed.
    Incomplete,
    /// Unrecognised bytes to discard.
    Skip(usize),
}

/// Decodes one key from the front of `b` (which must not be empty).
pub fn decode(b: &[u8]) -> Decoded {
    match b[0] {
        0x1b => match b.get(1) {
            None => Decoded::Incomplete,
            Some(b'[') => csi(b),
            Some(b'O') => ss3(b),
            // ESC followed by something else: a lone Escape press.
            Some(_) => Decoded::Key(Key::Esc, 1),
        },
        b'\r' | b'\n' => Decoded::Key(Key::Enter, 1),
        b'\t' => Decoded::Key(Key::Tab, 1),
        0x7f | 0x08 => Decoded::Key(Key::Backspace, 1),
        c @ 1..=26 => Decoded::Key(Key::Ctrl(char::from(b'a' + c - 1)), 1),
        c if c < 0x20 => Decoded::Skip(1),
        c => {
            let width = match c {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Decoded::Skip(1),
            };
            if b.len() < width {
                return Decoded::Incomplete;
            }
            match std::str::from_utf8(&b[..width]).ok().and_then(|s| s.chars().next()) {
                Some(ch) => Decoded::Key(Key::Char(ch), width),
                None => Decoded::Skip(1),
            }
        }
    }
}

/// Longer unfinished sequences are dropped. An SGR mouse report at column
/// and row 9999 takes 16 bytes.
const MAX_SEQ: usize = 32;

/// The mouse event for an xterm button code (`cb`, without the offset of
/// 32 the legacy encoding adds) at 1-based `x`, `y`; `release` for SGR's
/// final `m`.
fn mouse(cb: u32, x: u32, y: u32, release: bool) -> Option<Mouse> {
    // 128 and up are the extra buttons (back, forward), which TINVO ignores.
    if cb >= 128 {
        return None;
    }
    let button = match cb & 3 {
        0 => Some(Button::Left),
        1 => Some(Button::Middle),
        2 => Some(Button::Right),
        _ => None,
    };
    // Bits 4, 8 and 16 are Shift, Meta and Ctrl; 32 is motion, 64 the wheel.
    let kind = if cb & 64 != 0 {
        match cb & 3 {
            0 => MouseKind::ScrollUp,
            1 => MouseKind::ScrollDown,
            2 => MouseKind::ScrollLeft,
            _ => MouseKind::ScrollRight,
        }
    } else if release || button.is_none() && cb & 32 == 0 {
        MouseKind::Release
    } else if cb & 32 != 0 {
        // Motion with no button held is only reported in mode 1003, which
        // TINVO does not ask for.
        MouseKind::Drag(button?)
    } else {
        MouseKind::Press(button?)
    };
    let cell = |v: u32| u16::try_from(v.checked_sub(1)?).ok();
    Some(Mouse { kind, x: cell(x)?, y: cell(y)? })
}

fn csi(b: &[u8]) -> Decoded {
    // The legacy mouse encoding: ESC [ M and three bytes, each 32 more than
    // the button code and the 1-based column and row. Its bytes would
    // otherwise be read as keys.
    if b.get(2) == Some(&b'M') {
        if b.len() < 6 {
            return Decoded::Incomplete;
        }
        let v = |i: usize| u32::from(b[i]).wrapping_sub(32);
        return match mouse(v(3), v(4), v(5), false) {
            Some(m) => Decoded::Mouse(m, 6),
            None => Decoded::Skip(6),
        };
    }
    // ESC [ params final, where final is in 0x40..=0x7e.
    let Some(end) = b.iter().skip(2).position(|c| (0x40..=0x7e).contains(c)).map(|p| p + 2) else {
        return if b.len() >= MAX_SEQ { Decoded::Skip(b.len()) } else { Decoded::Incomplete };
    };
    let params = std::str::from_utf8(&b[2..end]).unwrap_or("");
    // SGR mouse encoding: ESC [ < button ; column ; row, then M or m.
    if let (Some(sgr), b'M' | b'm') = (params.strip_prefix('<'), b[end]) {
        let n: Vec<u32> = sgr.split(';').map_while(|p| p.parse().ok()).collect();
        return match n[..] {
            [cb, x, y] => match mouse(cb, x, y, b[end] == b'm') {
                Some(m) => Decoded::Mouse(m, end + 1),
                None => Decoded::Skip(end + 1),
            },
            _ => Decoded::Skip(end + 1),
        };
    }
    let first: u32 = params.split(';').next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let key = match b[end] {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(Key::Right),
        b'D' => Some(Key::Left),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        b'Z' => Some(Key::BackTab),
        b'~' => match first {
            1 | 7 => Some(Key::Home),
            4 | 8 => Some(Key::End),
            3 => Some(Key::Delete),
            5 => Some(Key::PageUp),
            6 => Some(Key::PageDown),
            11..=15 => Some(Key::F((first - 10) as u8)),
            17..=21 => Some(Key::F((first - 11) as u8)),
            23 | 24 => Some(Key::F((first - 12) as u8)),
            _ => None,
        },
        _ => None,
    };
    match key {
        Some(k) => Decoded::Key(k, end + 1),
        None => Decoded::Skip(end + 1),
    }
}

fn ss3(b: &[u8]) -> Decoded {
    let Some(&c) = b.get(2) else { return Decoded::Incomplete };
    let key = match c {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'P'..=b'S' => Key::F(c - b'P' + 1),
        _ => return Decoded::Skip(3),
    };
    Decoded::Key(key, 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: &[u8]) -> Key {
        match decode(b) {
            Decoded::Key(k, n) => {
                assert_eq!(n, b.len(), "{b:?} consumed {n}");
                k
            }
            other => panic!("{b:?}: {other:?}"),
        }
    }

    #[test]
    fn plain_and_control_keys() {
        assert_eq!(key(b"a"), Key::Char('a'));
        assert_eq!(key(b" "), Key::Char(' '));
        assert_eq!(key(b"\r"), Key::Enter);
        assert_eq!(key(b"\t"), Key::Tab);
        assert_eq!(key(b"\x7f"), Key::Backspace);
        assert_eq!(key(b"\x13"), Key::Ctrl('s'));
        assert_eq!(key(b"\x03"), Key::Ctrl('c'));
        assert_eq!(key("ü".as_bytes()), Key::Char('ü'));
        assert_eq!(key("♪".as_bytes()), Key::Char('♪'));
    }

    #[test]
    fn escape_sequences() {
        assert_eq!(key(b"\x1b[A"), Key::Up);
        assert_eq!(key(b"\x1b[D"), Key::Left);
        assert_eq!(key(b"\x1bOB"), Key::Down);
        assert_eq!(key(b"\x1b[3~"), Key::Delete);
        assert_eq!(key(b"\x1b[5~"), Key::PageUp);
        assert_eq!(key(b"\x1b[Z"), Key::BackTab);
        assert_eq!(key(b"\x1bOP"), Key::F(1));
        assert_eq!(key(b"\x1b[15~"), Key::F(5));
        assert_eq!(key(b"\x1b[24~"), Key::F(12));
        assert_eq!(key(b"\x1b[1;5C"), Key::Right);
    }

    fn mouse_event(b: &[u8]) -> Mouse {
        match decode(b) {
            Decoded::Mouse(m, n) => {
                assert_eq!(n, b.len(), "{b:?} consumed {n}");
                m
            }
            other => panic!("{b:?}: {other:?}"),
        }
    }

    #[test]
    fn sgr_mouse_reports() {
        use MouseKind::*;
        let at = |kind, x, y| Mouse { kind, x, y };
        assert_eq!(mouse_event(b"\x1b[<0;1;1M"), at(Press(Button::Left), 0, 0));
        assert_eq!(mouse_event(b"\x1b[<2;40;12M"), at(Press(Button::Right), 39, 11));
        assert_eq!(mouse_event(b"\x1b[<1;5;5M"), at(Press(Button::Middle), 4, 4));
        assert_eq!(mouse_event(b"\x1b[<0;40;12m"), at(Release, 39, 11));
        assert_eq!(mouse_event(b"\x1b[<32;41;12M"), at(Drag(Button::Left), 40, 11));
        assert_eq!(mouse_event(b"\x1b[<64;3;4M"), at(ScrollUp, 2, 3));
        assert_eq!(mouse_event(b"\x1b[<65;3;4M"), at(ScrollDown, 2, 3));
        assert_eq!(mouse_event(b"\x1b[<66;3;4M"), at(ScrollLeft, 2, 3));
        assert_eq!(mouse_event(b"\x1b[<67;3;4M"), at(ScrollRight, 2, 3));
        // Shift (4) and Ctrl (16) do not change the event.
        assert_eq!(mouse_event(b"\x1b[<20;1;1M"), at(Press(Button::Left), 0, 0));
        assert_eq!(mouse_event(b"\x1b[<0;9999;9999M"), at(Press(Button::Left), 9998, 9998));
        // Followed by more input, a report consumes only itself.
        assert_eq!(decode(b"\x1b[<0;2;3Mq"), Decoded::Mouse(at(Press(Button::Left), 1, 2), 9));
        assert_eq!(decode(b"\x1b[<0;2"), Decoded::Incomplete);
        // Motion without a button, extra buttons, and malformed reports are dropped.
        for bad in [&b"\x1b[<35;2;2M"[..], b"\x1b[<128;2;2M", b"\x1b[<0;0;2M", b"\x1b[<0;2M", b"\x1b[<0;2;2;2M"] {
            assert_eq!(decode(bad), Decoded::Skip(bad.len()), "{bad:?}");
        }
    }

    #[test]
    fn legacy_mouse_reports() {
        use MouseKind::*;
        // Button, column and row are each sent as a byte 32 more than the
        // code and the 1-based position.
        assert_eq!(mouse_event(b"\x1b[M !\""), Mouse { kind: Press(Button::Left), x: 0, y: 1 });
        assert_eq!(mouse_event(b"\x1b[M#!!"), Mouse { kind: Release, x: 0, y: 0 });
        assert_eq!(mouse_event(b"\x1b[M`+,"), Mouse { kind: ScrollUp, x: 10, y: 11 });
        assert_eq!(decode(b"\x1b[M !"), Decoded::Incomplete);
        // Its bytes are never read as keys, even when they are printable.
        assert_eq!(decode(b"\x1b[M !! "), Decoded::Mouse(Mouse { kind: Press(Button::Left), x: 0, y: 0 }, 6));
    }

    #[test]
    fn partial_and_garbage_input() {
        assert_eq!(decode(b"\x1b"), Decoded::Incomplete);
        assert_eq!(decode(b"\x1b["), Decoded::Incomplete);
        assert_eq!(decode(b"\x1b[1;5"), Decoded::Incomplete);
        assert_eq!(decode(&"é".as_bytes()[..1]), Decoded::Incomplete);
        assert_eq!(decode(b"\x1bx"), Decoded::Key(Key::Esc, 1));
        assert_eq!(decode(b"\x1b[99~"), Decoded::Skip(5));
        assert_eq!(decode(b"\xff"), Decoded::Skip(1));
        assert_eq!(decode(b"\x1d"), Decoded::Skip(1));
        // Sequences followed by more input consume only themselves.
        assert_eq!(decode(b"\x1b[Bxyz"), Decoded::Key(Key::Down, 3));
    }
}
