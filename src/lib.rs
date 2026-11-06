//! TINVO — a terminal-based digital audio workstation.
//!
//! The crate is layered so that each layer only depends on the ones below it:
//!
//! ```text
//!   app        keyboard-driven UI state machine and drawing
//!   tui        terminal I/O (raw mode, key decoding, diffing screen buffer)
//!   audio      native real-time output backends (WASAPI / ALSA / CoreAudio)
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
pub mod dsp;
pub mod engine;
pub mod instrument;
pub mod project;
pub mod render;
pub mod rt;
pub mod sample;
pub mod tui;
pub mod wav;
