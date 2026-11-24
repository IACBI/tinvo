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

## Phase 4a: Channel strip ✅ (0.7.0)

- Every track: high-pass, low shelf, mid bell, high shelf (RBJ cookbook biquads in `f64`), a feed-forward compressor with a soft knee, and `tanh` drive with first-order antiderivative antialiasing (measured on a driven 4.5 kHz sine: folded-back energy −34 dB against −25 dB for a plain `tanh`).
- Parameters glide over about 20 ms on a 32-frame tick kept in the strip's state, so output does not depend on buffer size; neutral settings bypass the strip bit-exactly.
- Strip parameters share the panel (`f` switches pages) and the automation lanes with the instrument's.
- High-pass and drive crossfade in and out over 10 ms; a compressor set to 1:1 finishes its release before it leaves the chain.
- Every stage on, with an automated mid band: about 0.2 % of one core per track (`tinvo bench`).
- Project format 7.

Known limits:
- There is no gain-reduction meter yet.
- Drive's antialiasing averages neighbouring samples in its linear region, a high-frequency roll-off of −3 dB at a quarter of the sample rate (12 kHz at 48 kHz); oversampled drive would avoid it.

## Phase 4b: Effect buses and master ✅ (0.8.0)

- Post-fader sends on every track to a delay bus and a reverb bus.
- Delay: tempo-synced note values, feedback with a low-pass in the loop, stereo or ping-pong; time changes glide instead of jumping.
- Reverb: an eight-line feedback delay network with a Hadamard mixing matrix, prime line lengths, per-line damping and four input diffusers; decay time is measured within 15 % of the setting (T30, Schroeder integration).
- Master: a stereo-linked true-peak limiter (look-ahead with a running minimum and a moving average, so the ceiling holds without the gain ever stepping), compensated in exports.
- Export reports integrated loudness (BS.1770-4, checked against EBU Tech 3341 cases 1 and 2) and true peak.
- Buses that receive nothing and have fallen silent cost nothing; projects without sends render bit-exactly as in 0.7.0.
- Demo with buses and limiter: about 1.7 % of one core (`tinvo bench`), from 1.3 %.
- Project format 8.

Known limits:
- Bus and master settings could not be automated (done in 4c).
- The true-peak estimate is a 4× interpolation with a 12-tap kernel: within 0.15 dB up to 12 kHz, but up to 0.5 dB low around 18 kHz (at 48 kHz), so material with strong content up there can peak slightly above the ceiling.

## Phase 4c: More mixing ✅ (0.9.0)

- Chorus on a third bus: two taps per side swept in opposite directions around 12 ms, the right side a quarter cycle behind the left, read with Hermite interpolation.
- Sidechain compression through a key bus: a pre-strip, pre-fader key send on every track, and a key switch on each compressor.
- Automation of bus and master settings on any one track's lanes; the reverb pre-delay now glides like the delay time.
- Gain-reduction meters for the compressors and the limiter, and live loudness (momentary, short-term, integrated, the last as a 0.1 LU histogram, within 0.05 LU of the exact meter).
- Drive at twice the sample rate with polyphase IIR half-band filters: aliasing of a hard-driven 4.5 kHz sine at -73 dB, against -25 dB for a plain waveshaper.
- Project format 9; projects without drive render bit-exactly as in 0.8.0.

Known limits:
- The oversampling filters are not linear phase: a driven track is delayed by about 3.3 frames at low frequencies (more towards the top), and while drive is switched in or out its 10 ms crossfade briefly mixes the two.
- Drive costs about 0.7 % of one core per track (`tinvo bench`): every strip stage on all six demo tracks takes about twice what it did in 0.8.0 on the same machine.
- Bus and master settings other than the delay time, the pre-delay, the chorus depth and the return levels change at once; automated ramps move them in 32-frame steps.

## Phase 5a: Output devices ✅ (0.10.0)

- Output device listing and selection on all three backends (`tinvo devices`, `--device`, `O` in the editor).
- Recovery: a lost stream is reopened with backoff; the editor follows the system default (WASAPI) and falls back to the default while a chosen device is missing, returning when it reappears.
- WASAPI `IAudioClient3` low-latency mode at the device's smallest shared-mode period.

Known limits:
- The low-latency path was only exercised where drivers refuse it: every device on the development machine reports a smallest period equal to its 10 ms default, so `InitializeSharedAudioStream` itself has not run on real hardware.
- Hot-plug recovery was tested by switching between real devices and by asking for a missing one; actually unplugging a device mid-stream has not been tested.
- ALSA and CoreAudio device listing compiles and links in CI, but has not run on real hardware. ALSA and CoreAudio cannot report the default device changing; the system routes their default streams itself.
- Reconnecting rebuilds the engine from the project: playback stops, and ringing notes and effect tails are cut.

## Phase 5b: MIDI input ✅ (0.11.0)

- MIDI input through WinMM, ALSA rawmidi and CoreMIDI, with a byte-stream parser that handles running status, interleaved real-time bytes, system exclusive and system common messages.
- Live playing on the selected track: a lock-free mailbox from the MIDI thread to the audio engine, bypassing the UI.
- Step entry with chords (`R`), and input selection (`tinvo midi`, `--midi`, `M`).

Known limits:
- No MIDI device was available while developing this phase. The parser, the engine's live notes, step entry and the path from the MIDI handler to both mailboxes are tested; the WinMM, rawmidi and CoreMIDI code compiles and links in CI but has never received a real message.
- Live notes start at the beginning of the audio callback after they arrive, so their timing jitters by up to one device period (10 ms on the development machine) on top of the output latency. Backends' timestamps are not used yet.
- Only notes were used: no sustain pedal, pitch bend, controllers or MIDI channels (every channel played the selected track; Phase 10b added them). There was no live recording while the pattern played (Phase 10a added it).
- On Linux, rawmidi sees hardware and USB ports only, not software clients of the ALSA sequencer; Phase 10b added sequencer input.
- A MIDI input that is unplugged stays silent until another one is picked with `M`; it is not reopened by itself.
- If more than 256 notes arrived within one audio callback, the extra ones were dropped (a dropped release would leave a note held until the key is pressed again). Now they wait for the next callback, while the mailbox has room.

## Phase 5c: Audio input and recording ✅ (0.12.0)

- Capture on all three backends, through a wait-free ring of whole frames from the capture thread to the UI, which streams it to a 32-bit float WAV.
- Recording into sampler tracks (`I`) as one undo step, `tinvo record`, `tinvo inputs`, an input page in the device list and `--input`.

Known limits:
- Recording is free-running: it starts and stops on the key, not on the beat, and is not aligned with playback. Input and output latency are not compensated, so a take recorded over the beat lands late by both latencies.
- There is no input monitoring through TINVO; use the interface's or the system's own monitoring.
- The ring holds 2^20 frames (about 21 s at 48 kHz). If the editor stalls longer than that, frames are dropped, counted and reported, never torn.
- A take ends at the longest sample TINVO loads (2^25 frames, about 11.6 minutes at 48 kHz).
- WASAPI capture was recorded from the development machine's microphone (3 s, 48 kHz stereo, no dropped frames). ALSA and CoreAudio capture compile and link in CI but have not run on real hardware. macOS asks for microphone permission the first time and delivers silence until it is granted.

## Phase 5d: More I/O ✅ (0.13.0, reviewed in 0.13.1)

- macOS output through an output audio unit on the HAL, at the device's rate and period, with a low-latency mode.
- JACK output and recording on Linux, loaded at run time; PipeWire is reached through its JACK implementation.
- MIDI input through the ALSA sequencer, with rawmidi as the fallback.

Known limits:
- PipeWire is reached through its JACK API, not libpipewire's own stream API (which needs the SPA POD builder, a header-only C API). PipeWire's libjack has to be the one the system loads. `pw-jack tinvo` always arranges that; otherwise it depends on the distribution's packaging. On Debian and Ubuntu the `pipewire-jack` package ships an `ld.so.conf.d` example to copy (checked on Debian 12); Fedora installs one, though some releases have shipped it in a package that dnf did not pull in.
- JACK is chosen explicitly (`--device jack`); the default is still ALSA's `default` PCM. TINVO connects to the first two physical ports only (jackd and PipeWire list the default device's first); mono hardware gets the left channel.
- The JACK path runs in CI against a real jackd (dummy driver) and against PipeWire's JACK, and ran against both in a Linux container: samples go through unchanged. It has not run with real audio hardware.
- The ALSA sequencer has not run anywhere: neither the development machine's container nor the CI runners have the `snd-seq` kernel module. It compiles, links and is lint-clean; its event layout was checked against `<alsa/seq_event.h>` with a C program. Its calls were reviewed against alsa-lib's `seq.c` and `seq_hw.c`, `aseqdump` and the kernel's `seq_clientmgr.c`: an input-only client may create a port, subscribe and receive (the kernel delivers only to clients opened for reading, and alsa-lib opens input-only clients read-only); the read loop matches `aseqdump`'s; `-ENOSPC` means the FIFO overran and was cleared; and events from MIDI 2.0 clients are converted by the kernel for legacy clients like TINVO's.
- The audio unit output runs in CI on the macOS runner's virtual devices and on BlackHole, including low latency being turned on and off again. It has not run on a real Mac's hardware here. Freeing the render state after `AudioOutputUnitStop` relies on its being synchronous when called from another thread, as an Apple engineer described it; Apple's headers do not say so.

## Phase 6a: Autosave and recovery ✅ (0.14.0)

- New unsaved edits are written at most every 5 seconds, atomically, next to the project (or in the state directory for an untitled one); saving or quitting normally removes the autosave.
- At start-up an autosave left by a session that did not end normally is offered for recovery, discarded, or kept for next time.
- A terminal error or a panic writes the autosave at once.

Known limits:
- Up to 5 seconds of edits can be lost if the process is killed. Within that window nothing is written, so a crash costs at most that much.
- Two TINVO sessions on the same project, or two untitled ones, share one autosave file: the one that writes last wins, and a session started while another runs is offered the running one's autosave. Nothing locks the project file either. (Fixed in phase 8b.)
- The autosave is renamed into place but its directory is not synced afterwards, so after a power failure (not a crash of TINVO) some file systems may come back with the previous autosave or none.
- The flow was run end to end on Linux in a pseudo-terminal: edits, `kill -9`, the recovery question, recovering and saving (the saved file equals the autosave byte for byte), and discarding. The writes on a terminal error and on a panic are covered by unit tests of the pieces, not by a run that actually fails mid-session. The recovery screen has not been tried by hand in a Windows console.

## Phase 6b: Fuzzing ✅ (0.15.0)

- `tinvo fuzz` for project files, WAV files and edit scripts, with invariants that go past "does not panic": save round trips, the engine's view of a project, undo, the sample loader against the decoder, and finite output when an accepted input is played.
- Runs in CI for 60 seconds per target on pull requests, pushes to `main` and release tags, and briefly in `cargo test`.
- It found four problems on its first runs, all fixed: a panic on the audio thread when a sampler's start point is after its end point, infinite output from float WAV files with huge values, sample paths that a project file could not give back, and a wrong promise about the range of 32-bit WAV samples.

Known limits:
- It is not coverage-guided. That needs compiler instrumentation and a fuzzing runtime (`cargo fuzz` uses libFuzzer, a nightly toolchain and crates). Instead an input is kept when it shows a feature not seen before: a new error, or a new kind of line, chunk or edit in an accepted input. Mutations that know the formats make up for part of the difference; a coverage-guided fuzzer would still reach some branches sooner.
- Rendering covers the first half second of an accepted project, without its samples (the files are not there); sample rendering uses fixed projects. One accepted input in 16 is rendered by default.
- Edit scripts stop after 64 edits, since each edit clones the project twice to check its undo.
- Before 0.15.0 it ran for about 24 million project inputs, 4.7 million WAV files and 200,000 edit scripts (two 10-minute runs per target on one machine). Finding nothing more in that time does not show that no bug is left.

## Phase 6c: Performance budgets ✅ (0.16.0)

- A test that counts allocations on the audio path (`tests/realtime.rs`): the engine while it plays, takes edits, cues, auditions, sample changes and live notes in callbacks of random sizes, plus the capture side of recording and the MIDI parser and mailboxes. Any allocation fails it with a backtrace.
- `tinvo bench` measured four scenarios at first (the demo, the demo with every strip stage on, a worst-case synth, a worst-case sampler; ten as of 0.29.0, listed in ARCHITECTURE) per callback of 128 frames: mean, 99th percentile and worst, as a share of the period. `--check` fails when a mean is over its scenario's budget; CI runs it on every platform.
- CI counts the instructions per second of audio of each scenario with cachegrind and fails when one is more than 10 % over `bench/instructions.txt`.

Known limits:
- The time budgets are loose (for the first four scenarios, 10 to 25 % of the period on average, against at most 6.2 % measured in two CI runs) because CI runners are shared, so a slowdown has to be large before they fail. The 99th percentile and the worst callback are printed but not enforced, since a shared machine's scheduler decides them.
- Instruction counts are checked only on x86-64 Linux, and they move with the compiler, and slightly with the C library and CPU (a Debian container's counts differed from CI's Ubuntu by up to 2.7 %): a new stable Rust can push them past the tolerance without a change in TINVO. The budget file then has to be updated from CI's output.
- Instructions are not time: a change that misses the cache more, or uses slower instructions, can cost more time with the same count.
- The allocation test covers what the backends call on the audio thread, not the backend callbacks themselves (format conversion, device buffers). Those are checked by reading, and on Linux and macOS by running them in CI.

## Phase 6d: Keys and colours ✅ (0.17.0)

- A settings file (`tinvo.conf` in the user's configuration folder, or `--settings FILE`) rebinds any of the editor's 71 actions and sets the colour of any of its 45 roles. A key given to two actions that are active at the same time is refused, naming the key and both actions, before the terminal is touched.
- Two built-in themes: the default for dark terminals and `light` for light ones. Colours can be the 16 named ones, a palette index or 24-bit `#rrggbb`.
- `tinvo settings` checks the file and prints every action and role with its value, ready to copy.
- Hints, messages and the key list name the keys actually bound. The key list scrolls, so it fits on small terminals.
- The piano roll draws a note held past the end of the pattern on the pattern's first steps, where it goes on sounding when the pattern repeats.

Known limits:
- Some keys are fixed: answering questions (`y`, `n`, `Esc`), the instrument number after adding a track, and editing text in prompts.
- The built-in themes use the 16 ANSI colours, which each terminal's palette defines, so they look different from one terminal to another. The light theme's contrast was computed from the xterm and Windows Terminal (Campbell) palettes on a white background, and its views were looked at drawn with those palettes; it has not been looked at in a real light terminal.
- 24-bit colours need a terminal that supports them; others show something else or nothing.
- A note held past the pattern's end is drawn as the pattern repeating. In a song whose next entry is another pattern, the note still sounds over it, but the roll does not show that.
- There is no editor for the settings inside TINVO; the file is edited by hand.

## Phase 6e: Mouse ✅ (0.18.0)

- Clicks: a step toggles, a track's name selects it, `M` and `S` mute and solo, the play state in the header starts and stops. In the piano roll a click adds or removes a note, and a key on the left plays its pitch; in the automation view it adds or removes a point; in the song view it selects an entry; in the panel it selects a parameter. Device and MIDI lists choose the entry clicked. The right button only moves the cursor, or puts a parameter back to its default.
- Dragging across a track's steps fills or clears them, as the first step clicked went. In the piano roll, dragging from a note sets its length; a press on a note and a release without dragging removes it. Each drag is one undo step.
- The wheel changes what is under the pointer: tempo, swing, pattern and pattern length in the header, a parameter, an automation point, a song entry's pattern. Over the tracks it moves the selection, over the beat ruler the step, in the piano roll the pitches, in the key list the lines.
- `mouse = off` under `[options]` in the settings file leaves the mouse to the terminal.
- The piano roll now scrolls only when the cursor leaves the view, so a click does not move what is under the pointer.

Known limits:
- With the mouse on, most terminals select text only with Shift held (or with `mouse = off`).
- On Windows the mouse comes as console input records. Windows Terminal passes the mouse to console programs through ConPTY; that path has not been tried with a real mouse here. What was tested on Windows: records written into a real console's input buffer (paint, wheel, mute, in a console with no pointer over it), and the conversion of every kind of record by unit tests.
- On Linux and macOS TINVO asks for SGR reports (mode 1006) and also reads the older encoding, which cannot name a column or row past 223. A terminal with neither reports no mouse. The Linux path was run end to end in a pseudo-terminal with real report bytes; it has not been tried with a real mouse in a desktop terminal here.
- Questions and text prompts answer only to keys. There is no double click, and Shift, Ctrl and Alt do not change what a click does.
- A drag paints along one track and within the steps on screen; a note's length can be dragged up to the last step shown. A key pressed during a drag ends it.

## Phase 6f: Foundation limits ✅ (0.19.0)

- A track removed while it sounds, or an added track undone, fades out over 10 ms instead of stopping dead. During the fade it plays on exactly as it would have, through its strip, fader and sends; then its sample is released.
- Drum parameters reach a hit that is still ringing: tune, the decays, click, tone and the clap's spread, whose bursts still to come keep to the new spacing. An open hat cut short by a closed one stays short when its decay grows.
- ALSA output and input open at a rate the device takes without resampling (ALSA's own resampler is a linear one): 48 kHz when it can, else 44.1 kHz, else the first common rate it takes. CoreAudio input runs at the device's nominal rate, as output already did.

Known limits:
- Each removed track fades in one of 16 slots. More than 16 removals within one fade cut short the fade with the fewest frames left; the editor removes one track per key press, so it does not come to that.
- ALSA's rate is chosen with resampling turned off. Where every rate passes that check, as with `null`, TINVO opens at 48 kHz as before, and a sound server behind ALSA converts if it runs at another rate. The choice was checked on plug devices over `null` with a fixed rate (44.1, 96 and 22.05 kHz, for playback and recording), not on real hardware.
- CoreAudio input at the device's rate has run on the macOS CI runner's BlackHole only.

## Phase 7a: Markers and export ranges ✅ (0.20.0)

- Named markers on song entries, at most one per entry and each name once. In the song view `m` names the entry under the cursor (an empty name removes its marker), `PgUp` and `PgDn` jump to the previous and next marker, and `=` makes the section around the cursor the loop region. A section runs from a marker, or the song's start, to the entry before the next marker, or the song's end.
- The song view lists the markers on a line of their own (a click jumps to one), shows `|` before a marked entry and names the cursor's section in its title.
- Markers move with their entries when entries are inserted or removed; a removed entry takes its marker with it, and undo brings both back. They are part of the undo history, like track names.
- `tinvo render --from MARKER --to MARKER`: `--from` alone renders that marker's section, `--to` stops where its marker starts, and `--to` alone starts at the song's start. The range plays as the loop region does, so `--loops` repeats it. The export prompt names a loop region that matches markers, such as `section 'verse'`.
- Project format 10 (`marker 2 verse`); formats 1–9 load without markers. The fuzzer checks that markers stay in order, on the song and uniquely named through edit scripts and in project files.

Known limits:
- A marker is a name for the start of an entry; it does not change what plays. Ranges are whole song entries.
- The marker line lists names in one row and cuts off what does not fit the width; `PgUp` and `PgDn` still reach every marker.
- Like track names, marker names are drawn one column per character, so double-width characters (CJK, most emoji) push the rest of the line along.

## Phase 7b: Per-track patterns ✅ (0.21.0)

- In each song entry a track follows the entry's pattern, plays a pattern of its own, or rests. The entry's pattern sets how long the entry lasts. A track's own pattern starts on the entry's first step; a shorter one starts over when it ends, a longer one is cut off when the entry ends.
- A track's automation follows the pattern it plays. A ramp towards a point the entry never reaches holds its value to the entry's end; a resting track is at its knob values.
- The song view is a grid: entries across, the song's own row of patterns on top and one row per track below it, showing `·` (the entry's pattern), a pattern of its own, or `-` (a rest). `↑` `↓` move between rows and `,` `.` change the cell: the entry's pattern on the song's row; on a track's row its part, through the entry's pattern, A to P and a rest. `Del` on a track's row makes it follow again. Repeating an entry copies its parts. Entries are shown a screenful at a time, as steps are in the grid.
- Parts move with their entries; removing an entry, or a track, and undoing it brings its parts back. Exports and stems play them; the grid's play column follows each track's own pattern.
- Project format 11 (`parts . B -` in a track block, up to its last part of its own); formats 1–10 load with every track following. The fuzzer checks that no part is left past the song's end, and the real-time test sends parts while playing.

Known limits:
- `↑` and `↓` now move between the song view's rows; changing an entry's pattern moved to `,` and `.`.
- A track's own pattern always starts on the entry's first step; it cannot start part-way or carry on from the entry before.
- The play column in the grid, piano roll and automation view shows the selected pattern only for tracks playing it. (The piano roll and the automation view show the rest of the song on their song line since phase 8d.)

## Phase 8a: Hardware check ✅ (0.22.0)

- `tinvo loopback [AUDIO] [--input NAME]` plays five 0.1 s sweeps (500 Hz to 8 kHz, −12 dBFS) on an output and finds them in what an input hears, by cross-correlation summed over the whole train. It shows that the output and the input work end to end and measures the round trip: from TINVO handing a frame to the output to the input handing it back. Each stream's clock is the median of its callbacks' times, so the scheduler's jitter does not move the result.
- The sweeps start at the marks of a Golomb ruler (0, 1, 4, 9 and 11 times 0.2 s), where no two pairs are the same distance apart: a train placed wrongly lines up with one sweep at most, so two sweeps heard where they belong fix where it is even when others are lost. A sweep counts as heard when it stands out and each of its halves (500 Hz to 2 kHz, 2 to 8 kHz) stands out on its own there, which a lone tone or click does not; at least two must be heard. Both channels of a stereo input are searched.
- `tinvo probe` also reports the longest gap between callbacks, and says when it was longer than the buffer, where the output may have glitched.
- Run on the development machine (Windows 11, Realtek on-board audio, WASAPI shared mode at 48 kHz with a 10 ms period):
  - The Realtek speakers into the Realtek microphone: all five sweeps heard, 67.1 to 67.5 ms round trip over seven runs, the sweeps of one run agreeing within 0.5 ms.
  - The NVIDIA Broadcast virtual speakers into the Realtek microphone: 162 to 171 ms; its processing adds about 100 ms.
  - The Realtek speakers into the NVIDIA Broadcast microphone: about 321 to 327 ms when heard. Its noise removal takes out parts of the sweeps, more or less from one run to the next: one run heard four of five, another only one, which is reported as not heard.
  - `--low-latency` on both devices: refused by the driver (smallest period equal to the 10 ms default), so the round trip stays 67.3 ms.
  - `tinvo probe`: 48 000 frames/s within 0.35 % on both outputs, the longest gap between callbacks 10.8 to 11.7 ms against a 22 ms buffer.
  - `tinvo bench --check`: mean callback cost 3.4 % (demo), 9.0 % (every strip stage), 6.1 % (synth) and 1.0 % (sampler) of a 128-frame period.

Known limits:
- The round trip includes the room: about 0.3 ms per 10 cm between the speaker and the microphone.
- The output and the input must run at the same rate; `loopback` says so when they do not, as it does not resample.
- No MIDI device and no second machine were available: WinMM, ALSA and CoreMIDI input have still never received a real message, and ALSA and CoreAudio still have not run on real hardware.

## Phase 8b: Project lock ✅ (0.22.0)

- A session holds `song.tvo.lock` next to its project: an exclusive `flock` on Linux and macOS, a handle that shares no writing on Windows. The system lets go of both when the process ends, however it ends, so a crash never leaves the project locked. The file names the process holding it. A symbolic link, a hard link or anything but a plain file at the lock's name is refused, never followed, so the lock cannot be made to overwrite another file.
- Opening a project another session has open is refused, naming that process; so is saving over one. Saving under a new name moves the lock to the new file. Another spelling of the same path (`./song.tvo`) is the same project.
- Untitled sessions each take a slot in the state directory (`untitled.tvo`, `untitled-2.tvo`, up to 16), with its own autosave. A free slot whose autosave was left by a crash is taken first, so it is offered for recovery.
- The lock is taken before the autosave is looked for, so a running session's autosave is never offered as a crashed one's.
- Tested on Windows in a real console: a second `tinvo song.tvo` (and `tinvo ./song.tvo`) is refused with the first one's process id, and once that console is closed the lock file is gone and the project opens. Unit tests cover a lock held by another process that is then killed, a lock file left by a crash, slots, and saving.

Known limits:
- Where the lock file cannot be made (a read-only directory), the project opens unlocked, with a warning.
- `flock` on a network file system may lock only on one machine, depending on the server; the Windows lock covers SMB shares.
- `tinvo render` reads a project without locking it, so it can export one a session is editing; it reads the last saved file.

## Phase 8c: Settings page ✅ (0.22.0)

- `F2` (or `P`) opens a page with three tabs: Keys, every action by context with its keys; Colours, the base theme and each role's style with a sample; Options, the mouse. `Enter` adds a key (the next key pressed) or changes a value, `Del` removes an action's last key, `r` puts back the default, `Esc` closes. An entry that differs from the default is marked `*`.
- A change works at once: a new key is bound, a colour is drawn, the mouse is turned on or off in the running terminal. A key that would be ambiguous, or a style that does not read, is refused with the same message the settings file gives, and nothing changes. The settings page and quit always keep one key.
- Closing the page writes the changes into the settings file (`--settings`, or the default one), changing only the lines whose values changed and adding the rest to their section, so comments and the order of the file stay. Only what the page changed is written over the file as it is now, so another session's saves are kept, and this session takes them up; one that clashes with a change made here is refused, naming the key. The result is read back and compared before it is written, atomically. A file that no longer reads is left alone, and the settings apply until TINVO quits.
- The page's own keys (arrows, `Enter`, `Del`, `r`, `Esc`) come before any binding, so no binding can lock it. A key the settings file has no name for (a terminal may send `Ctrl+M` for Enter) is refused rather than added.
- Unit tests cover adding, refusing and removing keys, the guarded keys, the page's keys under other bindings, keys the file cannot name, merging with another session's saves and refusing a clash, a new key working at once, colours and the base theme, a mistake in a colour, the mouse request, writing a new file and an existing one with its comments, a file that no longer reads, drawing, scrolling and clicks.

Known limits:
- Not tried by hand in a real console: the keys and drawing are unit-tested on a frame, and switching the mouse on and off at runtime is not checked in a real terminal.
- A key the terminal does not send (some `Ctrl` and `Alt` combinations) cannot be added from the page, only typed into the file.

## Phase 8d: The song in the piano roll ✅ (0.22.0)

- The piano roll and the automation view end with a song line: the pattern the selected track plays in each song entry, after its own part, the entry's pattern or a rest (`-`). Entries where it plays the pattern being edited stand out, and the label says how many there are (`Song: B in 3 of 8 entries`). The entry playing now is in the play column's colour, so while the song plays it shows where the track is even when that is another pattern.
- A click on an entry opens the pattern the track plays there; on a rest it says so.
- Entries are shown a screenful at a time, with the one playing, or else the song cursor's. The line takes the last of the view's rows when there are at least eight and the song is not empty.
- Unit tests cover what the line shows for followed, own and rested entries, the edited pattern standing out, the playing entry and its page, clicks, the automation view, and an empty song or a small terminal leaving the view as it was.

Known limits:
- The line shows one track; the song view shows them all.
- Markers and the loop region are not drawn on it.

## Phase 8e: Review ✅ (0.22.0)

A review of the whole project. Each finding was checked against the code and fixed with a test that fails without the fix:
- Audio: the limiter's running minimum kept within its ring through long releases; send sums cleared whole, so a short block cannot leave old input for a longer one; buses and the master run segment by segment, so their automation lands on its frame at any buffer size; each synth voice has its own noise; a stepped strip setting jumps instead of gliding forever.
- Timing and export: a tempo change keeps a swung step's position; the export tail waits until the delay, reverb and chorus are idle; stems drop lanes that would turn the limiter on.
- Live input: closing or switching the MIDI input, or a release lost to a full queue, releases every live note and resets step entry.
- Editing: loading a changed file again reaches every track using it; the parameter cursor is kept in range after a finished recording selects its track.
- Security: control characters in error messages are escaped.
- Screen: wide characters take two cells and combining marks none; the panel pages on small terminals; prompts keep the typed text in view; titles, bus parameter names and the settings page's messages fit.

Known limits:
- Character widths come from the common ranges (CJK, Hangul, full-width forms, emoji, combining marks), not Unicode's full tables; terminals also differ on some characters.
- Closing a real MIDI input was not tried here (no MIDI device on the development machine); the release on closing and on a lost message are unit-tested.

## Phase 9a: Step divisions ✅ (0.23.0)

- A step length per pattern: 1/8, 1/8T, 1/16 (as before), 1/16T or 1/32. The transport counts 24 pulses per quarter note, so every division is a whole number of pulses; for 16ths the frame arithmetic is the one from before, and 16th-note projects play bit for bit as they did.
- Each track keeps its own next step on the division of the pattern it plays, so song entries and per-track parts of different divisions play together. A track's own pattern is cut where the entry ends, its ratchets with it. Swing moves the second step of each pair; triplet steps are not swung.
- The grid, the piano roll and the automation view draw beats and bars for the division; the playhead follows each track's own steps. `b` `B` or the wheel over the top bar set it, and copies keep it.
- Up to 8 ratchets per step.
- Project format 12 (`pattern_division B 1/16T`); older files load as 16ths.

Known limits:
- All rows of a pattern share its division.
- Micro-timing keeps 24 ticks per step, so its finest offset depends on the step length.

## Phase 9b: Time signatures ✅ (0.23.0)

- A time signature per pattern, 1 to 16 beats of /2, /4, /8 or /16 notes, set with `k` `K` and `J` or the wheel over the top bar. It places the beat and bar lines in the grid, the piano roll and the automation view; the ruler leaves a space between its labels in long bars.
- Format 12 writes `pattern_meter B 6/8` for patterns not in 4/4; older files load in 4/4.

Known limits:
- The time signature does not change playback or the pattern's length; it only draws the lines.

## Phase 9c: Tempo-synced LFO ✅ (0.23.0)

- The synth's `LFO sync` sets one LFO cycle per note value, the delay's values from 1/32 to 1/2 and then 1/1, 2/1 and 4/1; `free` keeps the rate in Hz. A synced LFO starts its cycle when playback starts, also when the first step's automation syncs it, so playback and export land on the beat alike.
- Format 12 adds `param lfo_sync`; older files load with a free LFO.

## Phase 9d: Tempo points ✅ (0.23.0)

- A song entry can start at a tempo of its own. The tempo holds until the next point or, with ramping on, moves there in a straight line. Before the first point the song plays at the set tempo.
- The transport takes a new origin at each entry's exact fractional frame and integrates a ramp exactly, so steps land on the integral of the tempo at any buffer size, and song and region exports last as long as the song plays. The delay and synced LFOs follow the tempo playing.
- The song view has a tempo row: `{` `}` or the wheel set the entry's point, `0` removes it, `/` switches between hold and ramp. Its title shows the cursor entry's tempo, and the top bar the tempo playing while it differs from the set one.
- Format 12 adds `tempo_point <entry> <bpm>` and `tempo_ramp true`. Points move with their entries, and undo brings a removed entry's point back.

Known limits:
- Tempo points sit at the start of song entries; a tempo change inside a pattern needs an entry of its own.
- A pattern playing on its own, outside the song, plays at the set tempo.

## Phase 10a: Recording notes ✅ (0.24.0)

- `C` records the notes played on the live track while the pattern or song plays. The engine finds each key's nearest step on the frame it sounds, among the steps of the pattern the track plays there (where swing puts them) and the next entry's first step, and reports it with its offset in ticks through a take queue, along with each step the track plays. The UI turns the take into step edits: one undo step, overdub or replace, micro-timing kept or quantized, a chord of up to four notes per step, the length in steps as held, which the engine measures against the steps it plays. Keys still held when recording stops end there, and tracks inserted or removed meanwhile keep each note on its own track.
- The engine counts how many record commands it has handled, so the UI writes the take only once every note pushed before the stop is in its queue.
- A count-in of one or two bars clicks the pattern's beats in the engine, sample-accurately at any buffer size, and starts playback on the frame the last beat ends. A key up to half a step before that lands on the first step.
- The settings file has `record_mode`, `record_timing` and `count_in` under `[options]`, and the settings page lists them.

Known limits:
- Notes record onto steps: a note shorter than a step lasts a step, and two notes on one step play together.
- Recording from a stop plays the pattern being edited. To record over the song, start it and press `C` in the grid while it plays, without a count-in.

## Phase 10b: Pedal, bend, mod wheel and channels ✅ (0.24.0)

- MIDI input decodes controllers and pitch bend as well as notes, on every backend: the byte parser (CoreMIDI, rawmidi), WinMM's short messages and the ALSA sequencer's controller and pitch bend events. The engine's live mailbox carries the MIDI messages themselves, and `Message::Lost` takes the place of the old all-off note.
- The sustain pedal holds a key's voice after its release until the pedal lifts, per channel; the take ends a held note's length there.
- Pitch bend moves the synth's and sampler's voices by up to a `Bend range` parameter per track; the synth's `Mod wheel` parameter picks what the wheel moves, from its value toward its maximum on the parameter's own scale (logarithmic for the cutoff).
- A track can have a MIDI channel of its own (`H`, `midi_channel` in format 13). Notes and wheels on that channel play every track that has it; channels no track has play the selected track. The engine keeps each channel's bend and wheel and gives every track those of the channels that play it whenever they or the routing change, so a track no longer selected, moved by an insert or whose channel went to another track is back at rest. Recorded keys are told apart by channel and note and end on their own release or pedal, whatever is selected by then; losing the input lifts every pedal and puts every wheel back.

Known limits:
- Bend and wheel are not recorded into steps, and the wheel moves the synth only.
- Bend and wheel positions take effect at the start of each audio callback, like live notes, and are not smoothed.

## Phase 10c: Standard MIDI Files ✅ (0.24.0)

- Export (`src/smf.rs`) writes a format 1 file at 960 ticks per quarter note: a tempo track (each song entry's tempo, a ramp as one tempo per 16th note lasting exactly as long as that 16th, time signatures and markers), then one track per heard track. Notes are placed in musical time the way the engine plays them: swing, offsets in ticks, ratchets inside a step cut short by its entry, lengths that run past the pattern, and each track's part in each entry. A key struck again on its channel ends the note before it.
- Import reads format 0 and 1 files (running status, system exclusive and unknown meta events skipped) into a new project. Each MIDI track and channel is a synth track, and on channel 10 each General MIDI drum TINVO has is a drum track. Notes go to the nearest step of a 16th-note grid on a tick grid of 24 per step, so an exported file reads back exactly; a step holds up to four notes. The project is read again from its own export until the two agree, so exporting an import gives it back. The file is cut into patterns of whole bars, and at each bar line where the tempo changes, a stretch that repeats reusing its pattern, and the song plays them in order; the last one ends with the bar of the last note.
- The CLI renders to a `.mid` file and imports one to a new project; the editor opens a MIDI file unsaved and exports to one from its export prompt.
- `tinvo fuzz midi` checks every file the reader accepts: the project it reads loads back equal from its own text, and exporting it and reading that again is a fixed point.

Known limits:
- Steps with a chance below 100% are all written.
- Offsets, ratchets and lengths are placed in musical time. Where the tempo changes inside one (a ramp, or the next entry's tempo point), playback times it by the tempo at its step instead, so the file can differ from playback there, also on whether a hit just before the export's end is heard; a note still sounding when the song ends keeps its length in steps, where playback times it by the song's start.
- Imported tracks are plain synths: program changes, controllers, pitch bend and General MIDI drums TINVO has no sound for are not read. Notes past 32 tracks, 16 patterns or 256 song entries are left out and counted.
- Import puts every note on a 16th-note grid, so triplets and 32nd notes become offsets.
- An imported tempo change inside a bar takes effect from the next bar line.
- A file timed in SMPTE frames, and format 2, are refused.

## Phase 10d: MIDI output and clock ✅ (0.24.0)

- Output backends next to the input ones: WinMM `midiOut*` short messages; on Linux a `TINVO:output` sequencer port connected to the chosen port with `snd_seq_connect_to`, sending direct events, or a rawmidi device written to without the sequencer; on macOS a CoreMIDI output port sending one packet list per message.
- A track can play on the MIDI output instead of sounding (`V`, `midi_out true` in format 14). It sends on its own MIDI channel, or channel 1; drums send their General MIDI note on channel 10. The engine stamps each message with its exact frame plus the limiter's latency, so it lines up with the audio heard. A retriggered key is released first, and Stop, removing the track, muting it (or soloing another) and switching MIDI out off release every note it sent; switching it on lets the track's own voices go. Live notes from the MIDI keyboard go out too, held by the sustain pedal as they would sound (their release is sent when it lifts).
- MIDI clock (`midi_clock` on the settings page): 24 clocks per quarter note on their exact frames at any buffer size, following tempo points and ramps. Play sends Start, or for a song entry after the first its Song Position and Continue; an entry that starts between two 16th notes continues on the next. Stop sends Stop.
- Each audio callback sends the output thread an anchor (its first frame, its length, the rate and when it started), so the thread maps frames to wall-clock time a buffer after their callback. It keeps the earliest estimate and lets it move later only slowly, so one late callback does not shift what follows. `midi_delay` (0–50 ms) sends everything later, for an output heard sooner than the audio. Note-ons and clocks more than 50 ms late are dropped; releases, Start, Stop, Continue and Song Position never are. One the output refuses holds back what follows for up to a second; a release or Stop is then owed and tried again until the output takes it, and a Start, Continue or Song Position is dropped. The thread counts each key's starts and releases, so two tracks playing one key on one channel each release it, and it never sends a message before one the engine queued earlier, even when the limiter's latency fell between them. Clocks go only between Start or Continue and Stop, so an output chosen while playing gets them from the next Play. A new engine, an engine silent for 250 ms beyond four of its callbacks, or closing the output releases every note still sounding and stops a running receiver, trying again what the output refuses, before anything new for up to a second (on closing, for 200 ms). After the engine's queue was full and lost a message, the next callback releases every note sounding then, and stops the receiver unless the clock runs.
- `Z` chooses the output, `tinvo midi` lists inputs and outputs, and `--midi-out NAME` opens one at start.

Known limits:
- Only notes, the clock and transport are sent: no controllers, pitch bend or program changes.
- The clock runs only while playing. A loop back to its start is not signalled to the receiver, and turning the clock on while playing waits for the next Play.
- Timing follows the audio callbacks' start times plus about 1 ms of polling, so its jitter is the callbacks'. A callback's frames are taken to be heard one callback after it starts; a device that buffers more (ALSA keeps several periods) plays later than that, which `midi_delay` makes up for. TINVO cannot follow an external clock.
- Two tracks playing one key on one channel share it, as MIDI does: most receivers end it on the first release.
- MIDI's song position reaches 16,383 16th notes (1,024 bars of 4/4); from a song entry past that, the receiver is not started.
- An audio export leaves MIDI-out tracks silent; a MIDI file export keeps their notes.

## Review: performance, security and dead code ✅ (0.24.0)

A second review of the whole project, made with an independent model reading the code. Each finding was checked against the code, and the fixes that change behaviour have tests:
- Security: the CoreAudio input callback borrows only its callback mutably; the end of terminal input is an error instead of a busy loop; names and paths from devices and the command line are printed escaped on one line; a failed WAV writer leaves no temporary file; a recording never replaces a file created at its name meanwhile; ALSA MIDI overruns and read errors release every held note; project and settings files are read up to a size, lines are split only as far as their longest valid form, and an action takes up to 8 keys.
- Performance: drum envelopes flush denormals; tracks playing one file share its samples; grouped undo steps are built in linear time.
- Dead code: accessors nothing used were removed, and the DSP module's documentation now names the two parts that allocate and run off the audio thread.

Known limits (found bounded, and left for later):
- A long note's release and the song view's length look up each song entry's tempo by searching the tempo points, so the work is bounded but grows with the square of the song's size; a table per entry, rebuilt on edits, would make it constant.
- A change to one synth or mix parameter recomputes all of that instrument's or the mix's coefficients (both envelopes of every voice, every reverb gain), up to every 32 frames under automation.
- Each synth voice computes its filter's `exp2` and `tan` every sample, also when nothing modulates the cutoff.
- The piano roll formats a new string for every cell it draws.
- An export's loudness meter keeps every 100 ms block, so its memory grows with the export's length, if slowly.

## Review: performance, security and dead code, third pass ✅ (0.24.1)

A review of all of 0.24.0, with extra attention to Phase 10, made with an independent model reading the code, which then reviewed the fixes. Each finding was checked against the code. The fixes that change behaviour have tests that fail without them; the LFO's test shows it moves on as before, and the input threads' stop checks and the UI's budget have none, since they need a source that never pauses:
- Security: a terminal reporting a huge size (a pseudo-terminal may claim 65,535 columns and rows, gigabytes of cells) gets a screen of at most 1000 × 500; a MIDI file export refuses to walk more than a million steps of one track, silent ones included, before it allocates them; a sample path is at most 4096 bytes, in a project file and when loaded; a take records at most 100,000 keys and says how many it left out, and once full counts no more passes, so replacing keeps what it holds; the ALSA sequencer and rawmidi threads stop even while a source sends without a pause, and the UI takes at most a mailbox's worth of notes a frame; a rawmidi output that takes only the start of a message gets its end before anything else, and on the output thread's next look even with nothing else to send, instead of the message being sent again from its start; WinMM ports whose names repeat or end in ` #2` each get an id of their own.
- MIDI timing: a note-on dropped as late skipped the next release of its key, which could be another note's, so that one sounded on; each note now carries a tag, and its own release is the one skipped. A JACK callback longer than 1,024 frames anchored each part as a callback of its own, which moved the estimate of when frames are heard earlier and dropped notes as late; it is anchored once as a whole.
- Performance: a sampler with no sample no longer schedules releases for the notes it does not play; a synth with no voice sounding moves its LFO on without working out its value.
- Dead code: `Division::steps_per_beat` was removed; helpers only tests use (`render_stems`, `wav::write_file`, `Coefs::response_db`, the screen's `char_at`, `style_at` and `row_text`) are built only for tests; the fuzzing module's documentation describes the MIDI target.

Known limits (found bounded, and left for later):
- A synth voice stolen for a new note keeps its old release queued until it is due; it then does nothing. The queue is bounded per track.

## Review: the whole project after 0.28.0 ✅ (0.28.1)

A review of all of 0.28.0, the repository's documents and its CI, made with an independent model reading the code, which then reviewed each fix. Each finding was checked against the code, and each fix that changes behaviour has a test that fails without it:
- Files: Save As writes `.tvo` files and no save writes over a sample the project plays; the project reader refuses a second `parts` line for a track and a marker past the longest song where they stand; a WAV file of more chunks than a file holds is refused; a MIDI file that ends inside a track or names more tracks than it has says so; `fuzz --check` reads its input as the readers do.
- The sampler: an SFZ import resets the start and end points and is refused while a lane moves them; an undo that brings back a file no longer there says which and why; `tinvo sfz` reports what the import would load; `sw_up` is named as played whatever is held.
- MIDI input: All Notes Off and the mode messages end a channel's keys, and All Sound Off stops the instruments it plays at once, a plugin by a note choke; a lost one is sent again.
- Real time: on macOS, the plugin host's per-thread values live in pthread keys' slots, since the system gives a thread-local storage from the heap at first use; a capture device failing records its status without allocating.
- CI: Windows plays and records through a virtual sound card, Rust 1.85 checks every platform, the smoke test covers `slice`, `sfz` and zones, the BlackHole tests no longer overlap, and a benchmark holds 1024 zones.
- Documents: README, ARCHITECTURE, CONTRIBUTING, SECURITY and the pull request template say what the code does.

Known limits (left as they are):
- A plugin id interned for an instance that failed to load keeps its place in the process's pool of 4096: taking it back is unsafe while other threads may intern the same id.
- Interpolation reads across a slice's cut into the next slice, as the file does; each zone's end fades over 64 frames.
- While the MIDI input's queue overflows, a lost All Sound Off sent again a frame later can cut a note played just after it.
- The first callback of the HAL's audio thread has not been traced for allocations on a Mac; Rust's counting allocator cannot see the system's.

## More kinds of music

TINVO is at home with pattern-based electronic music: techno, house, electro, synthwave, lo-fi and boom bap, ambient loops. Phase 9 added triplet and 32nd-note grids, time signatures and tempo changes, and Phase 10 live-played parts: recording, the pedal and wheels, MIDI files and MIDI output. The phases below take it further, toward larger arrangements, plugins, sampled instruments and recorded vocals and instruments. Phases 11 to 14 have shipped; Phase 15 is planned. They are ordered so that each builds on the ones before. Like every phase so far, each ends with a usable, tested program and adds no dependency. Sizes are relative.

### Phase 11: Mixer (medium) ✅ (0.25.0)

For larger arrangements and the mix structure they need: drum and vocal groups, parallel compression, more parts.
- 32 tracks.
- Group buses: a track goes to a group or to the master. Groups have the channel strip, sends and automation.
- Insert chains on tracks, groups and the master: an ordered list of the built-in effects (EQ, compressor, drive, delay, reverb, chorus) and new ones (a multimode filter, a phaser, a bit crusher, a gate). Each slot can be bypassed and automated.
- The real-time rules hold: chains are built away from the audio thread and swapped in, and old ones leave through the release queue.
- A new project format.
- Tests: routed sums against a mix made by hand, chain order, click-free bypass, latency compensation for slots that look ahead, stems per group, and the allocation test while chains change during playback.

It came in four parts, each merged on its own; the project format they share, format 15, is frozen with 0.25.0.

#### 11a: 32 tracks ✅

- A project holds 32 tracks, in the editor, in project files (still format 14: an older TINVO refuses a file of more than 16 tracks with an error) and in MIDI file import.
- A lane's target is a `LaneTarget` that says what it automates, a parameter of its own track or a bus or master setting, in place of a number with the mix settings from 128 up. The groups and insert slots to come are further kinds of target.
- Removed tracks fade out in 16 slots of their own (`MAX_FADING`), as before.
- A `tracks32` bench scenario, with an instruction budget.
- Tests: the last track's own bit for its MIDI channel and for releasing the notes it sent when muted, inserting a track at the last place and removing it, a full project refusing another, and the allocation test with every track playing.

#### 11b: Group buses ✅

- Up to 8 group buses. A track goes to a group or to the master: `i` sends it to the next group, `d` makes a new group for it. A change of group crossfades over 10 ms, and a deleted group plays on in a slot of its own while its tracks fade to the master and its strip rings out.
- A group runs while fed and for two seconds after, then costs nothing; its strip then lets go of what it held (filter states, the compressor's reduction).
- A group has a stereo channel strip (one compressor detector for both sides), volume, balance, mute, and sends to the delay, reverb and chorus taken after its fader. Its page in the panel follows the strip page for a track in a group, with the group's name, size, level and gain reduction in the title. A lane on any track can automate it.
- Muting a group silences its tracks ahead of solo, and MIDI-out tracks in it release their notes.
- `E`, `Q` and `F` rename, mute and delete the track's group; deleting sends its tracks to the master and frees its lanes in one undo step.
- Project format 15: `group` blocks before the tracks, `group <n>` in a track and lane ids `group<n>.<id>`. Older files load with no groups.
- Stems: one per track that goes to the master and one per group with a heard track in it.
- A `groups` bench scenario with an instruction budget. The allocation test plays 32 tracks through 8 busy groups and sends random group edits.
- Tests: routed sums against a mix made by hand, bit-exact pass-through at the defaults, a group stopping on the same frame at any buffer size, deletion while playing, sends after the group fader, mute and solo, automation at any buffer size, the crossfade, renumbering, the stereo strip and its linked compressor, file round trips, stems per group, and the editor's keys and page.

Known limits: a track sent to a third place while a change of group is still fading moves the share still on its way out at once; and when more than 8 removed groups are still playing on, the oldest is cut short.

#### 11c: Insert chains ✅

- 8 slots on every track and group (after the strip, before the fader) and on the master (after its fader, before the limiter), 64 effects in a project. A slot holds an EQ, compressor or drive, which are the strip's stages, or a delay, reverb or chorus with a mix setting; each kind's first setting is its bypass.
- Effects are built on the UI thread and handed over in `Command::SetSlot`; the ones replaced or removed go back through the release queue, which now carries samples and effects and has room for 256.
- A change to a chain (an effect in, out, replaced or moved) fades the slots it touches out over 10 ms, applies on the frame they are out and fades the new contents in. A change made during a fade joins it. Bypass crossfades too, and a bypassed delay or reverb runs silence through until it has rung out, so it comes back without old echoes. A group stays on while its chain rings, and an idle group takes changes at once.
- Lanes can automate any effect setting (`LaneTarget::Slot`, `GroupSlot`, `MasterSlot`, naming the effect they expect). A lane locks its slot's effect; moving the effect takes the lane along.
- The panel's inserts page: an effect key picks the slot's effect, others move it, empty it and show the track's chain, its group's or the master's.
- Project format 15 gains `insert`, `insert_param`, `master_insert` and `master_insert_param` lines and lane ids `insert<k>.<id>`, `group<n>.insert<k>.<id>` and `master.insert<k>.<id>`.
- Stems are taken before the master's chain.
- An `inserts` bench scenario with an instruction budget. The allocation test plays 64 busy effects, some automated, and sends random chain edits with effects built off the audio thread.
- Tests: chain order and swaps, bypassed effects bit-exact, a group of one with an effect against its track with it, click-free insertion, bypass, moving and removal, effect automation at any buffer size, a chain change on the same frame at any buffer size, a group held on by its echoes, effects returning through the release queue (and the queue never overflowing), file round trips, edits rebuilding the chains, undo, malformed lines, the insert limit, locked slots, stems before the master's chain, and the editor's inserts page.

#### 11d: New effects and latency compensation ✅

- Four more kinds: a multimode filter (low-pass, band-pass, high-pass, notch), a phaser, a bit crusher and a gate, each with its bypass and automatable settings, in project format 15 under the ids `filter`, `phaser`, `crusher` and `gate`.
- The gate looks 1.5 ms ahead, so it delays its signal, bypassed too. Every track and group has a delay after its chain that the engine sets so that all paths reach the master equally late; a change crossfades over 10 ms on the same frame at any buffer size. `Engine::latency` includes it, so MIDI out, export and stems stay lined up, and the count-in click is delayed with the tracks.
- The `inserts` bench scenario cycles through all ten kinds, and the allocation test plays them.
- Tests: the new effects against their definitions, paths through gates on tracks, groups and the master lined up to the frame, a gate put in and taken out while playing at any buffer size and without a click, a chain's latency, an export through a gate starting on time, stems lined up with the mix, every kind through a file, and the review's fixes (a delay waiting for its history, following a change mid-crossfade and switched back on forgetting what it held, a short hit passing a gate whole, a 0 dB range opening a closed gate, a held-out gate's bypass on the same frame at any buffer size, a resonant filter recovering from an overflow, a group ringing out or removed keeping its latency and resting on the same frame at any buffer size, a delay set twice on one frame taking the second, a stopped count-in leaving no click, the count-in, MIDI out, a live note and an auditioned note as late as the tracks).

Known limits:
- A grouped track's own sends reach the buses ahead of its group's output by the group's latency and its compensation.
- While a track's change of group crossfades, and while a removed track fades out, the path it leaves is delayed as the plan for its new place says, so for those 10 ms it can be off by a gate's look-ahead.
- A sidechain key is not delayed: a keyed compressor or gate on a delayed path hears its key early by that delay.
- A gate put in waits for its look-ahead of input before its crossfade while the paths around it start theirs at once, so for those 10 ms they can be apart by up to its look-ahead.
- An export whose limiter a lane switches on or off at the very start keeps the latency the limiter had before.

#### Review: front end and back end, fourth pass ✅ (0.25.0)

A review of the whole project before the release, the terminal front end as closely as the engine, made by two independent reviewers who checked each other's findings over three rounds. Each finding was checked against the code, and the fixes that change behaviour have tests that fail without them (mutation-checked):
- Terminal input: a sequence cut short is dropped whole within 25 ms; bracketed paste finds its end marker first, and a cut paste or a full prompt says so on the prompt line; Windows key repeats (at most 64) and surrogate pairs; SIGTERM and SIGHUP end the session cleanly, also at the recovery question; a click aimed at a screen that has since changed is dropped; the editor draws less often once idle.
- Drawing: prompts edit at a cursor and step over a letter with its accents; text is cut and padded by columns; small screens keep the panel, the automation lanes, the song view's rows, the top bar and the hints apart and in view; the key list wraps.
- Files: an export refuses the project, its samples and folders, and `render --stems` checks every stem before the mix is written; lock files are checked through the handle that holds the lock and deleted only by their holder; the settings are saved under their lock, keeping each line's own ending; only plain files are read, and a named pipe is opened without waiting.
- Engine: an unfed delay plays every repeat, and groups and the chorus come to rest on the same frame, at any buffer size; a tempo edit reaches every chain's delay; a take's layout reports keep room of their own; a MIDI release after a reset leaves a newer note alone; one-shot voices; the samples' budget is checked on load, undo and redo.

Known limits: the main loop's stale-click and idle handling, signal delivery, Windows key-repeat delivery and the CoreAudio stop failure have no unit tests, since they need a real terminal or device. A screen cell keeps one combining mark.

### Phase 12: CLAP plugins (large) ✅ (0.27.0)

For professional instruments and effects: sampled pianos and orchestral libraries, amp models, mastering tools.
- Hosting CLAP 1.2 plugins. CLAP is a plain C interface on Windows, Linux and macOS, so it is reached through hand-written FFI like the audio backends. VST3 (C++ COM-style interfaces) and Audio Units (macOS only) stay out.
- Scanning the standard folders and `CLAP_PATH` for `.clap` files and bundles, away from the audio thread, with a cache.
- A plugin as a track's instrument or in an insert slot. Its parameters (`clap.params`) appear in the panel and on automation lanes, its latency (`clap.latency`) is compensated, and its state (`clap.state`) is saved inside the project file.
- The plugin's own window (`clap.gui`) opens as a floating window where the plugin offers one; otherwise TINVO opens a window of its own and puts the plugin's inside it, since a terminal has no window to place it in.
- The spec's thread rules: main-thread calls from the UI thread, processing on the audio thread, and the host's callbacks honoured.
- Export runs plugins through the same engine. A plugin whose output depends on timing or block size can make export differ from playback; TINVO says so instead of hiding it.
- A plugin plays inside TINVO's process, so a plugin that crashes while playing ends the session (the autosave keeps the project); only the scan reads each file in a child process. Running plugins in a separate process is not planned.
- A new project format: the plugin's id and state. A missing plugin keeps its state and is named when the project opens; a missing instrument is silent and a missing insert passes its input.
- Tests: two small CLAP plugins built in the repository (a gain and a sine instrument) for scanning, loading, parameters, state, latency and the thread rules, and the allocation test with a plugin playing.

It comes in four parts, each merged on its own. The first three came with 0.26.0, which freezes the project format they share, format 17; the fourth came with 0.27.0.

#### 12a: Finding plugins ✅ (0.26.0)

- `tinvo plugins [--rescan]`. The folders CLAP's `entry.h` names for the system (`%COMMONPROGRAMFILES%\CLAP` and `%LOCALAPPDATA%\Programs\Common\CLAP`; `~/.clap` and `/usr/lib/clap`; `/Library/Audio/Plug-Ins/CLAP` and `~/Library/Audio/Plug-Ins/CLAP`), then those in `CLAP_PATH`, are searched for `.clap` files (bundles on macOS) without following links to folders, at most 16 deep, through 100,000 entries and up to 10,000 files.
- Each new or changed file is read by a child process, `tinvo scan-clap`, with its standard streams closed. It loads the file (`LoadLibraryExW` searching the plugin's own folder for its libraries; `dlopen` with `RTLD_NOW | RTLD_LOCAL`; on macOS the bundle's executable as CoreFoundation finds it), calls `init` and `deinit` once each, reads the factory's descriptors (strings read to at most 1 KiB, at most 256 descriptors of at most 64 features, an id and a name required, 1 MiB in all) and writes its result, into a folder of the scan's own in the user's temporary folder (readable by the user alone on Unix), only after the file's `deinit`. A child that crashes, runs past 10 s or leaves no whole result marks the file as not loaded, with the reason. Processes a plugin starts end with its child where the system allows it: on Windows the child puts itself in a job that ends them when it ends (unless a job it is already in forbids that), on Unix the child leads a process group that is ended after it, before it is reaped (on Linux and macOS, where an ended child can be seen without reaping it; elsewhere only a child that runs out of time has its group ended), so the group's id cannot have been reused; a helper that leaves the group survives. A scan's 2 minutes count from before the search, which its own limits bound, and are checked before each child: a child started near the end gets only the time left, and the files it does not finish are read by the next scan, as are those whose child could not start or write its result, or that changed while they were read.
- The cache, `plugins.txt` in the state directory, keeps each file's size and modification time (on Unix its inode and change time too; on macOS those of the bundle's `Info.plist`, of the executable CoreFoundation finds and of the first 64 files in `Contents/MacOS`), names the system and processor it was made on and is written atomically. Only what the search reached is pruned: entries under a folder that could not be read, that was too deep or that a limit cut off stay. A file replaced by one of the same size and times goes unnoticed until `--rescan`.
- A plugin id two files offer is listed once, from the first, and the other file is named.
- The C types the next parts need (plugin, host, process, events, audio and note ports), checked against the headers' sizes and offsets on 64-bit systems.
- Tests: a CLAP plugin built in the repository (`examples/clap_fixture.rs`) loaded under names that make it crash in `init` or `deinit`, hang, refuse, write megabytes to its output, offer no factory or start a helper process; its descriptors and roles, a nameless descriptor passed over, a file that is not a library, the cache reused, read again after a change and pruned only where the search reached, the time budget (also cutting a child short without blaming the plugin), the helper ended with its scan, duplicate ids, `tinvo plugins` through `CLAP_PATH`, the search's order, limits and links, and the cache's and the result's formats (a result with anything after its end refused).

Known limits: a cache that would be over 16 MiB is not written, so such an installation is read again by each scan; cached entries under a folder one root could not search to the end are kept even when another root searches that folder fully; and a process a plugin starts outside its child's process group or job outlives the scan.

#### 12b: Playing plugins ✅ (0.26.0)

In parts, each merged on its own.

- **The host** ✅: `clap::host`. A `PluginHost` on the program's main thread (as CLAP asks on Windows and macOS) loads plugin files and creates, initialises and activates instances there, at a sample rate and a largest call (at most 8192 frames). A file is initialised once while any instance uses it (the system's library handle tells a second path to a loaded file from another file) and deinitialised after its last instance is destroyed; the library stays loaded, since a plugin can leave code behind that runs on thread exit (a Rust or C++ plugin's thread-local destructors), and a file can be initialised again. An instance's main thread is its host's: `clap.thread-check` says so for that thread only, `restart` refuses any other, and an instance dropped on another thread is never destroyed there: it goes back to its host, which destroys it at its next poll, and is leaked only if the host is gone. It reads its audio ports: every port gets buffers, the main ports (port 0, flagged main) are the ones fed and heard and must be mono or stereo, an effect needs a main input and an instrument a note input taking CLAP's notes or MIDI 1. It processes interleaved stereo through planar buffers, splitting calls longer than its largest, starts processing on its first call and stops before it leaves the audio thread, and is deactivated and destroyed when dropped. Notes go to the next call with room kept for every held note's release (512 events), one note a key for MIDI 1. The poll serves `request_callback` and reports `request_restart` and failures; `restart` resets the plugin's voices (CLAP's `reset`), deactivates, reads the ports again and activates; `request_process` is kept for the engine. A failed start or process call passes an effect's input or silences an instrument until a restart; output that is not a finite number is silenced. When rendering a file `clap.render` asks for offline processing, and a plugin that needs real time is refused. Processing allocates nothing: on macOS, where a thread's first use of thread-local storage allocates, what each thread keeps of its own lives in pthread keys' slots, and `prepare_audio_thread` is needed only on a thread that runs before any host exists; the allocation test plays an instrument and an effect. The test plugin's instances check every call against CLAP's rules (the thread the host reports, one OS thread for main-thread calls, the lifecycle's order, buffer counts and masks, frame counts, steady time, event order and kinds) and count what breaks one.
- **In the engine** ✅: plugin instruments and inserts by a `PluginRef` (an id interned once for the process), each holding a `PluginSlot`; instances attached by command (`SetPlugin`, or inside a `SetSlot` insert), one that does not fit its slot sent straight back, and every instance let go of (replaced, its track removed, its insert retired, counted as two with its insert) returned through the release queue, stopped on the audio thread first; a plugin track stereo through its strip, with a balance law and its key send hearing the middle; notes at their frame, and a new instance never hearing the last one's releases; restarts by token (`Lend`, `Return` to the slot that lent it); an instance dropped off its host's thread when the stream ends going home through the host's inbox, destroyed by the next poll; the allocation test with the engine playing, lending and removing plugins.
- **In projects and export** ✅: project format 16 (`track plugin <id>`, `insert <slot> plugin <id>`), an id kept as written whether or not it is installed; `Project::plugin_places`; `SetSlotPlugin`, an instance for the plugin insert a slot holds; renders through offline instances of their own (`render::PluginSource`), made before the first frame, their restarts served between chunks, and a plugin that cannot load or fails named with the reason; `tinvo render` reading the scan's catalog when the project names a plugin and writing no file unless every one, the stems too, rendered.
- **In the editor** ✅: a background scan; instances at the device's rate after each rebuild and when the catalog arrives; restarts and failures served from the editor's loop; a picker from the catalog, by role, for tracks and insert slots; missing plugins named when the project opens and kept in a list that stays; export from the editor through an offline host of its own.

#### 12c: Parameters, latency and state ✅ (0.26.0)

- `clap.params` in the panel and on automation lanes, by stable id; `clap.latency` in the latency compensation; `clap.state` in the project file. A missing plugin: an insert passes its input through and an instrument stays silent; both keep their state and are named when the project opens.

In parts, each merged on its own.

- **The host** ✅: an instance reads its parameters each time it is activated and its latency right after; parameter changes by stable id go to the next process call in room of their own (or to `flush` on the audio thread when it is skipped, but those that need processing), acknowledged by serial once the plugin has them or they went with their parameter, and dirty the state; a state of at most 16 MiB is loaded before the first activation and saved through a `PluginHandle`, which also reads values and text while the plugin plays elsewhere; the plugin's own parameter changes, its rescans (its names read again for the handles), its clears and a dirty state are reported by the poll. The test plugin's gain and sine gained parameters, a state, and modes for latency, a meter, an oversized state, failing saves and loads and marking its state dirty.
- **In the engine** ✅: `SetPluginParam` by place, plugin and stable id, held off by a lane on that parameter and kept for an instance lent out; lanes on a plugin's parameters wherever it plays from the track (its instrument, its chain, a group's or the master's), with a base left in force where no point is, and sent again to an instance that comes or comes back; plugin latency, an instrument's and an insert's, in the compensation, half a second at most for an instance and made up for on every path, with `Shared::latency_over` saying when a path is later still; a plugin insert's dry path as late as its output, so a bypass or a failed call moves nothing in time; renders lined up before their first frame, failing if a restart changes a plugin's latency. The project model takes plugin lanes and their base (`SetLaneBase`), checked against the plugin there; the test plugin's sine comes late too in its latency mode, and a mode restarts it later.
- **State in projects** ✅: project format 17 with each plugin's state (base64, at most 16 MiB), plugin lanes and their base; the editor saves its instances' states into the project before a save, an autosave, an export, a new engine and an edit that may remove a plugin, keeps them through undo and redo, starts each instance from its state (at its defaults, said, if it cannot load it) and marks a changed state unsaved; renders start from the states too.
- **In the editor** ✅: a plugin's parameters in the panel (a plugin track's on the instrument page, an insert's on a plugin page from its slot), by the instance's handle: names, text and ranges from the plugin, hidden ones left out, read-only ones not set; a change sent by stable id with a serial, shown until the plugin has it, an undo step of its own and an unsaved change, or a lane's base where a lane holds it; lanes on them made from the panel, played in a straight line and shown in the plugin's words; lanes on a parameter the plugin clears removed as an undo step; plugins later together than the engine makes up for named.

#### 12d: The plugin's window ✅ (0.27.0)

- `clap.gui` as a floating window, or embedded in a window TINVO opens for it, its platform's event loop running alongside the terminal's.

In parts, each merged on its own.

- **The host** ✅: `clap::host::gui`. A handle opens an instance's window on the main thread for a window API (`win32`, `x11`, `cocoa`): floating where the plugin offers it (titled, sized by the plugin), otherwise made, scaled where the system sizes in pixels, sized, put in a `ParentWindow` the caller makes and shown, and destroyed again if any step fails. A drag goes through `adjust_size` and `set_size` and the window's own report of it is not sent back. The plugin's `request_resize` is answered by the window on the main thread and queued from elsewhere, a size the window refuses sending the plugin back to its last one; its show, hide and resize-hint requests, the latest winning, and its `closed` are served by the host's poll, a window it destroyed itself only acknowledged, even from inside another call. `clap.timer-support` (at least 15 ms) and, on Unix, `clap.posix-fd-support`, for instances made to play in real time: `next_timer`, `fds` and `dispatch` for the main loop, late timers run once. A window still open is closed before its instance is deactivated, on the host's thread wherever the instance is dropped, and what the plugin left registered goes with it.
- **The main loop and Windows** ✅: the terminal's waits also wake for the thread's window messages (Windows) and the plugins' descriptors (Linux, macOS) and end by the next plugin timer; timers, descriptors and window messages served after each frame's input, and during an export. `clap::window`: on Windows a per-monitor DPI aware window of TINVO's for each embedded plugin window, closed by the user through the plugin's window first, its size offered to the plugin.
- **X11** ✅: `clap::window` on Linux: a window of TINVO's for each embedded plugin window through Xlib loaded at run time, closed through `WM_DELETE_WINDOW`, its sizes offered to the plugin, the axes the plugin cannot resize held by size hints, the plugin told `Xft.dpi`'s scale, and its connection waited on with the terminal. Tested under Xvfb in CI.
- **macOS** ✅: `clap::window` on macOS: an `NSWindow` of TINVO's for each embedded plugin window through the Objective-C runtime, AppKit loaded with the first, on the main thread as an accessory application; closed through the plugin's window first, sized in points, live resizes served as they come, and AppKit's events handed out at least once a frame while a window is open. Tested on the main thread in CI.

Known limits: on Windows and macOS, while the user moves or sizes a plugin's window, the system runs its own loop, so the terminal and the plugins' timers wait until it ends (the drag's sizes reach the plugin as they come, and audio plays on); on Windows, a plugin window narrower than a captioned window's least width cannot be embedded, and a plugin that handles its window's paint without validating it keeps the main thread busy; on X11, losing the connection to the display ends TINVO, as Xlib does for any client.
- **In the editor** ✅: `c` (`plugin_window`) on a plugin track's instrument page, a plugin's page or its slot on the inserts page opens the playing instance's window, titled with the plugin and its place, and closes it again; one window for each instance. It closes when its instance no longer plays for its place (the plugin removed, replaced or picked again, an instance gone, or one given up on after failing twice), when the device is lost or a new engine comes, and at quit. Before a lost, switched or failed device's instances are destroyed, a change of state no poll has reported yet is read from their handles, so it is still an unsaved change, and their states are kept. The title of the plugin's page, and of a plugin track's instrument page, says `window open`; the bottom line names the key there and on the inserts page. A change made in the window reaches the panel, which reads the plugin's values, and the unsaved mark, through the plugin's output events and its dirty state. The plugin's requests of its window are kept in atomics, so it may make them from the audio thread without waiting, and a close it asks for during a poll still wins over a show. A settings file that gave `c` to another action of the panel still loads, with the new key left unbound (`keymap::LATE`). Tests: the editor opening and closing the test plugin's window from each page (on Windows, and on X11 under Xvfb; on macOS, where a test's thread is not the main one, the refusal said), a page with no plugin or one not playing saying so, a knob turned in the window shown on the panel and marking the project unsaved once the plugin has played, the window closed with its plugin removed or picked again, given up on and with the device lost (mutation-checked), a change of state just before a new engine or a lost device kept as unsaved (mutation-checked), and a settings file from before the key loading.

Known limits of the editor's windows: a change made in a plugin's window is the plugin's, saved with its state, and not an undo step; where a lane holds the parameter, the panel shows the lane's base and the lane's points play over the change; and the windows open when the device was lost or another engine came are not opened again for the new instances.

### Phase 13: Sampled instruments (medium) ✅ (0.28.0)

For realistic instruments without plugins, and for chopping breaks (drum and bass, jungle, hip-hop).
- Key zones and velocity layers in a sampler track, round robin and release samples.
- SFZ import of the common opcodes (`sample`, `lokey`/`hikey`, `pitch_keycenter`, `lovel`/`hivel`, `seq_length`/`seq_position`, loop points, the amplitude envelope). Opcodes it does not use are listed, not dropped silently.
- A slicer: a sample cut at its transients or into equal parts, one slice per note.
- 16 voices per sampler. Samples stay in memory as today.
- Tests: zone and layer choice, round-robin order, the SFZ reader fuzzed, slice points on a synthetic signal.

It comes in three parts, each merged on its own, and ships whole with 0.28.0, which freezes project format 19 (format 18 was never released).

#### 13a: Zones ✅ (0.28.0)

- A sampler track plays its sample over the whole keyboard, as before, and zones: a sample file each, with a key range, a velocity range, a root key (or the track's root note), a round-robin position and cycle, attack or release, its own tuning in cents and gain in dB, and the frames of the file it plays. A note plays every attack zone it reaches, so zones of one range layer, and a zone that takes turns plays the notes that reach it in its turn, counted per zone as SFZ does. A key coming up plays its release zones, at the velocity it went down with, once to their end whatever the mode; stopping lets keys go without them.
- The track's parameters apply to every zone: tune and fine on top of each zone's tuning, and the start, end and loop points as fractions of each zone's part of its file.
- 16 voices, up to 1024 zones a track besides its sample. One note starts at most 16 voices, and the layers of one note, or of one key coming up, never take each other's voices; turns are counted for every note that reaches a zone, voices or not. Each key knows its zones, so a note looks only at those.
- The engine gets each track's zones in one `Arc`, built on the UI thread from the loaded files and handed back to it to free, as the sample was.
- Project format 18: `zone` lines in a sampler track, paths relative to the project as the sample's. Each file a track plays loads once and counts once against the project's budget; a zone whose file is missing is left out, the panel's title counts the zones and the missing files, and a WAV export waits for every file.

#### 13b: SFZ import ✅ (0.28.0)

- `o` on a sampler track takes an `.sfz` file: its regions become the track's zones, in place of its sample and zones, and its amplitude envelope sets the track's attack, decay, sustain and release, all as one undo step. Each file loads once and counts against the project's budget; a region whose file does not load is left out and named.
- The reader takes the headers (`<control>` with `default_path`, `note_offset` and `octave_offset`, `<global>`, `<master>`, `<group>`, `<region>`, each region taking the opcodes above it), `#define` and `#include`, comments, values with spaces, note names, and the opcodes a zone plays: keys, velocities, root key, round robin in turn (`seq_length`/`seq_position`) and at random (`lorand`/`hirand`), release triggers, tuning, gain, the part of the file, the loop mode and its points, and the envelope. A keyswitched instrument keeps one articulation (`sw_default`'s, or the first region's, among the keys from `sw_lokey` to `sw_hikey`). It never fails on what it does not know: unused opcodes and other headers are listed, skipped regions counted with the reason, and `tinvo sfz` prints it all.
- Zones gained what SFZ needs: a random range, drawn by a seeded generator that starts over with playback (a key's release zones draw as its attack did), and a loop mode of their own (no loop, one-shot, continuous, or sustained while the key is held) with loop points or the loop the file gives; a loop asked for by name with no points in the SFZ or the file loops the whole file, and a region whose loop points cannot play plays once. WAV files' `smpl` chunks give their forward loop and root key. A loop's interpolation reads its start past its end, and once round its end before its start.
- Project format 19: a zone line ends with its random range and mode, before the path.
- `tinvo fuzz sfz`: the reader's result is the same each time, every region can play as read, and against a file it settles or starts past the file's end; the settled zones save and load back equal, and the first 32 play without a non-finite sample. CI runs it with the other targets.
- The undo history keeps the samples no track plays only up to the project's budget, and reads the rest again when an undo or redo needs them, as an import can replace hundreds of megabytes at once.

#### 13c: The slicer ✅ (0.28.0)

- `%` on a sampler track cuts its sample into slices, in place of its sample and zones, each a zone on its own key from C2 up, played at the pitch it was recorded at (the track's tune, fine tune and bend still apply) and in the track's mode (gate or one-shot, the envelope, the filter), as one undo step. Of a sample, the part between the track's start and end points is cut, and the points then take in each whole slice; zones all of one file, as slicing leaves them, are cut again from the whole file, so a different cut is a keystroke away. It is refused while a lane has a point for the start or end point, in any pattern, as it would trim every slice. A settings file that gave `%` to another action keeps it.
- At its transients (`t`, or `t` and a whole-number sensitivity from 0 to 100): the energy of the signal's first difference, in hops of 5 ms, rising 18 dB (sensitivity 0) down to 3 dB (100) over the loudest hop of the 50 ms before (a whole cycle of a 20 Hz tone, so a low note's waveform is no attack), at a level against the loudest hop from -12 dB down to -40 dB. A cut is added only 40 ms or more from every other cut and from the ends of what is cut (so a part shorter than that stays one slice): the loudest candidates are taken first. Each starts where its changes begin, looking back from the hop's largest change, then at a zero crossing if one is within a millisecond before, on the side nearer zero; past 64, the rest are left out. The analysis keeps only the last 50 ms and a bounded list of candidates, so any length or rate takes little memory; on a 6-minute file it adds about 30 ms to the load.
- Into 2 to 64 equal parts, exact to the frame.
- `tinvo slice SAMPLE.wav [N | t [S]]` prints the cut: each slice's key, start, length and time.
- Tests: slice points on synthetic hits, a drum loop with ghost notes, decaying, held and low tones, a faint hit over a tail, a faint change before an attack, zero crossings, frames that are not numbers, a very low rate, too many hits, and the editor's slicing, its refusals and its undo. `tinvo fuzz wav` slices every sample that loads over a part of it, both ways.

### Phase 14: Synth engines (small to medium) ✅ (0.29.0)

For FM keys, basses and bells, and wavetable leads and pads.
- A 4-operator FM synth: the common algorithms, feedback and an envelope per operator.
- Wavetable oscillators in the synth: built-in tables and a WAV file loaded as one, the position moved by the LFO and envelopes, band-limited across the keyboard.
- Tests: FM sidebands against their Bessel amplitudes, wavetable aliasing measured by FFT like the existing oscillators.

It comes in two parts, each merged on its own, and ships whole with 0.29.0, which freezes project format 20.

#### 14a: FM ✅ (0.29.0)

- A new instrument, `fm` (`a` `9`): 8 voices of four sine operators, phase-modulating one another along one of eight algorithms (`4>3>2>1` to `4 3 2 1`), operator 4 fed back the average of its last two outputs. Each operator has a ratio, a detune, a level (a heard operator's gain, or a modulator's index: up to 4π radians), a velocity response and an ADSR of its own; a voice lasts as long as its heard operators.
- Instead of oversampling, which would add latency: per 64-frame chunk, Carson's rule bounds each operator's spectrum from its sounding modulators' indices, peak frequencies and spectra, and the modulation is scaled until that estimate stays under 45 % of the sample rate; faint sidebands beyond it can remain. High notes modulated hard, through one modulator, a chain or a cascade, fold back less than −30 dB of their power instead of −1 to −20 dB.
- 35 parameters, so an instrument may now have up to 40. Project format 20 (`track fm`).
- Tests: sidebands against `J_n(β)`, every algorithm's routing, feedback's harmonics and its staying periodic at full, velocity per operator, envelopes per operator, stealing, callback sizes, and the folding with and without the limit. A bench scenario of 8 voices in a chain with feedback.

#### 14b: Wavetables ✅ (0.29.0)

- In the existing synth, not a new kind: a fifth wave, `table`, for either oscillator, and four parameters after the synth's others (the table, its position, LFO and filter envelope to the position) plus a mod wheel target for the position.
- Four built-in tables (`basic`, `pulse`, `sync`, `fold`), built once before audio starts, since instruments are made on the audio thread. A WAV file loaded with `o` on a synth track is the track's `sample`, so it is saved, undone, budgeted, reloaded when missing and exported as a sampler's is; it reaches the engine as a table built off the audio thread and goes back to be freed there. It needs at least two samples. Shorter than 2048 it is one cycle; otherwise whole frames of 2048 (a part frame at the end left out), at most 64, spread evenly over it.
- Band-limited by mipmaps: each frame is kept at ten bandwidths (harmonics 1, 2, 4 ... 512), each inverse-transformed (`dsp::fft` gained the inverse) with at least 32 samples per period of its top harmonic, so the images linear interpolation makes stay faint. A pitch blends the two fullest levels below Nyquist, the fuller fading in as it falls, so the brightness never steps. At a steady position, what folds back from the built-in tables stays 70 dB down, measured at 48 kHz at eight pitches from 27 Hz to 11 kHz and three positions of each table; at a high note the PolyBLEP saw folds back over 20 dB more. A position moved fast modulates the partials, and sidebands of those near Nyquist can fold back.
- Tests: the levels' harmonics and their blending as a pitch glides, a cycle's harmonics read back, frames spread over a long file, the built-in tables' spectra against their shapes, the aliasing of every table from low to high, the position moved by the LFO, the envelope and the wheel, a file table given and handed back, the load as one undo step, the format and fuzzing of tables from every WAV file the fuzzer reads. A bench scenario of both oscillators in unison on a table, the position and pitch swept.

### Phase 15: Audio tracks (large)

For vocals, guitars and other live instruments: pop, rock, singer-songwriter.
- Audio tracks holding clips placed on the song (at a bar and step), apart from patterns, with gain, fades and trims that leave the file untouched.
- Recording onto an audio track while the song plays, with a count-in, the round trip compensated (as `tinvo loopback` measures it), punch-in over a range, and takes kept per range to build a comp from.
- Long clips streamed from disk: a reader thread fills buffers ahead of the audio thread, which never touches a file or allocates.
- A clip view: clips as blocks along the song, one row per track, with a waveform overview in block characters.
- Export and stems include audio tracks. A new project format (clip paths stored like sample paths).
- Not in this phase: time-stretching and pitch correction.
- Tests: clips on their exact frames, a slow reader making a reported dropout rather than blocking the audio thread, latency compensation measured the way the loopback check measures it, comp edits as undo steps.

## Known limitations of the foundation

- Linux and macOS backends have run only on virtual devices (a JACK server, PipeWire, BlackHole and the macOS runner's devices), not on real audio hardware.
- CI runs on hosted Windows, Linux and macOS machines, which have no sound card. JACK (jackd and PipeWire), CoreAudio (BlackHole and the runner's virtual devices) and WASAPI (VB-CABLE, output and capture) are played through there; ALSA is only compiled, linted and opened on its `null` device.
