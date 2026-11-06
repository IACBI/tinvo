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

## Phase 2: Sampler ✅ (0.4.0)

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

## Phase 3a: Arrangement ✅ (0.5.0)

- 16 patterns per project, each with its own length (1–64 steps); every track has steps in every pattern.
- Pattern switching while playing: the chosen pattern takes over at the end of the current one, sample-accurately. A switch made during the last step of a pattern lands one pattern later, because that step has already queued the next step's early hits.
- Song: a chain of up to 256 patterns, plus a loop region over song entries. Song edits made during playback take effect from the next step.
- Export of the song, one pattern or the loop region; per-track stems that add up to the mix.
- Copy, paste and clear a pattern, each one undo step.
- Project format 5; formats 1–4 load into pattern A.

## Phase 3b: Automation ✅ (0.6.0)

- Up to 4 automation lanes per track, for volume, pan or any instrument parameter, with points per pattern and step.
- Hold lanes change value exactly at the step's frame, before that step's notes start. Ramp lanes glide in the parameter's own scale and are evaluated every 32 frames on the absolute sample clock, so a render is identical at any buffer size.
- Before the first point of a pattern, and after stop, a parameter is at its knob value. Notes with a negative micro-timing offset start before their step and so use the value in force before it.
- Project format 6; formats 1–5 load without automation.

## Later in Phase 3

- Song-position markers and export ranges between them.
- Pattern chaining by track (different patterns per track in one song entry) if the single-chain model proves too rigid.

## Phase 4a: Channel strip ✅ (0.7.0, this release)

- Every track: high-pass, low shelf, mid bell, high shelf (RBJ cookbook biquads in `f64`), a feed-forward compressor with a soft knee, and `tanh` drive with first-order antiderivative antialiasing (measured on a driven 4.5 kHz sine: folded-back energy −34 dB against −25 dB for a plain `tanh`).
- Parameters glide over about 20 ms on a 32-frame tick kept in the strip's state, so output does not depend on buffer size; neutral settings bypass the strip bit-exactly.
- Strip parameters share the panel (`f` switches pages) and the automation lanes with the instrument's.
- Project format 7.

Known limits:
- There is no gain-reduction meter yet.
- Switching the high-pass off (down to 20 Hz) or drive off (to 0 dB) takes the stage out of the chain at once; at those settings the stage is nearly transparent, but not exactly.

## Phase 4b: Effects and buses

- Send/return buses with delay (tempo-synced), algorithmic reverb and chorus.
- Sidechain input for the compressor.
- Master chain: true-peak limiter, LUFS loudness metering (ITU-R BS.1770), oversampled processing where it reduces aliasing.
- Gain-reduction and output meters for the strip.

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
