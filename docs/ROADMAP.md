# TINVO Roadmap

Each phase ends with a usable, tested program. No phase adds a dependency.

Author: 𝓐.𝓒.𝓑

## Phase 0: Foundation ✅ (this release)

- Real-time engine: wait-free command queue, sample-accurate 16th-note step sequencer, up to 16 tracks, smoothed fader/pan/mute/solo, master bus, meters.
- Instruments: 8-voice subtractive synth (2 band-limited oscillators, SVF, 2 ADSRs) and five synthesized drums.
- Native output on Windows (WASAPI), Linux (ALSA) and macOS (AudioQueue).
- Keyboard-driven terminal UI: step grid, parameter panel, prompts, help.
- Lossless `.tvo` project format, atomic saves; WAV export at 16/24-bit (dithered) or 32-bit float.
- CLI: `render`, `demo`, `probe`, `bench`.

## Phase 1: Sound and feel

- Swing/shuffle and per-step micro-timing; per-step probability and ratchets.
- Polyphonic steps (chords) and a piano-roll view with ticks finer than a step.
- Synth: glide, LFOs, pulse-width modulation, noise oscillator, unison/detune spread, velocity → filter.
- Drum choke groups (closed hat chokes open hat).
- Undo/redo (the `Edit` stream already has the shape: store each edit with its inverse).

## Phase 2: Sampler

- WAV import (the decoder already exists) and a sample-streaming voice with linear and windowed-sinc interpolation.
- One-shot and key-mapped playback, start/end/loop points, ADSR, pitch, filter.
- Sample loading on a worker thread; buffers handed to the audio thread through the queue and freed off the audio thread.

## Phase 3: Arrangement

- Multiple patterns per track, a song timeline (pattern chain) and a loop region.
- Arrangement-wide export, rendering of individual stems, and marker-based export ranges.
- Automation lanes for any parameter (sample-accurate ramps).

## Phase 4: Mixing and effects

- Per-track insert chain: parametric EQ (RBJ biquads), compressor, saturation, delay, algorithmic reverb, chorus.
- Send/return buses, sidechain input for compressors.
- Master chain: true-peak limiter, LUFS loudness metering (ITU-R BS.1770), oversampled processing where it reduces aliasing.

## Phase 5: Devices and I/O

- Output device selection and hot-plug recovery; WASAPI `IAudioClient3` low-latency mode; CoreAudio HAL unit; JACK/PipeWire native paths.
- MIDI input (WinMM / ALSA sequencer / CoreMIDI) for live playing and step entry.
- Audio input and recording.

## Phase 6: Polish and production hardening

- Mouse support, themes, configurable keymaps.
- Crash-safe autosave and recovery.
- CI on all three operating systems, plus fuzzing of the project and WAV parsers.
- Performance budgets enforced in CI via `tinvo bench`.

## Known limitations of the foundation

- Linux and macOS backends compile and pass lint, but have not yet been run on real hardware.
- ALSA and CoreAudio open at a fixed 48 kHz (the OS resamples). WASAPI uses the device's mix rate.
- Removing a track while it is sounding cuts it off immediately, with no fade.
- Drum tone parameters apply from the next hit, not to a hit that is still ringing.
