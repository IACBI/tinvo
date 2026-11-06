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

/// Result of decoding the start of an input byte buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoded {
    /// A key, and how many bytes it used.
    Key(Key, usize),
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

const MAX_SEQ: usize = 16;

fn csi(b: &[u8]) -> Decoded {
    // ESC [ params final, where final is in 0x40..=0x7e.
    let Some(end) = b.iter().skip(2).position(|c| (0x40..=0x7e).contains(c)).map(|p| p + 2) else {
        return if b.len() >= MAX_SEQ { Decoded::Skip(b.len()) } else { Decoded::Incomplete };
    };
    let params = std::str::from_utf8(&b[2..end]).unwrap_or("");
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
