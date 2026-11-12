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

## Phase 5b: MIDI input ✅ (0.11.0, this release)

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

## Phase 5c–5d: More I/O

- 5c: Audio input and recording.
- 5d: CoreAudio HAL unit; JACK/PipeWire native paths; the ALSA sequencer.

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
