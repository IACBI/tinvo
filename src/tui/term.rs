//! Raw-mode terminal access through native console APIs.
//!
//! Windows uses the Console API (`ReadConsoleInputW` for exact key codes
//! and mouse records, VT processing for output). Linux and macOS use
//! termios and `poll`, and ask the terminal for SGR mouse reports.

use super::input::{Decoded, Key, Mouse, decode};
use std::io::{self, Write};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Mouse(Mouse),
    /// Text pasted in one piece (bracketed paste, on Linux and macOS), with
    /// its line breaks and tabs as `\n` and `\t`, and whether it was cut
    /// at [`MAX_PASTE`] bytes.
    Paste {
        text: String,
        cut: bool,
    },
    Resize,
}

/// The most bytes of text a paste brings in; the rest is dropped. Twice
/// what a prompt takes, so a prompt sees that a long paste was cut.
pub const MAX_PASTE: usize = 8192;

/// The end of a bracketed paste.
#[cfg_attr(windows, allow(dead_code))]
const PASTE_END: &[u8] = b"\x1b[201~";

/// What [`Decoder::next`] found.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))]
enum Next {
    Event(Event),
    /// The bytes so far start a sequence; the rest is still to come.
    More,
    /// Nothing left to decode.
    Empty,
}

/// Turns the VT input bytes Linux and macOS terminals send into events.
#[derive(Default)]
#[cfg_attr(windows, allow(dead_code))]
struct Decoder {
    pending: Vec<u8>,
    /// The text of a bracketed paste under way, and whether it was cut.
    paste: Option<(String, bool)>,
}

#[cfg_attr(windows, allow(dead_code))]
impl Decoder {
    /// The next whole event in the bytes read so far. Keys inside a paste
    /// become its text instead: a pasted line break must not submit a
    /// prompt, nor pasted letters run commands.
    fn next(&mut self) -> Next {
        loop {
            if self.paste.is_some() {
                return self.next_in_paste();
            }
            if self.pending.is_empty() {
                return Next::Empty;
            }
            let (event, n) = match decode(&self.pending) {
                Decoded::Incomplete => return Next::More,
                Decoded::Key(k, n) => (Some(Event::Key(k)), n),
                Decoded::Mouse(m, n) => (Some(Event::Mouse(m)), n),
                Decoded::PasteStart(n) => {
                    self.paste = Some((String::new(), false));
                    (None, n)
                }
                // An end with no start.
                Decoded::PasteEnd(n) | Decoded::Skip(n) => (None, n),
            };
            self.pending.drain(..n);
            if let Some(e) = event {
                return Next::Event(e);
            }
        }
    }

    /// Inside a paste, the text up to its end marker. The marker is looked
    /// for first, so a sequence cut short in the text cannot take in its
    /// bytes and leave the paste open.
    fn next_in_paste(&mut self) -> Next {
        let end = find(&self.pending, PASTE_END);
        // What may be the start of the marker waits for the rest.
        let body = end.unwrap_or(self.pending.len() - marker_start(&self.pending));
        let Some((text, cut)) = &mut self.paste else { return Next::Empty };
        let mut at = 0;
        while at < body {
            // ESC before a character is text here, not Alt with a key: only
            // the ESC is dropped.
            let rest = &self.pending[at..body];
            if rest[0] == 0x1b && rest.get(1).is_some_and(|&b| b != b'[' && b != b'O') {
                at += 1;
                continue;
            }
            let n = match decode(rest) {
                Decoded::Incomplete => break,
                Decoded::Key(k, n) => {
                    let ch = match k {
                        Key::Char(c) => Some(c),
                        Key::Enter => Some('\n'),
                        Key::Tab => Some('\t'),
                        _ => None,
                    };
                    if let Some(c) = ch {
                        // Cut at the first character that does not fit, so
                        // what is kept is the start of the text.
                        *cut |= text.len() + c.len_utf8() > MAX_PASTE;
                        if !*cut {
                            text.push(c);
                        }
                    }
                    n
                }
                Decoded::Mouse(_, n) | Decoded::PasteStart(n) | Decoded::PasteEnd(n) | Decoded::Skip(n) => n,
            };
            at += n;
        }
        if let Some(i) = end {
            // A sequence left unfinished in the text goes with it.
            self.pending.drain(..i + PASTE_END.len());
            return match self.paste.take() {
                Some((text, cut)) => Next::Event(Event::Paste { text, cut }),
                None => Next::Empty,
            };
        }
        self.pending.drain(..at);
        if self.pending.is_empty() { Next::Empty } else { Next::More }
    }

    /// The rest of a sequence did not come in time: a lone ESC is the
    /// Escape key; anything else is dropped whole, so none of a partial
    /// sequence's bytes is taken for a key. In a paste, what may be the
    /// start of its end marker is kept: the paste ends only with it.
    fn timed_out(&mut self) -> Option<Event> {
        if self.paste.is_some() {
            let keep = marker_start(&self.pending);
            self.pending.drain(..self.pending.len() - keep);
            return None;
        }
        let lone_esc = self.pending == [0x1b];
        self.pending.clear();
        lone_esc.then_some(Event::Key(Key::Esc))
    }
}

/// Where `needle` first starts in `bytes`.
#[cfg_attr(windows, allow(dead_code))]
fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|w| w == needle)
}

/// The length of the longest end of `bytes` that starts the paste's end
/// marker without being all of it.
#[cfg_attr(windows, allow(dead_code))]
fn marker_start(bytes: &[u8]) -> usize {
    (1..PASTE_END.len()).rev().find(|&k| bytes.ends_with(&PASTE_END[..k])).unwrap_or(0)
}

/// Whether the process was asked to end (SIGTERM, or SIGHUP when its
/// terminal went away) while the terminal was taken over. Always false on
/// Windows.
pub fn end_requested() -> bool {
    sys::end_requested()
}

/// Enter alternate screen, hide cursor, disable line wrap.
const ENTER_SEQ: &str = "\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[2J";
/// Report button presses and releases (1000), motion while a button is held
/// (1002), in the SGR encoding (1006), which has no limit on the column.
/// Windows consoles report the mouse through input records instead.
#[cfg(not(windows))]
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
/// Undo `MOUSE_ON`.
#[cfg(not(windows))]
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";
/// Mark pasted text (2004). Not on Windows, where the console delivers the
/// markers as typed keys.
#[cfg(not(windows))]
const PASTE_ON: &str = "\x1b[?2004h";
/// Undo `ENTER_SEQ`, `MOUSE_ON` and `PASTE_ON`.
const LEAVE_SEQ: &str = "\x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l";

/// A terminal in raw mode. Dropping it restores the original state.
pub struct Terminal {
    sys: sys::RawMode,
}

impl Terminal {
    /// Takes over the terminal; with `mouse`, it reports the mouse too.
    pub fn enter(mouse: bool) -> io::Result<Self> {
        let sys = sys::RawMode::enable(mouse)?;
        let mut t = Self { sys };
        t.write(ENTER_SEQ)?;
        #[cfg(not(windows))]
        t.write(PASTE_ON)?;
        #[cfg(not(windows))]
        if mouse {
            t.write(MOUSE_ON)?;
        }
        Ok(t)
    }

    /// Terminal size in columns and rows.
    pub fn size(&self) -> (u16, u16) {
        self.sys.size()
    }

    /// Starts or stops reporting the mouse.
    pub fn set_mouse(&mut self, on: bool) -> io::Result<()> {
        self.sys.set_mouse(on)?;
        #[cfg(not(windows))]
        self.write(if on { MOUSE_ON } else { MOUSE_OFF })?;
        Ok(())
    }

    /// Waits up to `timeout` for the next event.
    pub fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        self.sys.poll(timeout)
    }

    pub fn write(&mut self, s: &str) -> io::Result<()> {
        let mut out = io::stdout().lock();
        out.write_all(s.as_bytes())?;
        out.flush()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.write(LEAVE_SEQ);
        self.sys.restore();
    }
}

/// Makes panics readable: leaves the alternate screen before the default hook
/// prints the message. (Console modes are restored when `Terminal` drops.)
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = io::stdout();
        let _ = out.write_all(LEAVE_SEQ.as_bytes());
        let _ = out.flush();
        default(info);
    }));
}

#[cfg(windows)]
mod sys {
    use super::super::input::{Button, Mouse, MouseKind};
    use super::{Event, Key};
    use std::ffi::c_void;
    use std::io;
    use std::time::{Duration, Instant};

    type Handle = *mut c_void;

    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_PROCESSED_OUTPUT: u32 = 0x1;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;
    const ENABLE_WINDOW_INPUT: u32 = 0x8;
    const ENABLE_MOUSE_INPUT: u32 = 0x10;
    const ENABLE_EXTENDED_FLAGS: u32 = 0x80;

    /// The console input mode: without `ENABLE_QUICK_EDIT_MODE` (cleared by
    /// giving `ENABLE_EXTENDED_FLAGS` without it), so clicks do not freeze
    /// output, and with mouse records when `mouse` is on.
    fn input_flags(mouse: bool) -> u32 {
        ENABLE_WINDOW_INPUT | ENABLE_EXTENDED_FLAGS | if mouse { ENABLE_MOUSE_INPUT } else { 0 }
    }
    const KEY_EVENT: u16 = 0x1;
    const MOUSE_EVENT: u16 = 0x2;
    const WINDOW_BUFFER_SIZE_EVENT: u16 = 0x4;
    const FROM_LEFT_1ST_BUTTON_PRESSED: u32 = 0x1;
    const RIGHTMOST_BUTTON_PRESSED: u32 = 0x2;
    const FROM_LEFT_2ND_BUTTON_PRESSED: u32 = 0x4;
    const MOUSE_MOVED: u32 = 0x1;
    const MOUSE_WHEELED: u32 = 0x4;
    const MOUSE_HWHEELED: u32 = 0x8;
    const WAIT_OBJECT_0: u32 = 0;
    const SHIFT_PRESSED: u32 = 0x10;
    const CTRL_PRESSED: u32 = 0x4 | 0x8;
    const ALT_PRESSED: u32 = 0x1 | 0x2;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ScreenBufferInfo {
        size: Coord,
        cursor: Coord,
        attributes: u16,
        window: SmallRect,
        max_window: Coord,
    }

    /// INPUT_RECORD: a 2-byte tag, then a 4-byte-aligned 16-byte union.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct InputRecord {
        event_type: u16,
        event: [u32; 4],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyEventRecord {
        key_down: i32,
        repeat_count: u16,
        virtual_key_code: u16,
        virtual_scan_code: u16,
        unicode_char: u16,
        control_key_state: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MouseEventRecord {
        position: Coord,
        button_state: u32,
        control_key_state: u32,
        event_flags: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
        fn GetConsoleScreenBufferInfo(handle: Handle, info: *mut ScreenBufferInfo) -> i32;
        fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
        fn ReadConsoleInputW(handle: Handle, buffer: *mut InputRecord, length: u32, read: *mut u32) -> i32;
    }

    pub struct RawMode {
        input: Handle,
        output: Handle,
        input_mode: u32,
        output_mode: u32,
        /// The buttons held at the last mouse record, which reports only
        /// which are down, not which changed.
        buttons: u32,
        /// A key record's repeats still to report, and the key.
        repeat: Option<(Key, u16)>,
        /// The first half of a character outside the Basic Multilingual
        /// Plane, which the console sends as two records.
        high: Option<u16>,
    }

    /// The most repeats one key record reports.
    const MAX_REPEAT: u16 = 64;

    /// The most records standing for nothing read past a poll's deadline.
    const MAX_SKIPPED: u32 = 256;

    pub fn end_requested() -> bool {
        false
    }

    impl RawMode {
        pub fn enable(mouse: bool) -> io::Result<Self> {
            // SAFETY: plain Win32 console calls with valid out-pointers.
            unsafe {
                let input = GetStdHandle(STD_INPUT_HANDLE);
                let output = GetStdHandle(STD_OUTPUT_HANDLE);
                let (mut input_mode, mut output_mode) = (0u32, 0u32);
                if GetConsoleMode(input, &mut input_mode) == 0 || GetConsoleMode(output, &mut output_mode) == 0 {
                    return Err(io::Error::other("not running in a console (stdin/stdout redirected?)"));
                }
                // No line buffering, echo or Ctrl+C processing; report resizes;
                // disable QuickEdit so clicks do not freeze output.
                if SetConsoleMode(input, input_flags(mouse)) == 0 {
                    return Err(io::Error::last_os_error());
                }
                if SetConsoleMode(output, ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                    let err = io::Error::last_os_error();
                    SetConsoleMode(input, input_mode);
                    return Err(io::Error::other(format!("console lacks VT support: {err}")));
                }
                Ok(Self { input, output, input_mode, output_mode, buttons: 0, repeat: None, high: None })
            }
        }

        pub fn set_mouse(&mut self, on: bool) -> io::Result<()> {
            // SAFETY: a console handle from `enable`.
            if unsafe { SetConsoleMode(self.input, input_flags(on)) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn restore(&mut self) {
            // SAFETY: restoring modes captured in `enable`.
            unsafe {
                SetConsoleMode(self.input, self.input_mode);
                SetConsoleMode(self.output, self.output_mode);
            }
        }

        /// The visible part of the screen buffer.
        fn window(&self) -> Option<SmallRect> {
            let mut info = ScreenBufferInfo::default();
            // SAFETY: valid handle and out-pointer.
            (unsafe { GetConsoleScreenBufferInfo(self.output, &mut info) } != 0).then_some(info.window)
        }

        pub fn size(&self) -> (u16, u16) {
            let Some(window) = self.window() else { return (80, 24) };
            let w = (window.right - window.left + 1).max(1) as u16;
            let h = (window.bottom - window.top + 1).max(1) as u16;
            (w, h)
        }

        /// The event a mouse record stands for, if TINVO uses it.
        fn mouse(&mut self, m: &MouseEventRecord) -> Option<Mouse> {
            // Positions are in the screen buffer, which can be larger than
            // the window when the console keeps scrollback.
            let origin = self.window().map_or((0, 0), |w| (w.left, w.top));
            mouse_event(m, origin, &mut self.buttons)
        }
        pub fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
            if let Some((key, n)) = self.repeat.take() {
                self.repeat = (n > 1).then_some((key, n - 1));
                return Ok(Some(Event::Key(key)));
            }
            let deadline = Instant::now() + timeout;
            let mut skipped = 0;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let ms = left.as_millis().min(u128::from(u32::MAX - 1)) as u32;
                // SAFETY: valid console input handle.
                if unsafe { WaitForSingleObject(self.input, ms) } != WAIT_OBJECT_0 {
                    return Ok(None);
                }
                let mut rec = InputRecord::default();
                let mut read = 0u32;
                // SAFETY: room for exactly one record.
                if unsafe { ReadConsoleInputW(self.input, &mut rec, 1, &mut read) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                if read == 1 {
                    match rec.event_type {
                        KEY_EVENT => {
                            // SAFETY: the union holds a KEY_EVENT_RECORD for this tag,
                            // and both are 16 bytes.
                            let k: KeyEventRecord = unsafe { std::mem::transmute(rec.event) };
                            if k.key_down != 0 {
                                if let Some(key) = map_key(&k, &mut self.high) {
                                    let more = k.repeat_count.saturating_sub(1).min(MAX_REPEAT - 1);
                                    self.repeat = (more > 0).then_some((key, more));
                                    return Ok(Some(Event::Key(key)));
                                }
                            }
                        }
                        MOUSE_EVENT => {
                            // SAFETY: the union holds a MOUSE_EVENT_RECORD for this
                            // tag, and both are 16 bytes.
                            let m: MouseEventRecord = unsafe { std::mem::transmute(rec.event) };
                            if let Some(m) = self.mouse(&m) {
                                return Ok(Some(Event::Mouse(m)));
                            }
                        }
                        WINDOW_BUFFER_SIZE_EVENT => return Ok(Some(Event::Resize)),
                        _ => {}
                    }
                }
                // Records that stand for nothing (releases, moves) are read
                // past the deadline while more wait, so a zero timeout
                // still finds an event queued behind them; a bounded
                // number, so a flood of them cannot hold the loop.
                if Instant::now() >= deadline {
                    skipped += 1;
                    if skipped > MAX_SKIPPED {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// The event mouse record `m` stands for, with the window's top left at
    /// `(left, top)` in the buffer; `buttons` are those held before it.
    fn mouse_event(m: &MouseEventRecord, (left, top): (i16, i16), buttons: &mut u32) -> Option<Mouse> {
        let x = u16::try_from(m.position.x - left).ok()?;
        let y = u16::try_from(m.position.y - top).ok()?;
        // The wheel's distance is the signed high word: positive is away
        // from the user, or to the right.
        let forward = (m.button_state >> 16) as i16 > 0;
        let kind = if m.event_flags & MOUSE_WHEELED != 0 {
            if forward { MouseKind::ScrollUp } else { MouseKind::ScrollDown }
        } else if m.event_flags & MOUSE_HWHEELED != 0 {
            if forward { MouseKind::ScrollRight } else { MouseKind::ScrollLeft }
        } else {
            let held = m.button_state & 0x7;
            let (pressed, released) = (held & !*buttons, *buttons & !held);
            *buttons = held;
            let button = |b: u32| match b {
                _ if b & FROM_LEFT_1ST_BUTTON_PRESSED != 0 => Some(Button::Left),
                _ if b & RIGHTMOST_BUTTON_PRESSED != 0 => Some(Button::Right),
                _ if b & FROM_LEFT_2ND_BUTTON_PRESSED != 0 => Some(Button::Middle),
                _ => None,
            };
            if let Some(b) = button(pressed) {
                MouseKind::Press(b)
            } else if released != 0 {
                MouseKind::Release
            } else if m.event_flags & MOUSE_MOVED != 0 {
                MouseKind::Drag(button(held)?)
            } else {
                return None;
            }
        };
        Some(Mouse { kind, x, y })
    }

    /// The key record `k` stands for; `high` holds the first half of a
    /// surrogate pair until the next key press, which must carry its second
    /// half. (Releases come between the two halves, so they keep it.)
    fn map_key(k: &KeyEventRecord, high: &mut Option<u16>) -> Option<Key> {
        let state = k.control_key_state;
        let ctrl = state & CTRL_PRESSED != 0;
        // AltGr arrives as Ctrl+Alt and produces a printable character.
        let alt = state & ALT_PRESSED != 0;
        // A first half is good for the very next key press only.
        let first_half = high.take();
        let key = match k.virtual_key_code {
            0x08 => Key::Backspace,
            0x09 if state & SHIFT_PRESSED != 0 => Key::BackTab,
            0x09 => Key::Tab,
            0x0D => Key::Enter,
            0x1B => Key::Esc,
            0x21 => Key::PageUp,
            0x22 => Key::PageDown,
            0x23 => Key::End,
            0x24 => Key::Home,
            0x25 => Key::Left,
            0x26 => Key::Up,
            0x27 => Key::Right,
            0x28 => Key::Down,
            0x2E => Key::Delete,
            vk @ 0x70..=0x7B => Key::F((vk - 0x70 + 1) as u8),
            vk @ 0x41..=0x5A if ctrl && !alt => Key::Ctrl(char::from(vk as u8).to_ascii_lowercase()),
            _ => {
                let code = match (k.unicode_char, first_half) {
                    (0, _) => return None,
                    (c @ 0xD800..=0xDBFF, _) => {
                        *high = Some(c);
                        return None;
                    }
                    (c @ 0xDC00..=0xDFFF, Some(h)) => {
                        0x10000 + ((u32::from(h) - 0xD800) << 10) + (u32::from(c) - 0xDC00)
                    }
                    (c, _) => u32::from(c),
                };
                match char::from_u32(code) {
                    Some(ch) if !ch.is_control() => Key::Char(ch),
                    _ => return None,
                }
            }
        };
        Some(key)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn typed(unicode_char: u16) -> KeyEventRecord {
            KeyEventRecord {
                key_down: 1,
                repeat_count: 1,
                virtual_key_code: 0,
                virtual_scan_code: 0,
                unicode_char,
                control_key_state: 0,
            }
        }

        #[test]
        fn a_surrogate_pair_is_one_character() {
            let mut high = None;
            assert_eq!(map_key(&typed(0xD83C), &mut high), None);
            assert_eq!(map_key(&typed(0xDFB5), &mut high), Some(Key::Char('🎵')));
            assert_eq!(high, None);
            // A lone half is dropped, and does not stick to the next key.
            assert_eq!(map_key(&typed(0xDFB5), &mut high), None);
            assert_eq!(map_key(&typed(0xD83C), &mut high), None);
            assert_eq!(map_key(&typed(u16::from(b'a')), &mut high), Some(Key::Char('a')));
            assert_eq!(high, None);
            // Nor across another key.
            assert_eq!(map_key(&typed(0xD83C), &mut high), None);
            let enter = KeyEventRecord { virtual_key_code: 0x0D, ..typed(0x0D) };
            assert_eq!(map_key(&enter, &mut high), Some(Key::Enter));
            assert_eq!(map_key(&typed(0xDFB5), &mut high), None);
        }

        fn rec(x: i16, y: i16, button_state: u32, event_flags: u32) -> MouseEventRecord {
            MouseEventRecord { position: Coord { x, y }, button_state, control_key_state: 0, event_flags }
        }

        #[test]
        fn mouse_records_become_events() {
            use MouseKind::*;
            let mut held = 0;
            let mut ev = |r: MouseEventRecord| mouse_event(&r, (0, 10), &mut held).map(|m| (m.kind, m.x, m.y));
            assert_eq!(ev(rec(5, 12, 1, 0)), Some((Press(Button::Left), 5, 2)));
            assert_eq!(ev(rec(6, 12, 1, MOUSE_MOVED)), Some((Drag(Button::Left), 6, 2)));
            assert_eq!(ev(rec(6, 12, 3, 0)), Some((Press(Button::Right), 6, 2)));
            assert_eq!(ev(rec(6, 12, 2, 0)), Some((Release, 6, 2)));
            assert_eq!(ev(rec(6, 12, 0, 0)), Some((Release, 6, 2)));
            assert_eq!(ev(rec(7, 12, 0, MOUSE_MOVED)), None);
            assert_eq!(ev(rec(1, 11, 4, 0x2)), Some((Press(Button::Middle), 1, 1)));
            assert_eq!(ev(rec(1, 11, 0, 0)), Some((Release, 1, 1)));
            // The wheel's high word is signed.
            assert_eq!(ev(rec(3, 10, 120 << 16, MOUSE_WHEELED)), Some((ScrollUp, 3, 0)));
            assert_eq!(ev(rec(3, 10, u32::from(-120i16 as u16) << 16, MOUSE_WHEELED)), Some((ScrollDown, 3, 0)));
            assert_eq!(ev(rec(3, 10, 120 << 16, MOUSE_HWHEELED)), Some((ScrollRight, 3, 0)));
            assert_eq!(ev(rec(3, 10, u32::from(-120i16 as u16) << 16, MOUSE_HWHEELED)), Some((ScrollLeft, 3, 0)));
            // Above the window, in the scrollback.
            assert_eq!(ev(rec(3, 9, 1, 0)), None);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod sys {
    use super::{Decoder, Event, Next};
    use std::ffi::{c_int, c_ulong, c_void};
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[cfg(target_os = "linux")]
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Termios {
        iflag: u32,
        oflag: u32,
        cflag: u32,
        lflag: u32,
        line: u8,
        cc: [u8; 32],
        ispeed: u32,
        ospeed: u32,
    }

    #[cfg(target_os = "macos")]
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Termios {
        iflag: u64,
        oflag: u64,
        cflag: u64,
        lflag: u64,
        cc: [u8; 20],
        ispeed: u64,
        ospeed: u64,
    }

    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: i16,
        revents: i16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct WinSize {
        rows: u16,
        cols: u16,
        xpixel: u16,
        ypixel: u16,
    }

    #[cfg(target_os = "linux")]
    type NfdsT = c_ulong;
    #[cfg(target_os = "macos")]
    type NfdsT = std::ffi::c_uint;

    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: c_ulong = 0x5413;
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: c_ulong = 0x4008_7468;
    const TCSANOW: c_int = 0;
    const POLLIN: i16 = 1;
    const STDIN: c_int = 0;
    const STDOUT: c_int = 1;
    /// How long to wait for the rest of an escape sequence before treating
    /// a lone ESC byte as the Escape key.
    const ESC_TIMEOUT_MS: c_int = 25;

    unsafe extern "C" {
        fn tcgetattr(fd: c_int, t: *mut Termios) -> c_int;
        fn tcsetattr(fd: c_int, action: c_int, t: *const Termios) -> c_int;
        fn cfmakeraw(t: *mut Termios);
        fn poll(fds: *mut PollFd, n: NfdsT, timeout: c_int) -> c_int;
        fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn signal(sig: c_int, handler: usize) -> usize;
    }

    const SIGHUP: c_int = 1;
    const SIGTERM: c_int = 15;
    const SIG_DFL: usize = 0;

    static END: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_end(_: c_int) {
        END.store(true, Ordering::Relaxed);
    }

    pub fn end_requested() -> bool {
        END.load(Ordering::Relaxed)
    }

    pub struct RawMode {
        original: Termios,
        decoder: Decoder,
        last_size: (u16, u16),
    }

    /// Reads what `fd` has into `pending`. The end of input is an error:
    /// the terminal is gone, and waiting on it would spin forever.
    fn read_into(fd: c_int, pending: &mut Vec<u8>) -> io::Result<()> {
        let mut buf = [0u8; 256];
        // SAFETY: reading into a local buffer of the given length.
        let n = unsafe { read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(()) } else { Err(e) };
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the terminal closed its input"));
        }
        pending.extend_from_slice(&buf[..n as usize]);
        Ok(())
    }

    impl RawMode {
        /// The mouse is asked for with escape sequences, so `_mouse` changes
        /// nothing here.
        pub fn enable(_mouse: bool) -> io::Result<Self> {
            let mut t = std::mem::MaybeUninit::<Termios>::uninit();
            // SAFETY: tcgetattr fully initializes `t` on success.
            let original = unsafe {
                if tcgetattr(STDIN, t.as_mut_ptr()) != 0 {
                    return Err(io::Error::other("stdin is not a terminal"));
                }
                t.assume_init()
            };
            let mut raw = original;
            // SAFETY: valid termios pointers.
            unsafe {
                cfmakeraw(&mut raw);
                if tcsetattr(STDIN, TCSANOW, &raw) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            // Ended from outside, the session still autosaves and gives the
            // terminal back: the poll returns, interrupted, and the editor's
            // loop sees the request.
            // SAFETY: the handler only stores to an atomic.
            unsafe {
                signal(SIGHUP, on_end as extern "C" fn(c_int) as usize);
                signal(SIGTERM, on_end as extern "C" fn(c_int) as usize);
            }
            let mut m = Self { original, decoder: Decoder::default(), last_size: (0, 0) };
            m.last_size = m.size();
            Ok(m)
        }

        /// The mouse is turned on and off with escape sequences.
        pub fn set_mouse(&mut self, _on: bool) -> io::Result<()> {
            Ok(())
        }

        pub fn restore(&mut self) {
            // SAFETY: restoring the attributes read in `enable`, and the
            // default handling of the signals.
            unsafe {
                tcsetattr(STDIN, TCSANOW, &self.original);
                signal(SIGHUP, SIG_DFL);
                signal(SIGTERM, SIG_DFL);
            }
        }

        pub fn size(&self) -> (u16, u16) {
            let mut ws = WinSize::default();
            // SAFETY: TIOCGWINSZ writes a winsize struct.
            let r = unsafe { ioctl(STDOUT, TIOCGWINSZ, &mut ws as *mut WinSize) };
            if r != 0 || ws.cols == 0 || ws.rows == 0 { (80, 24) } else { (ws.cols, ws.rows) }
        }

        fn wait_readable(timeout_ms: c_int) -> io::Result<bool> {
            let mut fd = PollFd { fd: STDIN, events: POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let r = unsafe { poll(&mut fd, 1, timeout_ms) };
            if r < 0 {
                let e = io::Error::last_os_error();
                // A signal (e.g. SIGWINCH) interrupted the wait.
                return if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
            }
            Ok(r > 0)
        }

        fn fill(&mut self) -> io::Result<()> {
            read_into(STDIN, &mut self.decoder.pending)
        }

        pub fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
            let size = self.size();
            if size != self.last_size {
                self.last_size = size;
                return Ok(Some(Event::Resize));
            }
            let mut waited = false;
            let mut more_since = None;
            loop {
                match self.decoder.next() {
                    Next::Event(e) => return Ok(Some(e)),
                    Next::Empty if waited => return Ok(None),
                    Next::Empty => {
                        waited = true;
                        let ms = timeout.as_millis().min(i32::MAX as u128) as c_int;
                        if !Self::wait_readable(ms)? {
                            return Ok(None);
                        }
                        self.fill()?;
                    }
                    // The rest of a sequence has ESC_TIMEOUT_MS from when it
                    // was first waited for, however its bytes trickle in.
                    Next::More => {
                        let since = *more_since.get_or_insert_with(Instant::now);
                        let left = ESC_TIMEOUT_MS - since.elapsed().as_millis().min(ESC_TIMEOUT_MS as u128) as c_int;
                        if left > 0 && Self::wait_readable(left)? {
                            self.fill()?;
                        } else {
                            return Ok(self.decoder.timed_out());
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::fd::AsRawFd;

        #[test]
        fn input_is_read_and_its_end_is_an_error() {
            let path = std::env::temp_dir().join(format!("tinvo-term-{}", std::process::id()));
            std::fs::write(&path, b"ab").unwrap();
            let f = std::fs::File::open(&path).unwrap();
            let mut pending = vec![0x1b];
            read_into(f.as_raw_fd(), &mut pending).unwrap();
            assert_eq!(pending, b"\x1bab");
            // At the end, with a lone ESC still waiting for the rest of its
            // sequence.
            let e = read_into(f.as_raw_fd(), &mut pending).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
            assert_eq!(pending, b"\x1bab");
            drop(f);
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod sys {
    use super::Event;
    use std::io;
    use std::time::Duration;

    pub fn end_requested() -> bool {
        false
    }

    pub struct RawMode;

    impl RawMode {
        pub fn enable(_mouse: bool) -> io::Result<Self> {
            Err(io::Error::other("terminal UI is not supported on this platform"))
        }
        pub fn restore(&mut self) {}
        pub fn set_mouse(&mut self, _on: bool) -> io::Result<()> {
            Ok(())
        }
        pub fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        pub fn poll(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder(bytes: &[u8]) -> Decoder {
        Decoder { pending: bytes.to_vec(), paste: None }
    }

    fn events(d: &mut Decoder) -> Vec<Event> {
        let mut out = Vec::new();
        while let Next::Event(e) = d.next() {
            out.push(e);
        }
        out
    }

    #[test]
    fn a_paste_is_one_event_and_runs_no_keys() {
        let mut d = decoder(b"q\x1b[200~Lead\r\nX\x1b[A\x1b[201~w");
        assert_eq!(
            events(&mut d),
            [
                Event::Key(Key::Char('q')),
                Event::Paste { text: "Lead\n\nX".into(), cut: false },
                Event::Key(Key::Char('w'))
            ]
        );
        // One split across reads goes on where it left off.
        let mut d = decoder(b"\x1b[200~ab");
        assert_eq!(d.next(), Next::Empty);
        d.pending.extend_from_slice(b"c\x1b[201~");
        assert_eq!(events(&mut d), [Event::Paste { text: "abc".into(), cut: false }]);
        // Past the limit, the rest is dropped, from the first character
        // that does not fit.
        let mut long = b"\x1b[200~".to_vec();
        long.extend(std::iter::repeat_n(b'x', MAX_PASTE - 1));
        long.extend_from_slice("éx".as_bytes());
        long.extend_from_slice(b"\x1b[201~");
        let got = events(&mut decoder(&long));
        assert_eq!(got, [Event::Paste { text: "x".repeat(MAX_PASTE - 1), cut: true }]);
        // An end with no start is no event.
        assert_eq!(events(&mut decoder(b"\x1b[201~a")), [Event::Key(Key::Char('a'))]);
        // Text ending in what starts a sequence still ends at the marker.
        let got = events(&mut decoder(b"\x1b[200~ab\x1b[\x1b[201~q"));
        assert_eq!(got, [Event::Paste { text: "ab".into(), cut: false }, Event::Key(Key::Char('q'))]);
        // A lone ESC in the text takes nothing after it with it.
        let got = events(&mut decoder(b"\x1b[200~foo\x1bbar\x1b\r\x1b[201~"));
        assert_eq!(got, [Event::Paste { text: "foobar\n".into(), cut: false }]);
        // A marker split across reads, its start held past a wait.
        let mut d = decoder(b"\x1b[200~ab\x1b[2");
        assert_eq!(d.next(), Next::More);
        assert_eq!(d.timed_out(), None);
        assert_eq!(d.pending, b"\x1b[2");
        d.pending.extend_from_slice(b"01~z");
        assert_eq!(events(&mut d), [Event::Paste { text: "ab".into(), cut: false }, Event::Key(Key::Char('z'))]);
    }

    #[test]
    fn a_sequence_cut_short_is_dropped_whole() {
        let mut d = decoder(b"\x1b[");
        assert_eq!(d.next(), Next::More);
        assert_eq!(d.timed_out(), None);
        assert!(d.pending.is_empty(), "the `[` is not left to run as a key");
        let mut d = decoder(b"\x1b");
        assert_eq!(d.next(), Next::More);
        assert_eq!(d.timed_out(), Some(Event::Key(Key::Esc)));
        let mut d = decoder(&"é".as_bytes()[..1]);
        assert_eq!(d.next(), Next::More);
        assert_eq!(d.timed_out(), None);
        assert!(d.pending.is_empty());
    }
}
