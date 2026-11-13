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
- Only notes are used: no sustain pedal, pitch bend, controllers or MIDI channels (every channel plays the selected track). There is no live recording while the pattern plays.
- On Linux, rawmidi sees hardware and USB ports only, not software clients of the ALSA sequencer.
- A MIDI input that is unplugged stays silent until another one is picked with `M`; it is not reopened by itself.
- If more than 256 notes arrive within one audio callback, the extra ones are dropped (a dropped release would leave a note held until the key is pressed again).

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

- Unsaved edits are written every 5 seconds, atomically, next to the project (or in the state directory for an untitled one); saving or quitting normally removes the autosave.
- At start-up an autosave left by a session that did not end normally is offered for recovery, discarded, or kept for next time.
- A terminal error or a panic writes the autosave at once.

Known limits:
- Up to 5 seconds of edits can be lost if the process is killed. Within that window nothing is written, so a crash costs at most that much.
- Two TINVO sessions on the same project, or two untitled ones, share one autosave file: the one that writes last wins, and a session started while another runs is offered the running one's autosave. Nothing locks the project file either.
- The autosave is renamed into place but its directory is not synced afterwards, so after a power failure (not a crash of TINVO) some file systems may come back with the previous autosave or none.
- The flow was run end to end on Linux in a pseudo-terminal: edits, `kill -9`, the recovery question, recovering and saving (the saved file equals the autosave byte for byte), and discarding. The writes on a terminal error and on a panic are covered by unit tests of the pieces, not by a run that actually fails mid-session. The recovery screen has not been tried by hand in a Windows console.

## Phase 6b: Fuzzing ✅ (0.15.0)

- `tinvo fuzz` for project files, WAV files and edit scripts, with invariants that go past "does not panic": save round trips, the engine's view of a project, undo, the sample loader against the decoder, and finite output when an accepted input is played.
- Runs in CI for 60 seconds per target on every push, and briefly in `cargo test`.
- It found four problems on its first runs, all fixed: a panic on the audio thread when a sampler's start point is after its end point, infinite output from float WAV files with huge values, sample paths that a project file could not give back, and a wrong promise about the range of 32-bit WAV samples.

Known limits:
- It is not coverage-guided. That needs compiler instrumentation and a fuzzing runtime (`cargo fuzz` uses libFuzzer, a nightly toolchain and crates). Instead an input is kept when it shows a feature not seen before: a new error, or a new kind of line, chunk or edit in an accepted input. Mutations that know the formats make up for part of the difference; a coverage-guided fuzzer would still reach some branches sooner.
- Rendering covers the first half second of an accepted project, without its samples (the files are not there); sample rendering uses fixed projects. One accepted input in 16 is rendered by default.
- Edit scripts stop after 64 edits, since each edit clones the project twice to check its undo.
- Before 0.15.0 it ran for about 24 million project inputs, 4.7 million WAV files and 200,000 edit scripts (two 10-minute runs per target on one machine). Finding nothing more in that time does not show that no bug is left.

## Phase 6c: Performance budgets ✅ (0.16.0)

- A test that counts allocations on the audio path (`tests/realtime.rs`): the engine while it plays, takes edits, cues, auditions, sample changes and live notes in callbacks of random sizes, plus the capture side of recording and the MIDI parser and mailboxes. Any allocation fails it with a backtrace.
- `tinvo bench` measures four scenarios (the demo, the demo with every strip stage on, a worst-case synth, a worst-case sampler) per callback of 128 frames: mean, 99th percentile and worst, as a share of the period. `--check` fails when a mean is over its scenario's budget; CI runs it on every platform.
- CI counts the instructions per second of audio of each scenario with cachegrind and fails when one is more than 10 % over `bench/instructions.txt`.

Known limits:
- The time budgets are loose (10 to 25 % of the period on average, against at most 6.2 % measured in two CI runs) because CI runners are shared, so a slowdown has to be large before they fail. The 99th percentile and the worst callback are printed but not enforced, since a shared machine's scheduler decides them.
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

## Phase 6f: Foundation limits ✅ (0.19.0, this release)

- A track removed while it sounds, or an added track undone, fades out over 10 ms instead of stopping dead. During the fade it plays on exactly as it would have, through its strip, fader and sends; then its sample is released.
- Drum parameters reach a hit that is still ringing: tune, the decays, click, tone and the clap's spread, whose bursts still to come keep to the new spacing. An open hat cut short by a closed one stays short when its decay grows.
- ALSA output and input open at a rate the device takes without resampling (ALSA's own resampler is a linear one): 48 kHz when it can, else 44.1 kHz, else the first common rate it takes. CoreAudio input runs at the device's nominal rate, as output already did.

Known limits:
- Each removed track fades in one of 16 slots. More than 16 removals within one fade cut short the fade with the fewest frames left; the editor removes one track per key press, so it does not come to that.
- ALSA's rate is chosen with resampling turned off. Where every rate passes that check, as with `null`, TINVO opens at 48 kHz as before, and a sound server behind ALSA converts if it runs at another rate. The choice was checked on plug devices over `null` with a fixed rate (44.1, 96 and 22.05 kHz, for playback and recording), not on real hardware.
- CoreAudio input at the device's rate has run on the macOS CI runner's BlackHole only.

## Known limitations of the foundation

- Linux and macOS backends have run only on virtual devices (a JACK server, PipeWire, BlackHole and the macOS runner's devices), not on real audio hardware.
- CI runs on hosted Windows, Linux and macOS machines, which have no sound card. JACK (jackd and PipeWire) and CoreAudio (BlackHole and the runner's virtual devices) are played through there; WASAPI and ALSA are only compiled, linted and listed.
