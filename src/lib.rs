//! TINVO — a terminal-based digital audio workstation.
//!
//! The crate is layered so that each layer only depends on the ones below it:
//!
//! ```text
//!   app        keyboard-driven UI state machine and drawing
//!   tui        terminal I/O (raw mode, key decoding, diffing screen buffer)
//!   audio      native real-time output backends (WASAPI / ALSA / JACK / CoreAudio)
//!   autosave   crash-safe autosave of unsaved edits, and their recovery
//!   render     offline rendering, sharing the exact real-time code path
//!   project    document model, edits, persistence
//!   engine     real-time audio graph: transport, tracks, mixer
//!   instrument synthesizers, sampler and drum voices
//!   sample     loaded audio samples and their interpolation
//!   dsp        allocation-free signal-processing primitives
//!   rt         lock-free primitives for crossing the real-time boundary
//!   wav        WAV encoding/decoding
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

pub mod app;
pub mod audio;
pub mod autosave;
pub mod dsp;
pub mod engine;
pub mod fsutil;
pub mod fuzz;
pub mod instrument;
pub mod midi;
pub mod mix;
pub mod project;
pub mod record;
pub mod render;
pub mod rt;
pub mod sample;
pub mod strip;
pub mod tui;
pub mod wav;
