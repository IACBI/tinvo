# TINVO Architecture

TINVO is a terminal digital audio workstation written in Rust with **no third-party crates**: only `std`, its own DSP, and direct FFI to the operating system's audio and console APIs.

Author: 𝓐.𝓒.𝓑

## Design principles

1. **The audio thread is sacred.** After the engine is built, `Engine::process` never allocates, locks, blocks or does I/O. Every buffer, voice and track slot is sized when the engine is created.
2. **One source of truth, one way to change it.** Every change to the document is an `Edit` value. The UI applies it to the `Project`, gets back the edit *as actually applied* (with values clamped), and sends that same value to the engine. The model and the engine can't drift apart, because they run the same operations, and a test proves the edits rebuild the project.
3. **Offline equals real time.** WAV export drives the exact `Engine` used for playback, so an export matches playback down to the bit (the tests check that rendering is deterministic and independent of buffer size).
4. **Correct before clever.** Floating-point state is flushed explicitly instead of switching the CPU to flush-to-zero mode, because changing the floating-point environment behind the compiler is unsound in Rust. Timing comes from a fixed origin rather than being accumulated, so it does not drift.

## Layers

Each layer depends only on the ones below it.

```
 main.rs      CLI: editor, render, demo, probe, bench
 app          UI state machine (pure, unit-tested), drawing, main loop
 tui          raw terminal I/O, key decoding, diffing screen buffer
 audio        WASAPI / ALSA / CoreAudio backends behind one `open()`
 render       offline rendering (same Engine)
 project      document model, Edit validation, text file format
 engine       transport, tracks, mixer, command handling
 instrument   polyphonic synth, synthesized drums, parameter specs
 dsp          oscillators, envelopes, filters, smoothing, noise, FFT
 rt           wait-free SPSC queue, atomic f32
 wav          WAV encode (16/24-bit dithered, 32-bit float) and decode
```

## Threads and data flow

```
 ┌──────────── UI thread ────────────┐          ┌──────── audio thread ────────┐
 │ keys → App → Project::apply(edit) │  edits,  │ Engine::process(buffer)      │
 │                 │ applied edit    │ play/stop│   drain commands             │
 │                 └──► EngineLink ──┼──SPSC───►│   fire due note-offs/steps   │
 │                                   │  queue   │   render instruments         │
 │  meters, playhead, CPU load ◄─────┼─atomics──│   mix (gain/pan smoothing)   │
 └───────────────────────────────────┘          └──────────────┬───────────────┘
                                                               ▼
                                               backend converts f32 stereo to the
                                               device format (F32/I16/I24/I32)
```

- **Command queue** (`rt::spsc`): bounded, wait-free, cache-padded. If it is ever full, `EngineLink` keeps the remaining commands in order and retries every UI frame, so no edit is lost.
- **Telemetry** (`engine::Shared`): playhead step, per-track and master peaks (lock-free `fetch_max` on the bit patterns of non-negative floats), and smoothed callback load.
- **Start-up**: the backend opens the device first, then calls a factory on the audio thread with the real sample rate. That is when the engine is allocated, before the stream starts.

## Engine

- **Tracks**: up to 16 slots, each with an `Instrument`, a 64-step pattern, fader/pan/mute/solo, and smoothed left/right gains (5 ms time constant, so changes never click).
- **Transport**: 16th-note steps. The frame of step *n* is `origin + round((n + swing(n) − origin_step) × samples_per_step)`, where `swing(n)` is `percent/50 − 1` for odd *n* and 0 otherwise. A tempo change moves the origin to the current position, so the playhead stays continuous.
- **Steps and hits**: a step is a note or a chord of up to 4 notes (stored as intervals above the lowest note) with velocity, length, chance, ratchet and a micro-timing offset. When the transport reaches step *n* it queues hits, never plays them directly: step *n*'s on-time and late hits, and step *n+1*'s early ones. An early hit is at most half a straight step early, and the shortest swung step is half a straight step, so an early hit is never due before it is queued. Every queued hit fires through the same path, after note-offs and the choke check, at its exact frame.
- **Chance**: rolled per step when its hits are queued, from a seeded generator that restarts on Play, so playback and export always produce the same sequence of hits from the start. Ratchets split the (swung) step span evenly, starting at the shifted position.
- **Choke**: when a closed hat fires, open hats on all tracks fade out over 12 ms.
- **Sample-accurate events**: each 64-frame block is split at every step boundary and note-off inside it. Note-offs fire before note-ons that land on the same frame.
- **Voice identity**: `note_on` returns a `VoiceId`, and note-offs target that exact voice, so retriggering a pitch never cuts off the wrong note.
- **Pan law**: constant power, −3 dB at centre. The bottom of the fader range (−60 dB) means silence.

## DSP

| Block | Method |
|---|---|
| Oscillators | Saw and square with PolyBLEP, triangle with PolyBLAMP, sine; phase kept in f64 |
| Filter | TPT/ZDF state-variable filter (LP, unity-peak BP, HP); stable under per-sample modulation |
| Envelopes | Linear attack, exponential decay/release (times are to −60 dB); retriggers start from the current level |
| Synth modulation | One free-running LFO per synth, computed per 64-sample chunk, to pitch, pulse width, cutoff and level; glide is exponential in pitch (99 % of the interval in the glide time); pitch is recomputed per sample only while gliding or under LFO pitch modulation |
| Unison | Up to 4 copies of each oscillator spread evenly across ±detune, started at random phases and summed at 1/√n so the level stays steady |
| Drums | Kick (pitch-swept sine plus click), snare (two tones plus filtered noise), hats (six band-limited squares, metallic), clap (three bursts plus tail); retriggers cross-fade the old tail over 1 ms |
| Export | TPDF dither (±1 LSB) for 16/24-bit; clipped samples are counted and reported |

## Platform layer

| | Audio | Terminal |
|---|---|---|
| Windows | WASAPI shared, event-driven, MMCSS "Pro Audio" priority; COM through hand-written vtables | Console API: `ReadConsoleInputW` (exact key codes), VT output |
| Linux | ALSA `default` PCM, float, 48 kHz (needs `libasound` at link time) | termios via `cfmakeraw`, `poll`, VT/xterm key decoding |
| macOS | AudioQueue (AudioToolbox), float, 48 kHz | termios, as on Linux |

## Project file (`.tvo`)

Line-based UTF-8 text with a version header (`tinvo 3`). Format 2 added `swing` and per-step chance and ratchet; format 3 adds the micro-timing offset and the chord (`-` or intervals such as `3,7,10`). Older files still load, with the newer fields at their neutral values. A chord must be written in canonical form (ascending, no duplicates, in MIDI range), so every file has exactly one spelling. Parameters are stored by a stable `id`, not by index, so instruments can gain parameters later without breaking old files. Floats use Rust's shortest round-trip formatting, so save → load is lossless. Loading validates every value; it reports errors with line numbers instead of silently clamping. Saves are atomic: the file is written to a temporary file and then renamed over the target.

## Undo

`Project::inverse` returns, for any valid edit, the edits that restore the state before it. The UI records each change as an (undo, redo) pair of edit lists, and replays them through the same path as normal edits, so the engine stays in sync. Deleting a track inverts to inserting it back at its index and restoring every parameter and step. Track names are not edits, so each history entry stores the names before and after.

## Verification

- 124 unit tests cover: DSP accuracy (pitch, Butterworth −3 dB point, roll-off, measured alias reduction via FFT), envelope timing, queue ordering under a 1M-item two-thread stress test, sample-accurate step timing, render invariance across buffer sizes (1 to 4096 frames), mute/solo/pan, note lengths, smoothing, lossless file round trips, WAV round trips, key decoding, screen diffing, swing/chance/ratchet/choke timing, glide and LFO, chords and micro-timing (including early hits under maximum swing), pulse width (measured second harmonic), noise and unison, reading format 1 and 2 files, edit inversion for every edit kind, undo/redo, and UI behaviour (including that the engine receives exactly the edits the project applied, through undo and redo).
- `tinvo probe` exercises the real device. `tinvo bench` measures rendering cost for the demo and for a worst-case synth track (about 1 % and 5 % of one core on a current desktop CPU).
