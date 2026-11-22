<a id="top"></a>
# TINVO

A keyboard-driven digital audio workstation for the terminal: zero dependencies, real-time safe, offline.

[![CI](https://github.com/IACBI/tinvo/actions/workflows/ci.yml/badge.svg)](https://github.com/IACBI/tinvo/actions/workflows/ci.yml)
![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange)
![Dependencies: 0](https://img.shields.io/badge/dependencies-0-brightgreen)
![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20Linux%20%7C%20macOS-blue)
[![License: MIT](https://img.shields.io/badge/license-MIT-lightgrey)](LICENSE)

**Read this in:** [English](#english) · [Türkçe](#turkce)

---
<a id="english"></a>
## English

### Overview

TINVO is a step-sequencer DAW that runs in a terminal window. Everything is written in Rust on top of the standard library alone: the synthesizer and drums, the real-time engine, the WAV writer, and the audio backends (WASAPI, ALSA, JACK, CoreAudio) are called through hand-written FFI. There are no crates to download, so it builds offline. The release binary is a few megabytes (about 3 MB on Windows).

The audio callback never allocates or frees memory, takes a lock or blocks. WAV export runs the same engine as playback, so what you hear is what you export.

https://github.com/user-attachments/assets/46029a57-69c3-407c-a1af-21c34194486d

TINVO playing [`examples/showcase.tvo`](examples/showcase.tvo). The sound is that project's own `tinvo render` export, laid under the screen recording in sync; export and playback run the same engine. To hear it alone: [showcase.mp3](docs/media/showcase.mp3). To open it yourself: `tinvo examples/showcase.tvo`.

### Features

- 32 tracks, up to 64 steps per pattern, tempo 20–300 BPM, swing 50–75 %. Each pattern has its own step length (1/8, 1/8 triplets, 1/16, 1/16 triplets or 1/32) and time signature (such as 3/4, 6/8 or 7/8), which places its beat and bar lines. Song entries can set a tempo that holds or ramps to the next.
- Per step: a note or a chord of up to 4 notes, velocity, length, chance (1–100 %), ratchets (1–8 hits) and micro-timing (up to half a step early or late).
- A piano roll for melodic tracks.
- 16 patterns (A–P) and a song: a chain of up to 256 patterns with a loop region, and named markers that divide it into sections. In any entry a track can play a pattern of its own or rest. While a pattern plays on its own, the one you pick takes over when it ends.
- Automation: up to 4 lanes per track for volume, pan or any instrument or strip parameter, holding or ramping between points set per step.
- A channel strip on every track: high-pass, three-band EQ, compressor and antialiased drive.
- Up to 8 group buses. A track goes to a group or to the master, and a group has a stereo channel strip of its own (its compressor ducks both sides together), volume, balance, mute, sends and automation.
- Insert effects: 8 slots on every track, group and the master, 64 in a project, each holding an EQ, compressor, drive, delay, reverb, chorus, multimode filter, phaser, bit crusher or gate, in any order. Every slot can be bypassed and automated, and putting an effect in, moving, bypassing or removing it crossfades instead of clicking.
- The gate looks 1.5 ms ahead, and every other path is delayed to match, so tracks stay in time with each other, in exports and stems too.
- Sends from every track to a tempo-synced delay, an algorithmic reverb and a chorus, sidechain compression through a key bus, and a true-peak limiter on the master. Bus and master settings can be automated.
- Gain-reduction and loudness meters (momentary, short-term, integrated) while playing.
- 8-voice subtractive synth with two band-limited oscillators (pulse width, unison with detune), noise, a state-variable filter, two envelopes, glide, an LFO (to pitch, pulse width, cutoff and level; free in Hz or synced to the tempo, 1/32 to four whole notes), and velocity → cutoff.
- Synthesized kick, snare, closed and open hi-hat, and clap. The closed hat chokes the open hat.
- A 16-voice sampler that plays WAV files with windowed-sinc interpolation: root note and tuning, start/end points, loop, gate or one-shot, ADSR and a filter. Besides its sample, a sampler track can lay files over the keyboard as zones, written as `zone` lines in the project file or imported from an SFZ instrument with `o`: key ranges, velocity layers, round robin in turn or at random, release samples, and a loop mode of each zone's own. A WAV file's own loop and root key (its `smpl` chunk) are read. `%` slices a track's sample, at its transients or into equal parts, into zones of one slice per key from C2 that take the place of its sample and zones: a break to play from the keyboard or the grid. Zones that all play one file, as slicing leaves them, are cut again from the whole file.
- Mixer with click-free volume, pan, mute and solo, plus peak meters.
- Undo/redo with a 1000-step history.
- Text `.tvo` project files that survive a save/load round trip exactly. Saves are atomic, and older format versions still load.
- Autosave every 5 seconds while there are unsaved edits. If TINVO crashes or is killed, the next start offers to recover them.
- A project is open in one TINVO session at a time: a second one is refused with the process that has it, and a crash never leaves it locked.
- The editor's keys, colours and mouse can be changed on a settings page (`F2`) that applies them at once, or in the settings file it writes to. A light theme is built in.
- Mouse support: click steps, tracks, notes and parameters, drag to fill steps or stretch a note, turn the wheel over a value to change it.
- WAV export at 16 or 24 bit (dithered) or 32-bit float: the song, one pattern, the loop region or the part between two markers, optionally as stems: one per track, and one per group for the tracks in groups. Each export reports its loudness (LUFS, BS.1770), true peak and any clipping.
- Standard MIDI Files: export the song, a pattern or the loop region as notes for another program, and open a MIDI file as a new project.
- MIDI output: any track can play an external synth or another program instead of sounding, in time with the audio, and MIDI clock keeps other gear on TINVO's tempo.

The roadmap is in [docs/ROADMAP.md](docs/ROADMAP.md) and the design is described in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

### Requirements

- Rust 1.85 or newer.
- Linux: the ALSA development package (`libasound2-dev` on Debian/Ubuntu, `alsa-lib-devel` on Fedora). JACK is optional: TINVO uses it when a server (jackd, or PipeWire's JACK) is running and you pick it.
- Windows 10 or later, or macOS 11 or later. Nothing extra is needed.
- A terminal of at least 64×16 characters with UTF-8 and ANSI colours (Windows Terminal, any modern Linux or macOS terminal).

### Installation

```bash
git clone https://github.com/IACBI/tinvo.git
cd tinvo
cargo install --path .
```

To build without installing, run `cargo build --release`; the binary is `target/release/tinvo`.

### Usage

```bash
tinvo                     # open the editor with the demo beat
tinvo song.tvo            # open a project, or start a new one at that path
tinvo render song.tvo song.wav --format 24 --loops 4
tinvo render song.tvo beat.wav --pattern B --stems
tinvo render song.tvo chorus.wav --from chorus --loops 1   # the section from marker "chorus" to the next
tinvo render song.tvo song.mid    # the song's notes as a Standard MIDI File
tinvo import song.mid song.tvo    # a MIDI file as a new project (or open it: tinvo song.mid)
tinvo demo demo.tvo       # write the demo project to a file
tinvo probe 3             # check the audio device for 3 seconds
tinvo devices             # list the output devices
tinvo --device usb song.tvo        # play on the device whose name contains "usb"
tinvo --device jack song.tvo       # Linux: play through JACK or PipeWire's JACK
tinvo probe --device realtek --low-latency 3
tinvo loopback --input realtek     # play sweeps and hear them back: checks both devices, measures the round trip
tinvo midi                # list the MIDI inputs and outputs
tinvo --midi keystation song.tvo   # play from that MIDI keyboard
tinvo --midi-out "synth" song.tvo  # send MIDI-out tracks and the clock there
tinvo inputs              # list the audio input devices
tinvo record take.wav 10  # record 10 seconds from the default input
tinvo plugins             # list the CLAP plugins installed (--rescan reads every file again)
tinvo bench               # measure rendering speed
tinvo bench --check       # fail if a scenario is over its time budget
tinvo fuzz wav --seconds 600       # fuzz the WAV reader for 10 minutes
tinvo sfz piano.sfz       # what the sampler plays of an SFZ instrument, and what it leaves out
tinvo slice break.wav     # where the slicer cuts a sample (t 80 for fainter hits, 16 for equal parts)
tinvo settings            # check the settings file and print every key and colour
tinvo --settings my.conf song.tvo  # use the keys and colours in my.conf
```

The editor stays on the system default unless told otherwise, follows it when it changes, and reconnects by itself if the device disappears. It also opens the first MIDI input it finds: notes play the selected track, and with step entry on (`R`) they write steps at the cursor. The sustain pedal holds notes, the pitch bend wheel bends synths and samplers by their `Bend range` (2 semitones unless set), and the mod wheel moves the synth parameter its `Mod wheel` names (the cutoff unless set) up toward its maximum. All Notes Off ends a channel's notes as their release would. All Sound Off stops the instruments the channel plays within a few milliseconds, pattern notes and release tails included (effect tails ring out; a plugin is sent a choke, a MIDI-out track only its key releases). `H` gives a track a MIDI channel of its own, which plays it whichever track is selected; the other channels play the selected track, and several tracks on one channel play together. `C` records what you play while the pattern plays: each note lands on its nearest step, and the whole take is one undo step. `I` records from the audio input into the selected sampler track and loads the take when you press it again.

`Z` chooses a MIDI output, and `V` sends the selected track there instead of sounding it: on its MIDI channel, or channel 1, and drums as General MIDI notes on channel 10. Notes leave when their audio would be heard, and muting the track ends them. With `midi_clock = on` the output also gets MIDI clock, Start, Stop and Continue with the song position, so a drum machine or another sequencer follows TINVO's tempo; turned on while playing, the clock starts with the next Play. `midi_delay` sends everything a little later (up to 50 ms) when the output sounds before TINVO's audio does.

A MIDI file (format 0 or 1) opens as a new, unsaved project: each MIDI track and channel becomes a synth track, and the General MIDI drums on channel 10 become kick, snare, clap and hat tracks. Notes land on the nearest 16th-note step with their timing kept as micro-timing, a step holds a chord of up to four notes, and the file is cut into patterns of whole bars that the song plays in order, a repeated stretch reusing its pattern. The tempo comes from the file, and later tempo changes become tempo points from the bar line they fall on, or the next one. Export (`e` with a file name ending in `.mid`, or `tinvo render` to a `.mid` file) writes each heard track's notes where TINVO plays them, with swing, micro-timing, ratchets and each song entry's tempo; drums go to channel 10, and tracks without a MIDI channel of their own take the free ones in turn.

`tinvo plugins` lists the CLAP plugins in CLAP's standard folders and in the folders `CLAP_PATH` names: each one's name, version, vendor, what it is (instrument, effect…), id and file. Each file is read in a short-lived process of its own, so a plugin that crashes or hangs while it starts is listed as not loaded, with the reason, and TINVO carries on. What was read is kept in a cache until the file changes. `tinvo render` plays the plugins a project file names (`track plugin <id>`, `insert 0 plugin <id>`), each made for offline rendering; a plugin that is not installed or cannot load stops it with the plugin's name.

In the editor, `a` `8` adds a track playing a CLAP instrument chosen from a list, and `&` on the inserts page puts a CLAP effect in the selected slot. The editor reads the installed plugins in the background when it starts; until then, and for plugins that are not installed, the project keeps them as written and they stay silent. `&` also lists what could not play and why, and export plays the plugins as `tinvo render` does. A plugin's parameters are on the panel: a plugin track's on the instrument page, an insert's on a page of its own (`Enter` on its slot on the inserts page, `f` goes back). `←` `→` change one, `A` gives it a lane and undo takes a change back. Each plugin's state is saved with the project. `c` on those pages, or on a plugin's slot on the inserts page, opens the plugin's own window (floating if the plugin floats one, otherwise inside a window TINVO opens for it), and `c` again closes it. A knob turned there marks the project unsaved and shows on the panel, unless an automation lane holds that parameter (the panel then shows the lane's base).

The main keys in the editor, as they are by default (press `?` for the full list, which shows your own keys):

| Key | Action |
|---|---|
| `Space` | play / stop |
| `Tab` | switch between the step grid and the parameter panel |
| arrow keys | move the cursor, or select and adjust a parameter |
| `Enter` `x` | toggle a step |
| `,` `.` / `<` `>` | pitch down / up by a semitone / an octave |
| `[` `]` / `{` `}` | velocity / length |
| `-` `=` / `/` | chance / ratchets |
| `(` `)` | move the step earlier / later |
| `c` | cycle chords (maj, min, 7, m7, maj7, sus4) |
| `v` | piano roll for the track (`↑` `↓` pitch, `Enter` adds or removes a note); its last line shows what the track plays in each song entry |
| `u` `U` | undo / redo (also `Ctrl+Z` / `Ctrl+Y`) |
| `t` `T` / `g` `G` / `l` `L` | tempo / swing / pattern length |
| `b` `B` | shorter / longer steps for the pattern (1/8 to 1/32) |
| `k` `K` / `J` | fewer / more beats to the pattern's bar / the beat's note value (/4, /8, /16, /2) |
| `n` `N` | next / previous pattern |
| `y` `Y` `X` | copy / paste / clear the pattern |
| `S` | song view: entries across, tracks down (`←` `→` entry, `↑` `↓` the song's row or a track's, `,` `.` the entry's pattern, or on a track row its own pattern, the entry's or a rest; `Enter` repeat, `Del` remove the entry or make the track follow it, `[` `]` loop, `m` name a marker, `PgUp` `PgDn` previous / next marker, `=` loop the section, `{` `}` the entry's tempo point, `0` remove it, `/` hold or ramp between points) |
| `f` | parameter panel: instrument, channel strip and sends, the track's group, buses and master, insert effects |
| `,` `.` `<` `>` `-` `;` | on the inserts page: the slot's previous / next effect, move it a slot earlier / later, empty the slot, show the track's, its group's or the master's chain |
| `&` | CLAP plugins: on the inserts page, an effect for the slot; elsewhere, a new track playing an instrument; and what could not play |
| `Enter` | on the inserts page: the parameters of the plugin in the slot |
| `c` | in the panel: open or close the plugin's own window (on a plugin track's instrument page, a plugin's page, or its slot on the inserts page) |
| `A` | automate the selected parameter (panel) / show the automation lanes (grid) |
| in the automation view | `↑` `↓` lane, `Enter`/`x` add or remove a point, `[` `]` `{` `}` its value, `/` hold or ramp, `-` free the lane |
| `a` `D` `r` `m` `s` | add (`1`-`7` built-in, `8` a CLAP plugin), delete, rename, mute, solo a track |
| `i` `d` | send the track to the next group (from the last, back to the master); a new group for it |
| `E` `Q` `F` | rename, mute or delete the track's group (its tracks go to the master) |
| `H` | the MIDI channel (1–16) that plays the track whichever is selected; empty for none |
| `o` | load a WAV file, or import an SFZ instrument, into a sampler track (`a` `7` adds one) |
| `%` | slice the sampler track's sample: `t` at its transients (`t 80` finds fainter ones), or a number of equal parts |
| `O` | choose the audio output and input devices (`←` `→` switch pages, `Tab` toggles low-latency output) |
| `I` | record from the audio input into the sampler track; `I` again stops and loads the take |
| `M` | choose the MIDI input |
| `Z` | choose the MIDI output |
| `V` | send the track to the MIDI output instead of sounding it; `V` again plays it here |
| `R` | step entry: MIDI keys write the step at the cursor (keys held together make a chord) |
| `C` | record notes from the MIDI keyboard while the pattern plays (from a stop it starts playback, after a count-in if one is set); `C` again stops |
| `w` `W` `e` | save, save as, export WAV (or MIDI, to a `.mid` file) |
| `F2` `P` | settings: keys, colours and the mouse |
| `q` | quit |

In a text prompt (a file name, a track name, a sample path), `←` `→`, `Home`, `End`, `Backspace` and `Del` edit at the cursor, `Enter` accepts and `Esc` or `Ctrl+C` cancels; in the export prompt `←` `→` choose what to export instead. On Linux and macOS a paste goes into the prompt without its line breaks, tabs and other control characters, up to the prompt's 4096 bytes; it never runs keys. When text is cut, the prompt line says so (`[4096 bytes, full]`, or `[paste cut]` for a paste longer than 8192 bytes). An export writes only a `.wav` or `.mid` (`.midi`) file, never the project or one of its samples, and a WAV export waits until every sample the project plays is loaded.

The mouse works too. A click toggles a step, selects a track, or mutes and solos it (`M` `S`); in the piano roll it adds or removes a note, in the automation view a point, and on their song line it opens the pattern the track plays in that entry. Dragging across a track's steps fills or clears them, and dragging from a piano-roll note sets its length. The wheel changes the value under the pointer: tempo, swing, pattern, length, step length and beats to the bar in the top bar, a parameter, a point, a song entry or its tempo point. The right button only moves the cursor, or resets a parameter. To select text in the terminal, hold Shift (in most terminals) or set `mouse = off`.

### Configuration

Keys and colours are read from `tinvo.conf`: in `%APPDATA%\tinvo` on Windows, `~/Library/Application Support/tinvo` on macOS, and `$XDG_CONFIG_HOME/tinvo` (usually `~/.config/tinvo`) on Linux. The file needs only the lines you want to change:

```ini
[keys]
undo = u ctrl+z
left = h left
pattern_shorter =

[theme]
base = light
cursor = black on bright-cyan bold

[options]
mouse = off
record_mode = replace
```

An action listed under `[keys]` gets exactly the keys given; an empty value leaves it with none. `base` picks a built-in theme (`default` or `light`), and each role's style is `[colour] [on colour] [bold]`, where a colour is a name, a palette index `0`–`255` or `#rrggbb`. `tinvo settings` checks the file and prints every action and role with its current value, so its output is a full settings file to start from. A mistake stops TINVO before it starts and says what is wrong: the line, or for one key given to two actions that work in the same view, the key and both actions. Answers to questions (`y`, `n`, `Esc`) and editing in text prompts always use the same keys. `mouse = off` leaves the mouse to the terminal. `record_mode` (`overdub` or `replace`), `record_timing` (`keep` or `quantize`) and `count_in` (`off`, `1` or `2` bars) set how `C` records notes; overdub adds to the steps, replace keeps only what the last pass over each step played. `midi_clock` (`off` or `on`) sends MIDI clock and transport to the MIDI output, and `midi_delay` (`0`, `5`, `10`, `20`, `30` or `50` ms) sends everything to it that much later.

The settings page (`F2` or `P`) does the same without leaving the editor. It has three tabs, Keys, Colours and Options (`←` `→` or `Tab`). `Enter` adds a key to an action (press the key next) or changes a colour or option, `Del` removes an action's last key, `r` puts back the default and `Esc` closes the page. A change works at once. When the page closes, the changes are written into the settings file, keeping its comments, every line that did not change and whatever another TINVO session saved there meanwhile; a file that no longer reads is left alone. The page will not take the last key from the settings page itself or from quit.

### Contributing

Bug reports and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the rules, including the zero-dependency rule. A change has to pass `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. A change to a file format or the edit model should also survive a few minutes of `tinvo fuzz`. Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

Found a security problem? Please report it privately, as [SECURITY.md](SECURITY.md) describes, not in an issue.

### License

MIT. See [LICENSE](LICENSE). Author: 𝓐.𝓒.𝓑

[⬆ Back to top](#top)

---
<a id="turkce"></a>
## Türkçe

### Genel Bakış

TINVO, terminal penceresinde çalışan, adım sıralayıcı (step sequencer) tabanlı bir DAW. Synth ve davullar, gerçek zamanlı motor ve WAV yazıcı Rust'ın standart kütüphanesinden başka hiçbir şey kullanmadan yazıldı. Ses altyapıları (WASAPI, ALSA, JACK, CoreAudio) elle yazılmış FFI ile çağrılıyor. İndirilecek tek bir crate yok, bu yüzden internet olmadan da derleniyor. Release binary'si birkaç megabayt (Windows'ta yaklaşık 3 MB).

Ses callback'i hiçbir zaman bellek ayırıp serbest bırakmaz, kilit almaz, beklemez. WAV dışa aktarma çalmayla aynı motoru kullanır: ne duyuyorsan dosyaya o yazılır.

https://github.com/user-attachments/assets/46029a57-69c3-407c-a1af-21c34194486d

TINVO, [`examples/showcase.tvo`](examples/showcase.tvo) projesini çalıyor. Ses, aynı projenin `tinvo render` ile alınmış export'u ve ekran kaydının altına senkron yerleştirildi; export ile çalma aynı motoru kullanıyor. Yalnızca sesi dinlemek için: [showcase.mp3](docs/media/showcase.mp3). Kendin açmak için: `tinvo examples/showcase.tvo`.

### Özellikler

- 32 kanal, pattern başına 64'e kadar adım, 20–300 BPM tempo, %50–75 swing. Her pattern'in kendi adım uzunluğu (1/8, 1/8 triole, 1/16, 1/16 triole ya da 1/32) ve ölçüsü (3/4, 6/8, 7/8 gibi) var; vuruş ve ölçü çizgileri buna göre çizilir. Şarkı girişleri kendi tempolarını koyabilir; tempo bir sonrakine kadar sabit kalır ya da ona doğru kayar.
- Her adımda bir nota ya da en fazla 4 notalık akor, velocity, uzunluk, olasılık (%1–100), ratchet (1–8 vuruş) ve mikro zamanlama (yarım adıma kadar erken ya da geç).
- Melodik kanallar için piyano rulosu.
- 16 pattern (A–P) ve şarkı: döngü bölgesi olan, en fazla 256 pattern'lik bir zincir. İsimli marker'lar şarkıyı bölümlere ayırır. Her girişte bir kanal kendi pattern'ini çalabilir ya da susabilir. Tek bir pattern çalarken seçtiğin pattern, çalan bitince devreye girer.
- Otomasyon: her kanalda ses seviyesi, pan, herhangi bir enstrüman ya da kanal şeridi parametresi için en fazla 4 şerit. Adım adım konan noktalar arasında değer ya sabit kalır ya da kayarak geçer.
- Her kanalda bir kanal şeridi (channel strip): high-pass, üç bantlı EQ, kompresör ve aliasing'i bastırılmış drive.
- En fazla 8 grup bus'ı. Bir kanal ya bir gruba ya da master'a gider; grubun kendi stereo kanal şeridi (kompresörü iki tarafı birlikte bastırır), ses seviyesi, balansı, mute'u, send'leri ve otomasyonu vardır.
- Insert efektleri: her kanalda, grupta ve master'da 8 slot, projede toplam 64. Her slota EQ, kompresör, drive, delay, reverb, chorus, çok modlu filtre, phaser, bit crusher ya da gate konur, sıra serbesttir. Her slot bypass edilebilir ve otomasyona bağlanabilir; efekt eklemek, taşımak, bypass etmek ya da kaldırmak tıklama yapmaz, yumuşak geçişle olur.
- Gate 1,5 ms ileriyi dinler; diğer yollar da aynı kadar geciktirilir, böylece kanallar birbirine göre kaymaz. Export'ta ve stem'lerde de öyle.
- Her kanaldan tempoya bağlı bir delay'e, algoritmik bir reverb'e ve chorus'a send, key bus üzerinden sidechain kompresyon, master'da true-peak limiter. Bus ve master ayarları otomasyona bağlanabilir.
- Çalarken gain reduction ve loudness göstergeleri (momentary, short-term, integrated).
- 8 sesli subtractive synth: iki band-limited osilatör (pulse width, detune'lu unison), noise, state-variable filtre, iki zarf, glide, LFO (pitch, pulse width, cutoff ve seviyeye; Hz cinsinden serbest ya da tempoya bağlı, 1/32'den dört birliğe kadar) ve velocity → cutoff.
- Sentezlenmiş kick, snare, kapalı ve açık hi-hat, clap. Kapalı hat açık hat'i susturur (choke).
- WAV dosyalarını windowed-sinc interpolasyonla çalan 16 sesli sampler: kök nota ve akort, başlangıç/bitiş noktası, loop, gate ya da one-shot, ADSR ve filtre. Bir sampler kanalı kendi sample'ının yanında dosyaları klavyeye bölge olarak da yayabilir; bölgeler proje dosyasında `zone` satırlarıdır ya da `o` ile bir SFZ enstrümanından alınır: tuş aralıkları, velocity katmanları, sırayla ya da rastgele round robin, release sample'ları ve her bölgenin kendi loop modu. WAV dosyasının kendi loop noktaları ve kök notası (`smpl` chunk'ı) da okunur. `%` kanalın sample'ını transient'lerinden ya da eşit parçalara böler; dilimler kanalın sample'ının ve bölgelerinin yerini alır ve her biri C2'den başlayarak bir tuşa düşer, böylece bir break klavyeden ya da grid'den çalınır. Hepsi tek bir dosyayı çalan bölgeler (dilimlemenin bıraktığı gibi) yeniden dosyanın tamamından kesilir.
- Tıklamasız ses, pan, mute ve solo; seviye göstergeleri.
- 1000 adımlık geri al / yinele.
- Kaydet/aç döngüsünde hiçbir değeri değiştirmeyen metin tabanlı `.tvo` proje dosyaları. Kayıtlar atomik, eski format sürümleri de açılıyor.
- Kaydedilmemiş değişiklik varken 5 saniyede bir otomatik yedek. TINVO çökerse ya da öldürülürse, bir sonraki açılışta bu değişiklikleri geri yüklemeyi önerir.
- Bir proje aynı anda tek bir TINVO oturumunda açık olur. İkinci oturum, projeyi tutan sürecin numarasıyla birlikte reddedilir. Çökme projeyi kilitli bırakmaz.
- Editörün tuşları, renkleri ve faresi, değişikliği anında uygulayan bir ayar sayfasından (`F2`) ya da bu sayfanın yazdığı ayar dosyasından değiştirilebilir. Açık renkli terminaller için hazır bir tema da var.
- Fare desteği: adımlara, kanallara, notalara ve parametrelere tıkla; sürükleyerek adımları doldur ya da bir notayı uzat; bir değerin üstünde tekerleği çevirerek onu değiştir.
- 16 veya 24 bit (dither'lı) ya da 32-bit float WAV dışa aktarma: şarkının tamamı, tek bir pattern, döngü bölgesi ya da iki marker arası, istenirse stem olarak: her kanal ayrı dosyaya, gruptaki kanallar ise grup başına tek dosyaya. Her dışa aktarmada loudness (LUFS, BS.1770), true peak ve varsa clipping raporlanır.
- Standard MIDI File: şarkı, bir pattern ya da döngü bölgesi başka bir programa nota olarak aktarılabilir; bir MIDI dosyası da yeni bir proje olarak açılabilir.
- MIDI çıkışı: herhangi bir kanal kendi sesi yerine harici bir synth'i ya da başka bir programı, sesle aynı zamanda çalabilir; MIDI clock da diğer cihazları TINVO'nun temposunda tutar.

Yol haritası [docs/ROADMAP.md](docs/ROADMAP.md), mimari [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) dosyasında (İngilizce).

### Gereksinimler

- Rust 1.85 veya üstü.
- Linux: ALSA geliştirme paketi (Debian/Ubuntu'da `libasound2-dev`, Fedora'da `alsa-lib-devel`). JACK isteğe bağlı: bir sunucu (jackd ya da PipeWire'ın JACK'i) çalışıyorsa ve onu seçerseniz TINVO JACK üzerinden çalar.
- Windows 10 ve üstü ya da macOS 11 ve üstü için ek bir şey gerekmez.
- En az 64×16 karakterlik, UTF-8 ve ANSI renk destekli bir terminal (Windows Terminal ya da güncel herhangi bir Linux/macOS terminali).

### Kurulum

```bash
git clone https://github.com/IACBI/tinvo.git
cd tinvo
cargo install --path .
```

Kurmadan derlemek için `cargo build --release` yeterli; binary `target/release/tinvo` konumunda oluşur.

### Kullanım

```bash
tinvo                     # open the editor with the demo beat
tinvo song.tvo            # open a project, or start a new one at that path
tinvo render song.tvo song.wav --format 24 --loops 4
tinvo render song.tvo beat.wav --pattern B --stems
tinvo render song.tvo chorus.wav --from chorus --loops 1   # the section from marker "chorus" to the next
tinvo render song.tvo song.mid    # the song's notes as a Standard MIDI File
tinvo import song.mid song.tvo    # a MIDI file as a new project (or open it: tinvo song.mid)
tinvo demo demo.tvo       # write the demo project to a file
tinvo probe 3             # check the audio device for 3 seconds
tinvo devices             # list the output devices
tinvo --device usb song.tvo        # play on the device whose name contains "usb"
tinvo --device jack song.tvo       # Linux: play through JACK or PipeWire's JACK
tinvo probe --device realtek --low-latency 3
tinvo loopback --input realtek     # play sweeps and hear them back: checks both devices, measures the round trip
tinvo midi                # list the MIDI inputs and outputs
tinvo --midi keystation song.tvo   # play from that MIDI keyboard
tinvo --midi-out "synth" song.tvo  # send MIDI-out tracks and the clock there
tinvo inputs              # list the audio input devices
tinvo record take.wav 10  # record 10 seconds from the default input
tinvo plugins             # list the CLAP plugins installed (--rescan reads every file again)
tinvo bench               # measure rendering speed
tinvo bench --check       # fail if a scenario is over its time budget
tinvo fuzz wav --seconds 600       # fuzz the WAV reader for 10 minutes
tinvo sfz piano.sfz       # what the sampler plays of an SFZ instrument, and what it leaves out
tinvo slice break.wav     # where the slicer cuts a sample (t 80 for fainter hits, 16 for equal parts)
tinvo settings            # check the settings file and print every key and colour
tinvo --settings my.conf song.tvo  # use the keys and colours in my.conf
```

Başka bir cihaz seçilmedikçe editör sistemin varsayılan çıkışını kullanır, varsayılan değişince onu takip eder, cihaz kaybolursa kendi kendine yeniden bağlanır. Bulduğu ilk MIDI girişini de açar: çalınan notalar seçili kanalda duyulur, adım girişi açıkken (`R`) imlecin olduğu adıma yazılır. Sustain pedalı notaları tutar, pitch bend tekerleği synth ve sampler'ı `Bend range` kadar büker (ayarlanmadıysa 2 yarım ses), mod tekerleği de synth'te `Mod wheel` ile seçilen parametreyi (ayarlanmadıysa cutoff) en yüksek değerine doğru götürür. All Notes Off bir MIDI kanalının notalarını bırakılmış gibi sonlandırır. All Sound Off ise o kanalın çaldığı enstrümanları, pattern notaları ve release kuyrukları dahil birkaç milisaniyede susturur (efekt kuyrukları çalmaya devam eder; eklentiye choke gönderilir, MIDI-out kanalına yalnızca tuş bırakmaları gider). `H` ile bir kanala kendi MIDI kanalı verilir; o MIDI kanalı, seçili kanal hangisi olursa olsun onu çalar. Diğer MIDI kanalları seçili kanalı çalar, aynı MIDI kanalındaki birden çok kanal birlikte çalar. `C` ise pattern çalarken çaldıklarını kaydeder: her nota en yakın adıma düşer, kaydın tamamı tek bir undo adımıdır. `I` ses girişinden seçili sampler kanalına kayıt alır; tekrar basınca kayıt durur ve kanala yüklenir.

`Z` bir MIDI çıkışı seçer, `V` de seçili kanalı kendi sesi yerine oraya gönderir: kendi MIDI kanalından ya da 1. kanaldan; davullar 10. kanalda General MIDI notaları olarak gider. Notalar, sesleri duyulacağı anda çıkar; kanal mute edilince notaları da biter. `midi_clock = on` ile çıkışa MIDI clock, Start, Stop ve şarkı konumuyla Continue da gider, böylece bir davul makinesi ya da başka bir sequencer TINVO'nun temposunu takip eder; çalarken açılırsa clock bir sonraki Play ile başlar. Çıkış TINVO'nun sesinden önce duyuluyorsa `midi_delay` her şeyi biraz geciktirir (en fazla 50 ms).

Bir MIDI dosyası (format 0 ya da 1) kaydedilmemiş yeni bir proje olarak açılır: her MIDI track'i ve kanalı bir synth kanalı olur, 10. kanaldaki General MIDI davulları da kick, snare, clap ve hat kanallarına dönüşür. Notalar en yakın 16'lık adıma düşer, zamanlamaları micro-timing olarak korunur; bir adımda en fazla dört notalık akor olur. Dosya tam ölçülerden oluşan pattern'lere bölünür, şarkı bunları sırayla çalar; tekrar eden bir bölüm aynı pattern'i kullanır. Tempo dosyadan gelir; sonraki tempo değişiklikleri denk geldikleri ölçü çizgisinden, ölçü ortasındaysa bir sonrakinden başlayan tempo noktaları olur. Dışa aktarma (`e` ile `.mid` uzantılı bir dosya adı ya da `.mid` dosyasına `tinvo render`), duyulan her kanalın notalarını TINVO'nun çaldığı yere yazar: swing, micro-timing, ratchet ve her şarkı girişinin temposuyla. Davullar 10. kanala gider; kendi MIDI kanalı olmayan kanallar boştaki MIDI kanallarını sırayla alır.

`tinvo plugins`, CLAP'in standart klasörlerinde ve `CLAP_PATH` ile verilen klasörlerde kurulu CLAP eklentilerini listeler: her birinin adını, sürümünü, üreticisini, ne olduğunu (instrument, effect…), id'sini ve dosyasını. Her dosya kısa ömürlü, ayrı bir süreçte okunur; açılırken çöken ya da takılan bir eklenti nedeniyle birlikte "yüklenemedi" diye listelenir, TINVO işine devam eder. Okunanlar, dosya değişene kadar önbellekte kalır. `tinvo render`, proje dosyasının adını verdiği eklentileri (`track plugin <id>`, `insert 0 plugin <id>`) çevrimdışı render için açarak çalar; kurulu olmayan ya da açılamayan bir eklenti, adıyla birlikte render'ı durdurur.

Editörde `a` `8`, listeden seçilen bir CLAP enstrümanını çalan bir kanal ekler; insert sayfasında `&`, seçili slota bir CLAP efekti koyar. Editör açılırken kurulu eklentileri arka planda okur; o bitene kadar ve kurulu olmayan eklentiler için proje onları yazıldığı gibi tutar, sesleri çıkmaz. `&` ayrıca neyin çalamadığını ve nedenini listeler; export da eklentileri `tinvo render` gibi çalar. Eklentinin parametreleri paneldedir: eklenti çalan bir kanalınkiler enstrüman sayfasında, bir insert'ünkiler kendi sayfasında (insert sayfasında slotunun üstünde `Enter`, geri dönmek için `f`). `←` `→` değeri değiştirir, `A` ona bir şerit verir, undo değişikliği geri alır. Her eklentinin durumu projeyle birlikte kaydedilir. Bu sayfalarda ya da insert sayfasında eklentinin slotunda `c`, eklentinin kendi penceresini açar (eklenti kendi penceresini ayrı açabiliyorsa öyle, yoksa TINVO'nun onun için açtığı bir pencerenin içinde); `c` yeniden kapatır. Orada çevrilen bir düğme projeyi kaydedilmemiş yapar ve panelde görünür; o parametreyi bir otomasyon şeridi tutuyorsa panel şeridin taban değerini gösterir.

Editördeki temel tuşlar, varsayılan halleriyle (tam liste için `?`; orada senin tuşların görünür):

| Tuş | İşlev |
|---|---|
| `Space` | çal / durdur |
| `Tab` | adım ızgarası ile parametre paneli arasında geçiş |
| ok tuşları | imleci gezdir; panelde parametre seç ve ayarla |
| `Enter` `x` | adımı aç / kapat |
| `,` `.` / `<` `>` | perdeyi yarım ton / oktav düşür, yükselt |
| `[` `]` / `{` `}` | velocity / uzunluk |
| `-` `=` / `/` | olasılık / ratchet |
| `(` `)` | adımı erkene / geçe kaydır |
| `c` | akorlar arasında dolaş (maj, min, 7, m7, maj7, sus4) |
| `v` | kanalın piyano rulosu (`↑` `↓` perde, `Enter` nota ekler ya da siler); son satırı kanalın her şarkı girişinde ne çaldığını gösterir |
| `u` `U` | geri al / yinele (`Ctrl+Z` / `Ctrl+Y` de olur) |
| `t` `T` / `g` `G` / `l` `L` | tempo / swing / pattern uzunluğu |
| `b` `B` | pattern'in adımlarını kısaltır / uzatır (1/8'den 1/32'ye) |
| `k` `K` / `J` | pattern'in ölçüsünde vuruş sayısını azaltır / artırır / vuruşun nota değerini değiştirir (/4, /8, /16, /2) |
| `n` `N` | sonraki / önceki pattern |
| `y` `Y` `X` | pattern'i kopyala / yapıştır / temizle |
| `S` | şarkı görünümü: girişler yan yana, kanallar alt alta (`←` `→` giriş, `↑` `↓` şarkı satırı ya da bir kanalın satırı, `,` `.` girişin pattern'i, kanal satırında ise kanalın kendi pattern'i, girişinki ya da sus; `Enter` tekrarla, `Del` girişi sil ya da kanalı girişe uydur, `[` `]` döngü, `m` marker koy, `PgUp` `PgDn` önceki / sonraki marker, `=` bölümü döngüye al, `{` `}` girişin tempo noktası, `0` noktayı sil, `/` noktalar arasında sabit tut ya da kaydır) |
| `f` | parametre paneli: enstrüman, kanal şeridi ve send'ler, kanalın grubu, buslar ve master, insert efektleri |
| `,` `.` `<` `>` `-` `;` | insert sayfasında: slottaki önceki / sonraki efekt, efekti bir slot öne / arkaya taşı, slotu boşalt, kanalın, grubunun ya da master'ın zincirini göster |
| `&` | CLAP eklentileri: insert sayfasında slot için bir efekt; başka yerde, bir enstrüman çalan yeni bir kanal; ve neyin çalamadığı |
| `Enter` | insert sayfasında: slottaki eklentinin parametreleri |
| `c` | panelde: eklentinin kendi penceresini açar ya da kapatır (eklenti çalan kanalın enstrüman sayfasında, eklentinin sayfasında ya da insert sayfasında slotunda) |
| `A` | seçili parametreyi otomasyona al (panel) / otomasyon şeritlerini göster (ızgara) |
| otomasyon görünümünde | `↑` `↓` şerit, `Enter`/`x` nokta ekle ya da sil, `[` `]` `{` `}` değeri, `/` sabit ya da kayan, `-` şeridi boşalt |
| `a` `D` `r` `m` `s` | kanal ekle (`1`-`7` hazır enstrümanlar, `8` bir CLAP eklentisi), sil, yeniden adlandır, mute, solo |
| `i` `d` | kanalı sıradaki gruba gönder (sonuncudan sonra master'a döner); kanal için yeni grup aç |
| `E` `Q` `F` | kanalın grubunu yeniden adlandır, mute et ya da sil (kanalları master'a gider) |
| `H` | kanalı, seçili olmasa da çalan MIDI kanalı (1–16); boş bırakılırsa yok |
| `o` | sampler kanalına WAV dosyası yükle ya da SFZ enstrümanı al (`a` `7` sampler kanalı ekler) |
| `%` | sampler kanalının sample'ını dilimle: `t` transient'lerinden (`t 80` daha zayıflarını da bulur) ya da sayı kadar eşit parçaya |
| `O` | ses çıkış ve giriş cihazlarını seç (`←` `→` sayfa değiştirir, `Tab` düşük gecikmeli çıkışı açıp kapatır) |
| `I` | ses girişinden sampler kanalına kayıt al; tekrar `I` kaydı durdurup kanala yükler |
| `M` | MIDI girişini seç |
| `Z` | MIDI çıkışını seç |
| `V` | kanalı kendi sesi yerine MIDI çıkışına gönder; tekrar `V` yeniden burada çalar |
| `R` | adım girişi: MIDI tuşları imleçteki adımı yazar (birlikte basılan tuşlar akor olur) |
| `C` | pattern çalarken MIDI klavyeden nota kaydet (durmuşken çalmayı başlatır, ayarlıysa önce count-in verir); tekrar `C` durdurur |
| `w` `W` `e` | kaydet, farklı kaydet, WAV (ya da `.mid` dosyasına MIDI) dışa aktar |
| `F2` `P` | ayarlar: tuşlar, renkler ve fare |
| `q` | çık |

Bir metin isteminde (dosya adı, kanal adı, sample yolu) `←` `→`, `Home`, `End`, `Backspace` ve `Del` imlecin olduğu yerde düzenler; `Enter` onaylar, `Esc` ya da `Ctrl+C` vazgeçer. Export isteminde `←` `→` bunun yerine neyin dışa aktarılacağını seçer. Linux ve macOS'ta yapıştırılan metin; satır sonları, sekmeler ve diğer kontrol karakterleri atılarak, istemin 4096 baytına kadar isteme girer, hiçbir tuşu çalıştırmaz. Metin kesildiğinde istem satırı bunu gösterir (`[4096 bytes, full]` ya da 8192 bayttan uzun bir yapıştırmada `[paste cut]`). Export yalnızca `.wav` ya da `.mid` (`.midi`) dosyası yazar; projenin kendisinin ya da çaldığı bir sample'ın üzerine yazmaz. WAV export'u, projenin çaldığı bütün sample'lar yüklenene kadar yapılmaz.

Fare de çalışır. Tıklamak bir adımı açıp kapatır, bir kanalı seçer ya da onu susturur, solo yapar (`M` `S`); piyano rulosunda nota, otomasyon görünümünde nokta ekler ya da siler; bu iki görünümün şarkı satırında ise kanalın o girişte çaldığı pattern'i açar. Bir kanalın adımları üzerinde sürüklemek onları doldurur ya da temizler; piyano rulosunda bir notadan sürüklemek uzunluğunu ayarlar. Tekerlek, işaretçinin altındaki değeri değiştirir: üst çubukta tempo, swing, pattern, uzunluk, adım uzunluğu ve ölçüdeki vuruş sayısı, bir parametre, bir nokta, bir şarkı girişi ya da tempo noktası. Sağ tuş yalnızca imleci oraya götürür ya da parametreyi varsayılanına döndürür. Terminalde metin seçmek için Shift'e basılı tut (çoğu terminalde) ya da `mouse = off` yap.

### Yapılandırma

Tuşlar ve renkler `tinvo.conf` dosyasından okunur: Windows'ta `%APPDATA%\tinvo`, macOS'te `~/Library/Application Support/tinvo`, Linux'ta `$XDG_CONFIG_HOME/tinvo` (genellikle `~/.config/tinvo`) klasöründe. Dosyaya sadece değiştirmek istediğin satırları yazman yeterli:

```ini
[keys]
undo = u ctrl+z
left = h left
pattern_shorter =

[theme]
base = light
cursor = black on bright-cyan bold

[options]
mouse = off
record_mode = replace
```

`[keys]` altında yazılan bir eylem yalnızca verilen tuşlara bağlanır; değeri boş bırakılırsa hiç tuşu kalmaz. `base` hazır temalardan birini seçer (`default` ya da `light`). Her rolün stili `[renk] [on renk] [bold]` biçimindedir; renk bir isim, `0`–`255` arası palet numarası ya da `#rrggbb` olabilir. `tinvo settings` dosyayı denetler ve bütün eylemleri ve rolleri şu anki değerleriyle yazdırır; çıktısı başlamak için hazır, eksiksiz bir ayar dosyasıdır. Dosyada bir hata varsa TINVO açılmadan durur ve sorunu söyler: hatalı satırı, ya da aynı görünümde çalışan iki eyleme aynı tuş verildiyse o tuşu ve iki eylemi. Sorulara verilen cevaplar (`y`, `n`, `Esc`) ve metin kutularındaki düzenleme tuşları sabittir. `mouse = off` fareyi terminale bırakır. `record_mode` (`overdub` ya da `replace`), `record_timing` (`keep` ya da `quantize`) ve `count_in` (`off`, `1` ya da `2` ölçü), `C` ile nota kaydının nasıl yapılacağını belirler: overdub adımlara ekler, replace her adımda yalnızca son geçişte çalınanı tutar. `midi_clock` (`off` ya da `on`) MIDI çıkışına clock ve transport gönderir, `midi_delay` (`0`, `5`, `10`, `20`, `30` ya da `50` ms) da çıkışa giden her şeyi o kadar geciktirir.

Ayar sayfası (`F2` ya da `P`) aynı işi editörden çıkmadan yapar. Üç sekmesi var: Keys, Colours ve Options (`←` `→` ya da `Tab`). `Enter` bir eyleme tuş ekler (ardından tuşa bas) ya da bir rengi veya seçeneği değiştirir, `Del` eylemin son tuşunu siler, `r` varsayılanı geri getirir, `Esc` sayfayı kapatır. Değişiklik anında çalışır. Sayfa kapanınca değişiklikler ayar dosyasına yazılır; dosyadaki yorumlar, değişmeyen satırlar ve bu arada başka bir TINVO oturumunun kaydettikleri olduğu gibi kalır. Artık okunamayan bir dosyaya dokunulmaz. Sayfa, ayar sayfasının ve çıkışın son tuşunu silmeye izin vermez.

### Katkı

Hata bildirimleri ve pull request'ler memnuniyetle karşılanır. Sıfır bağımlılık kuralı dahil tüm kurallar [CONTRIBUTING.md](CONTRIBUTING.md) dosyasında (İngilizce). Her değişiklik `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` ve `cargo test` adımlarından geçmeli. Dosya formatlarına ya da düzenleme modeline dokunan bir değişiklik birkaç dakikalık `tinvo fuzz` çalışmasından da sağ çıkmalı. Projeye katılan herkes [davranış kurallarına](CODE_OF_CONDUCT.md) (İngilizce) uyar.

Bir güvenlik açığı mı buldunuz? Lütfen issue açmayın; [SECURITY.md](SECURITY.md) dosyasında anlatıldığı gibi gizli olarak bildirin.

### Lisans

MIT. Ayrıntılar [LICENSE](LICENSE) dosyasında. Yazar: 𝓐.𝓒.𝓑

[⬆ Başa Dön](#top)
