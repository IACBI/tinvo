# Changelog

All notable changes to TINVO are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

Author: 𝓐.𝓒.𝓑

## [Unreleased]

## [0.6.0] - 2026-11-06

### Added
- Automation lanes: up to 4 per track, for volume, pan or any instrument parameter. Points are set per pattern and step. A hold lane keeps each point's value until the next point; a ramp lane glides to it, in the parameter's own scale (frequencies glide geometrically).
- `A` in the parameter panel gives the selected parameter a lane and opens the automation view; `A` in the grid toggles the view. In the view, `Enter`/`x` adds or removes a point, `[` `]` `{` `}` change its value, `/` switches hold and ramp, `-` frees the lane.
- Copy, paste and clear of a pattern include its automation points.
- The demo bass opens its filter across the fill pattern.

### Changed
- Project files are now format 6, which adds `lane` and `point` lines. Formats 1–5 still load.
- While a parameter is automated, turning its knob sets the value it returns to when playback stops and before the first point of a pattern.
- `tinvo bench` renders the demo song instead of pattern A.

## [0.5.0] - 2026-11-06

### Added
- 16 patterns (A–P) per project, each with its own length. `n`/`N` picks the pattern to edit; while a pattern plays, the picked one takes over when the current one ends.
- `y`, `Y` and `X` copy, paste and clear a pattern. A paste or a clear is one undo step.
- Song view (`S`): a chain of up to 256 patterns with a loop region (`[` `]`, `-` removes it). `Space` in the song view plays the song from the cursor.
- Export of the song, the pattern being edited or the loop region (`←`/`→` in the export prompt). `tinvo render` takes `--song`, `--region` and `--pattern A-P`, and by default renders the song if it has entries.
- `tinvo render --stems` also writes one WAV per audible track. The stems are the same length and add up to the mix.
- The demo now has a fill pattern and a song (A A A B).

### Changed
- Project files are now format 5, which adds patterns, `song` and `loop` lines. Formats 1–4 still load, into pattern A.

## [0.4.0] - 2026-11-06

### Added
- Sampler instrument (add-track key `7`): plays a WAV file (16/24/32-bit PCM or 32-bit float, any sample rate) with 8 voices, root note, tune and fine tune, start/end points, a loop with its own start, gate or one-shot mode, ADSR, and a low-pass filter.
- Windowed-sinc interpolation (8 taps, polyphase) by default, with linear as an option. At the root pitch, playback returns the original samples.
- `o` loads a WAV file into the selected sampler track. Loading is one undo step, and deleting a sampler track and undoing brings its sample back.
- `tinvo render` plays sampler tracks and stops with an error if a sample file is missing.

### Changed
- Project files are now format 4, which adds a `sample` line to sampler tracks. A sample inside the project's folder is stored as a relative path, so the folder can be moved. Formats 1–3 still load.
- Sample buffers that the engine releases go back to the UI thread through a queue, so the audio thread never frees memory.

## [0.3.0] - 2026-11-06

### Added
- Chords: up to 4 notes per step. `c` cycles major, minor, 7, m7, maj7 and sus4; chord steps are green in the grid.
- Piano roll (`v`) for melodic tracks: `↑`/`↓` pitch, `PgUp`/`PgDn` octave, `Enter` adds or removes a note.
- Micro-timing: `(` and `)` move a step up to half a step early or late, in 1/24-step ticks.
- Synth pulse width with LFO modulation, a noise source, and unison with detune.
- `tinvo bench` also measures a worst-case synth track.

### Changed
- Project files are now format 3. Formats 1 and 2 still load.
- Pitch keys transpose a whole chord and keep it inside the MIDI range.
- CI runs on pushes to `main`, on tags and on pull requests, instead of on every branch push.

## [0.2.0] - 2026-11-06

### Added
- Undo and redo (`u`/`U`, `Ctrl+Z`/`Ctrl+Y`) with a 1000-step history. Quick repeats of the same adjustment undo as one step.
- Swing from 50 % to 75 % (`g`/`G`).
- Per-step chance (`-`/`=`) and ratchets of 1–4 hits (`/`).
- Synth glide, an LFO (sine, triangle, saw, square, sample & hold) routed to pitch, cutoff and level, and velocity → cutoff.
- The closed hi-hat chokes the open hi-hat.
- Continuous integration on Windows, Linux and macOS.

### Changed
- Project files are now format 2. Format 1 files still load.

## [0.1.0] - 2026-11-06

### Added
- Real-time engine with a sample-accurate step sequencer, mixer and meters.
- 8-voice subtractive synth and five synthesized drums.
- Native audio output through WASAPI, ALSA and CoreAudio.
- Keyboard-driven terminal editor.
- `.tvo` project files and WAV export.
- Command-line tools: `render`, `demo`, `probe` and `bench`.

[Unreleased]: https://github.com/IACBI/tinvo/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/IACBI/tinvo/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/IACBI/tinvo/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/IACBI/tinvo/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/IACBI/tinvo/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/IACBI/tinvo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/IACBI/tinvo/releases/tag/v0.1.0
