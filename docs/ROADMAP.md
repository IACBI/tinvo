# TINVO Roadmap

Each phase ends with a usable, tested program. No phase adds a dependency.

Author: 𝓐.𝓒.𝓑

## Phase 0: Foundation ✅

- Real-time engine: wait-free command queue, sample-accurate 16th-note step sequencer, up to 16 tracks, smoothed fader/pan/mute/solo, master bus, meters.
- Instruments: 8-voice subtractive synth (2 band-limited oscillators, SVF, 2 ADSRs) and five synthesized drums.
- Native output on Windows (WASAPI), Linux (ALSA) and macOS (AudioQueue).
- Keyboard-driven terminal UI: step grid, parameter panel, prompts, help.
- Lossless `.tvo` project format, atomic saves; WAV export at 16/24-bit (dithered) or 32-bit float.
- CLI: `render`, `demo`, `probe`, `bench`.

## Phase 1: Sound and feel ✅ (0.2.0)

- Swing 50–75 % (delays odd 16ths, sample-accurate, ratchets follow it).
- Per-step chance (1–100 %, reproducible: the random sequence restarts on every Play) and ratchets (1–4 evenly spaced hits).
- Synth: glide, an LFO (sine, triangle, saw, square, sample & hold) to pitch, cutoff and level, and velocity → cutoff.
- Closed hat chokes open hat (12 ms fade, no click).
- Undo/redo with a 1000-step history. Each edit is stored with its inverse; quick repeats of the same nudge merge into one step.
- Project format 2 (swing, chance, ratchet); format 1 files still load.
- CI on Windows, Linux and macOS: format, clippy, tests in debug and release, CLI smoke test, minimum Rust version.

## Phase 1b: Notes ✅ (0.3.0)

- Chords: a step holds up to 4 notes, stored as intervals above its lowest note, so transposing keeps the shape. `c` cycles common chords.
- Piano roll (`v`) for melodic tracks: pitch × step, held notes drawn through their length.
- Micro-timing: each step moves up to half a step early or late in 1/24-step ticks (96 per quarter note). Early hits are queued a step ahead, so they play on time even at the first step after a loop, and stay exact under swing.
- Synth: pulse width with LFO modulation, a noise source, and unison (up to 4 detuned copies per oscillator).
- Project format 3; formats 1 and 2 still load.
- `tinvo bench` also measures a worst-case synth (8 voices, unison 4, noise, LFO).

## Phase 2: Sampler ✅ (0.4.0, this release)

- Sampler track: 8 voices playing a WAV file in memory, pitched from a root note (±24 semitones of tune plus fine tune), following the file's own sample rate.
- 8-tap polyphase windowed-sinc interpolation (Blackman window, 256 interpolated phases) or linear. The sinc kernel is exact at integer positions, so the root pitch plays the file back unchanged.
- Start/end points, a loop with its own start point, gate or one-shot mode, ADSR, low-pass filter.
- Sample buffers reach the audio thread inside a command and leave it through a return queue, so they are freed on the UI thread.
- Project format 4: relative sample paths inside the project folder, absolute elsewhere.

Known limits, planned for later phases:
- Files load on the UI thread. A long file pauses the editor briefly while it is read; playback is not affected.
- Stereo files are mixed to mono.
- One sample per track; there are no multi-sample key maps yet.
- The interpolation kernel does not lower its cutoff when pitching up, so a sample transposed far upwards can alias.

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
- Fuzzing of the project and WAV parsers.
- Performance budgets enforced in CI via `tinvo bench`.
- Notes that span the loop point in the piano roll display (they already play correctly).

## Known limitations of the foundation

- Linux and macOS backends compile and pass lint, but have not yet been run on real hardware.
- ALSA and CoreAudio open at a fixed 48 kHz (the OS resamples). WASAPI uses the device's mix rate.
- Removing a track while it is sounding cuts it off immediately, with no fade. The same applies to undoing an added track.
- CI runs on hosted Windows, Linux and macOS machines, which have no sound card: the backends are compiled and linted there, not played through.
- Drum tone parameters apply from the next hit, not to a hit that is still ringing.
