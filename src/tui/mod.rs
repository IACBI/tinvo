//! Terminal user-interface plumbing: raw-mode terminal I/O, key decoding and
//! a diffing screen buffer that only redraws cells that changed.

pub mod input;
pub mod screen;
pub mod term;

pub use input::{Button, Key, Mouse, MouseKind};
pub use screen::{Color, Screen, Style};
pub use term::{Event, Terminal};

/// `text` safe to print to a terminal: control characters other than line
/// breaks are written as escapes, so text read from a file cannot send the
/// terminal escape sequences.
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() && ch != '\n' {
            out.extend(ch.escape_default());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Like [`printable`], with line breaks escaped too: for text from outside
/// that is meant to be one line (a device's name or id, a path) among
/// TINVO's own lines.
pub fn printable_line(text: &str) -> String {
    printable(text).replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn printable_escapes_control_characters() {
        let text = "a\u{1b}]52;c;VEVTVA==\u{7}\nb\tç";
        assert_eq!(super::printable(text), "a\\u{1b}]52;c;VEVTVA==\\u{7}\nb\\tç");
        assert_eq!(super::printable_line("a\u{1b}[2J\nb"), "a\\u{1b}[2J\\nb");
    }
}
