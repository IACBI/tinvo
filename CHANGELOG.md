# Changelog

All notable changes to TINVO are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

Author: 𝓐.𝓒.𝓑

## [Unreleased]

## [0.18.0] - 2026-11-13

### Added
- Mouse support in the editor. A click toggles a step, selects a track, mutes or solos it (`M`, `S`), starts and stops playback (the play state in the header), adds or removes a piano-roll note or an automation point, selects a song entry or a parameter, and chooses an entry in the device and MIDI lists. The right button only moves the cursor there, or puts a parameter back to its default. Dragging across a track's steps fills or clears them, and dragging from a piano-roll note sets its length; each drag is one undo step. The wheel changes the value under the pointer (tempo, swing, pattern, pattern length, a parameter, an automation point, a song entry) and scrolls the tracks, steps, piano roll and key list.
- `mouse = off` under a new `[options]` section of the settings file, to leave the mouse to the terminal (for selecting text without Shift).

### Changed
- The piano roll scrolls only when the cursor leaves the view, instead of keeping the cursor in the middle, so what is under the pointer stays put.

### Fixed
- The key list's title was cut off on terminals narrower than about 120 columns; it is shorter now.
- With no audio output, the message pointing to export named `e` even when export was bound to another key.

## [0.17.0] - 2026-11-13

### Added
- A settings file for keys and colours: `tinvo.conf` in `%APPDATA%\tinvo` on Windows, `~/Library/Application Support/tinvo` on macOS, and `$XDG_CONFIG_HOME/tinvo` or `~/.config/tinvo` elsewhere. `--settings FILE` reads another one. Under `[keys]`, a line such as `undo = u ctrl+z` gives an action its keys, and an empty value leaves it with none; under `[theme]`, `base = light` picks a built-in theme and a line such as `cursor = black on 214 bold` sets a role's colour. Anything left out keeps its default. A mistake stops TINVO before it takes over the terminal and names its line; a key given to two actions that are active at the same time is refused, naming both.
- `tinvo settings [FILE]` checks the settings and prints the whole file: every action and colour role, what it does and what it has now.
- A `light` theme for terminals with a light background.
- Colours from the 256-colour palette (`0`–`255`) and 24-bit colours (`#rrggbb`), for terminals that support them.

### Changed
- Hints, status messages and the key list (`?`) show the keys actually bound, so they follow the settings file. The key list scrolls with the arrow keys and `PgUp`/`PgDn`, and any other key closes it.

### Fixed
- The piano roll now draws a note held past the end of the pattern on the pattern's first steps, where it goes on sounding when the pattern repeats; it was cut off at the last step. Steps beyond the pattern's length, kept from when it was longer, no longer draw held notes.

## [0.16.0] - 2026-11-13

### Added
- A test that the audio path does not allocate (`tests/realtime.rs`, part of `cargo test`). A counting allocator watches every call into the engine while songs play, while thousands of random edits, cues, auditions, sample changes and live-track changes arrive between callbacks of 1 to 4096 frames, and while live notes, recorded input and MIDI bytes pass through their queues. An allocation fails the test and prints where it came from.
- A sampler scenario in `tinvo bench`: eight voices resampled through the sinc interpolator, looping, through the filter.
- `tinvo bench --check`, which fails when a scenario's mean cost per callback is over its time budget. CI runs it on Windows, Linux and macOS.
- Instruction budgets: on x86-64 Linux, CI counts each scenario's instructions per second of audio with Valgrind's cachegrind (`tinvo bench --instructions bench/instructions.txt`) and fails when one is more than 10 % over its budget. Unlike time, the count does not depend on how busy the machine is, so it catches small regressions.

### Changed
- `tinvo bench` now plays each scenario in callbacks of 128 frames, as a device would, and reports the mean, 99th percentile and worst callback as a share of the period, next to the real-time factor and the budget.

## [0.15.0] - 2026-11-13

### Added
- `tinvo fuzz project|wav|edits`, a fuzzer for what TINVO reads and keeps, written with `std` alone. It mutates project files, WAV files and scripts of edits, and checks on every input that nothing panics. For projects: an accepted file saves to text that loads back equal, is rebuilt exactly by its own edits, and renders without a non-finite sample. For WAV files: the header reader, the decoder, the block reader and the sample loader agree, and the sample renders finite through a plain and a heavily processed track. For edits: every edit is undone exactly by its inverse. A failing input is reduced and saved, and `tinvo fuzz TARGET --check FILE` replays it. CI fuzzes each target for 60 seconds on every push, and `cargo test` runs a short fuzz of each.

### Fixed
These were all found by the fuzzer.
- Sampler: with the start point after the end point, the next note panicked the audio thread, and so did turning the knob while a note played. Now no note starts, and playing voices stop at the end point. This has been there since the sampler arrived in 0.4.0.
- Sampler: a float WAV holding huge values (up to the largest `f32`) played as infinite output, which then stayed in the filters and effects for the rest of the session. Loaded samples are now limited to ±1024 (+60 dBFS); non-finite values were already replaced by silence.
- A sample path with a folder or file name that starts or ends with whitespace is now refused when loading. Project files trim each line, so such a path came back changed and the sample went missing; a path made only of spaces made the saved project impossible to open.

### Changed
- The WAV reader's documentation now says that 32-bit integer samples at full scale decode to exactly 1.0, since `f32` has 24 bits of precision. It had said every integer sample stays below 1.

## [0.14.0] - 2026-11-12

### Added
- Crash-safe autosave. While a project has unsaved edits, TINVO writes it every 5 seconds to `song.tvo.autosave` next to `song.tvo`, or, for a project that was never saved, to `untitled.tvo.autosave` in TINVO's state directory (`%LOCALAPPDATA%\tinvo` on Windows, `~/Library/Application Support/tinvo` on macOS, `$XDG_STATE_HOME/tinvo` or `~/.local/state/tinvo` elsewhere). Each write is atomic, so the autosave is always a whole project. Saving deletes it, and so does quitting normally.
- Recovery. If TINVO starts and finds an autosave, the last session ended without saving (a crash, a kill, a lost terminal). It asks before anything else: `y` recovers the edits, which stay unsaved until you save; `n` discards them and deletes the autosave; `q` quits and asks again next time. An autosave that cannot be read is renamed to `….autosave.unreadable` and reported, so nothing overwrites it.
- When the session ends on a terminal error or a panic, the unsaved edits are written to the autosave right away rather than at the next interval.

### Fixed
- The `Tab` hint in the output list no longer says low latency is WASAPI-only; it works on macOS too since 0.13.0.

## [0.13.1] - 2026-11-12

A review of the 0.13.0 backends against their documentation and sources (alsa-lib, the kernel's sequencer, `aseqdump`, PipeWire's JACK, Apple's headers and technical notes), with what it found fixed.

### Fixed
- macOS: after a low-latency stream closed, the short period stayed in force for the rest of the session, because the HAL keeps one period per process. Closing the stream now puts the device's period back, so turning low latency off really turns it off.
- Linux: the ALSA sequencer reader stops on an error that would repeat, instead of spinning on `poll`.
- Linux: JACK recording no longer takes PipeWire's monitor ports (`…:monitor_FL`), which carry the playback, for a capture source.

### Changed
- macOS: if stopping the output unit fails, the render state is kept rather than freed, so a callback that might still run cannot touch freed memory. The device-lost flag is never freed, since the HAL does not say when a removed listener has finished.

### Verified
- JACK through PipeWire's own JACK library (PipeWire 0.3.65 on Debian 12, with `pw-jack`): samples go through unchanged. CI now runs the JACK tests against both jackd and PipeWire.

## [0.13.0] - 2026-11-12

### Added
- JACK on Linux, for output and recording: `--device jack` (or `jack` in the `O` list, which shows it while a server runs). It works with jackd and with PipeWire's JACK implementation, so TINVO joins PipeWire's graph directly. The ports are `tinvo:out_L`/`out_R` and `tinvo-in:in_L`/`in_R`, connected to the first two physical ports on start. libjack is loaded at run time, so it is not needed to build or run TINVO.
- MIDI input through the ALSA sequencer on Linux, which also sees software clients (virtual keyboards, other programs). Ports are named `seq:<client>:<port>`. Where there is no sequencer, TINVO falls back to rawmidi as before.
- `tinvo probe --low-latency` and the `Tab` key in the `O` list now work on macOS: the output asks the device for a 64-frame period, or its minimum if that is larger.

### Changed
- macOS output plays through an output audio unit on the HAL instead of an AudioQueue. It renders straight into the device's buffer at the device's own sample rate and period, so there is less latency and no resampling. A chosen device that disappears is reported as lost. Recording still uses an AudioQueue.
- ALSA's own `jack` plugin PCM is no longer listed; the native JACK entry replaces it.

## [0.12.0] - 2026-11-12

### Added
- Audio input and recording: WASAPI capture on Windows, ALSA capture on Linux, an AudioQueue input on macOS. `tinvo inputs` lists the input devices, and `tinvo record OUT.wav [SECONDS] [--input NAME]` records to a file and reports the length, peak and any dropped frames.
- `I` in the editor records into the selected sampler track. The take is written next to the project as `<project>-<track>-NN.wav`, a 32-bit float WAV with the device's sample rate and its first one or two channels, so nothing the device delivered is changed. `I` again stops it and loads it into the track as one undo step; undo keeps the file. If tracks are inserted or removed meanwhile, the take still goes to its own track. A take whose track was deleted is kept as a file.
- The `O` device list has an input page (`←` `→`), and `--input NAME` picks the input from the command line.
- While recording, the header shows `● REC`, the elapsed time and the input level.

### Changed
- A device name that matches nothing says "no device matches" for inputs and outputs alike.

## [0.11.0] - 2026-11-12

### Added
- MIDI input: WinMM on Windows, ALSA rawmidi on Linux, CoreMIDI on macOS. `tinvo midi` lists the inputs. The editor opens the first one it finds. `--midi NAME` picks another (by id or a unique part of its name), `--midi none` turns MIDI off, and `M` in the editor switches inputs.
- Live playing: notes from the keyboard sound on the selected track. They go from the MIDI thread straight to the audio engine through a lock-free mailbox, not through the UI, and start at the beginning of the next audio callback. Drums hit at their own pitch whatever the key. Up to 32 notes can be held at once; one more releases the oldest.
- Step entry: with `R` on, each key writes the step under the cursor (velocity from the key, length and other settings from the track's brush), and the cursor moves on when every key is up. Keys held together write a chord of up to four notes as one undo step. On drum tracks only the velocity is taken.
- The header shows `REC` while step entry is on and a MIDI activity light while an input is open.

## [0.10.0] - 2026-11-12

### Added
- `tinvo devices` lists the output devices (the system default marked), and `--device NAME` plays on one, given by its id or a unique part of its name, in the editor and in `tinvo probe`.
- `O` in the editor opens the output device list: the system default (which follows changes) or a specific device, switched without leaving the session.
- The editor keeps the audio going when devices change. A lost stream (device unplugged, driver reset) is reopened, retrying every 1 s and backing off to 8 s. When the system default changes (WASAPI), the editor moves to the new default. While a chosen device is missing it plays on the default and returns to the chosen one when it reappears. Reconnecting stops playback, and nothing is lost.
- `--low-latency` (and `Tab` in the device list) asks WASAPI for the device's smallest shared-mode period through `IAudioClient3`. Drivers whose smallest period is their default report that low latency is unavailable, and the normal stream is used.
- `tinvo probe` reports the device, the period and the low-latency state.

### Changed
- ALSA lists the PCMs its hints offer for playback and opens the chosen one; CoreAudio lists the devices with output streams and plays on the chosen one by its UID. Both still default to the system default.

## [0.9.0] - 2026-11-12

### Added
- A third effect bus, a stereo chorus (two swept taps per side, a quarter cycle apart between the sides, with rate and depth), fed by a new chorus send on every track.
- Sidechain compression: every track has a key send into a key bus, taken from the instrument before the strip and the fader (so a muted track still keys), and each compressor can listen to the key bus instead of its own input.
- Bus and master settings can be automated: `A` on the buses and master page gives the setting a lane on the current track. One lane in the project can hold each setting; removing the track or stopping playback returns it to its knob value.
- Meters in the editor: compressor gain reduction on the channel-strip page, the limiter's gain reduction and momentary, short-term and integrated loudness (since playback started) on the buses and master page, and short-term loudness in the header.
- The demo ducks its bass under the kick through the key bus and puts chorus on the open hat.

### Changed
- The drive runs at twice the sample rate (polyphase IIR half-band filters, 99 dB stopband) on top of its antiderivative antialiasing: aliasing of a hard-driven 4.5 kHz sine falls from -25 dB (plain waveshaper) to -73 dB, and the top end is kept (-1.1 dB at 15 kHz, was -5.1 dB). Projects that use drive sound slightly different; everything else renders exactly as in 0.8.0.
- The reverb pre-delay glides to a new setting instead of jumping.
- Project files are now format 9 (`comp_key`, `send_chorus` and `send_key` parameters, chorus `mix` lines, lanes on bus and master settings). Formats 1–8 still load.

### Fixed
- Long delay-time glides stalled up to about 37 frames short of the new time (f32 rounding at the slowest tempos) and then kept interpolating; they now land exactly.

## [0.8.0] - 2026-11-06

### Added
- Two effect buses fed by sends on every track: a tempo-synced delay (1/32 to 1/2, straight, dotted and triplet; feedback, a tone low-pass in the feedback path, stereo or ping-pong) and an algorithmic reverb (an eight-line feedback delay network with decay time, damping and pre-delay). Sends are taken after the fader and pan, so mute, solo and fader moves apply to them.
- A true-peak limiter on the master: 1.5 ms look-ahead, ceiling and release, 4× oversampled peak detection. Exports compensate its look-ahead, so they still start on the first beat; with it off, nothing in the signal path changes.
- `tinvo render` and the in-app export report integrated loudness (ITU-R BS.1770-4, gated, in LUFS) and the true peak alongside the sample peak.
- `f` now steps through three panel pages: instrument, channel strip (now with the two sends) and buses and master. Send levels can be automated; bus and master settings cannot yet.
- The demo sends its snare and clap to the reverb and its open hat and clap to the delay, and runs 3 dB hotter into the limiter.

### Changed
- Project files are now format 8, which adds `send_delay` and `send_reverb` parameters and `mix <id> <value>` lines; only settings away from their defaults are written. Formats 1–7 still load and sound exactly as before.
- Stems are taken before the master limiter and carry their own share of the delay and reverb returns.

## [0.7.0] - 2026-11-06

### Added
- A channel strip on every track, between the instrument and the fader: high-pass (12 dB/octave), low shelf, mid bell and high shelf (±18 dB), a compressor (threshold, ratio, attack, release, makeup, 6 dB soft knee) and drive (antialiased `tanh` saturation). At its defaults the strip is bypassed bit-exactly and costs nothing.
- `f` switches the parameter panel between the instrument and the strip. Strip parameters can be automated like any other.
- Strip parameter changes glide over about 20 ms, so turning a knob or a hold point does not click. The high-pass and the drive, which are not transparent even at their lowest settings, crossfade in and out over 10 ms; a compressor turned down to 1:1 releases at its release time; an opened project starts on its strip settings instead of gliding into them, so export and playback match from the first sample.
- `tinvo bench` also measures the demo with every strip stage on and the mid band swept.
- The demo uses the strip: high-passed hats, snare and clap, a low shelf on the kick, a compressed and driven bass.

### Changed
- Project files are now format 7, which adds the strip's `param` lines; only settings away from the neutral defaults are written. Formats 1–6 still load.
- Export streams to disk: memory use no longer grows with the length of the export. A 17-minute song took 771 MB to export before and takes 7 MB now; the files are byte-for-byte the same. Stems are written the same way.
- Exports and saves write to a uniquely named temporary file created with `create_new` (it never opens an existing file or link) and rename it over the target. A failed export leaves nothing behind and does not touch an existing file.
- Loading a sample reads the WAV header first and refuses a file that is too long before reading its audio, which is then read in blocks of at most 64 KiB and mixed to mono as it goes.
- The audio callback spends a fixed work budget on commands; adding or removing a track (which moves whole track slots, about 30 µs) counts as 512 simple edits. The rest wait, in order, for the next callback.
- Samples that no track uses and no undo or redo step can bring back are freed.

### Fixed
- Changing samples faster than the UI collected the released ones could overflow the queue that returns them, and the engine then leaked them on purpose. The UI now counts every sample it has handed to the engine, including those of the project it opened with, and keeps at most as many in flight as that queue holds.

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

[Unreleased]: https://github.com/IACBI/tinvo/compare/v0.18.0...HEAD
[0.18.0]: https://github.com/IACBI/tinvo/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/IACBI/tinvo/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/IACBI/tinvo/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/IACBI/tinvo/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/IACBI/tinvo/compare/v0.13.1...v0.14.0
[0.13.1]: https://github.com/IACBI/tinvo/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/IACBI/tinvo/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/IACBI/tinvo/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/IACBI/tinvo/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/IACBI/tinvo/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/IACBI/tinvo/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/IACBI/tinvo/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/IACBI/tinvo/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/IACBI/tinvo/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/IACBI/tinvo/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/IACBI/tinvo/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/IACBI/tinvo/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/IACBI/tinvo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/IACBI/tinvo/releases/tag/v0.1.0
