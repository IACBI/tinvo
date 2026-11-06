# Changelog

All notable changes to TINVO are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

Author: 𝓐.𝓒.𝓑

## [Unreleased]

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

[Unreleased]: https://github.com/IACBI/tinvo/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/IACBI/tinvo/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/IACBI/tinvo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/IACBI/tinvo/releases/tag/v0.1.0
