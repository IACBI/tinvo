//! Terminal user-interface plumbing: raw-mode terminal I/O, key decoding and
//! a diffing screen buffer that only redraws cells that changed.

pub mod input;
pub mod screen;
pub mod term;

pub use input::{Button, Key, Mouse, MouseKind};
pub use screen::{Color, Screen, Style};
pub use term::{Event, Terminal};
