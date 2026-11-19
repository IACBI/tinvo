//! TINVO — a terminal-based digital audio workstation.
//!
//! The crate is layered so that each layer only depends on the ones below it:
//!
//! ```text
//!   app        keyboard-driven UI state machine and drawing
//!   tui        terminal I/O (raw mode, key decoding, diffing screen buffer)
//!   audio      native real-time output backends (WASAPI / ALSA / JACK / CoreAudio)
//!   autosave   crash-safe autosave of unsaved edits, and their recovery
//!   clap       CLAP plugin files: finding them and reading what they offer
//!   render     offline rendering, sharing the exact real-time code path
//!   project    document model, edits, persistence
//!   smf        Standard MIDI File export and import
//!   engine     real-time audio graph: transport, tracks, mixer
//!   instrument synthesizers, sampler and drum voices
//!   sample     loaded audio samples and their interpolation
//!   dsp        allocation-free signal-processing primitives
//!   rt         lock-free primitives for crossing the real-time boundary
//!   wav        WAV encoding/decoding
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

pub mod align;
pub mod app;
pub mod audio;
pub mod autosave;
pub mod base64;
pub mod bench;
pub mod clap;
pub mod dsp;
pub mod engine;
pub mod fsutil;
pub mod fuzz;
pub mod group;
pub mod insert;
pub mod instrument;
pub mod keymap;
pub mod lock;
pub mod loopback;
pub mod midi;
pub mod mix;
pub mod note_take;
pub mod plugins;
pub mod project;
pub mod record;
pub mod render;
pub mod rt;
pub mod sample;
pub mod settings;
pub mod smf;
pub mod strip;
pub mod theme;
pub mod tui;
pub mod wav;
