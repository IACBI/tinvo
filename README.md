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

TINVO is a step-sequencer DAW that runs in a terminal window. Everything is written in Rust on top of the standard library alone: the synthesizer and drums, the real-time engine, the WAV writer, and the audio backends (WASAPI, ALSA, JACK, CoreAudio) are called through hand-written FFI. There are no crates to download, so it builds offline. The release binary is about 0.5 MB.

The audio thread never allocates, locks or blocks. WAV export runs the same engine as playback, so what you hear is what you export.

### Features

- 16 tracks, up to 64 sixteenth-note steps, tempo 20–300 BPM, swing 50–75 %.
- Per step: a note or a chord of up to 4 notes, velocity, length, chance (1–100 %), ratchets (1–4 hits) and micro-timing (up to half a step early or late).
- A piano roll for melodic tracks.
- 16 patterns (A–P) and a song: a chain of up to 256 patterns with a loop region. Patterns switch at the end of the bar while playing.
- Automation: up to 4 lanes per track for volume, pan or any instrument or strip parameter, holding or ramping between points set per step.
- A channel strip on every track: high-pass, three-band EQ, compressor and antialiased drive.
- Sends from every track to a tempo-synced delay, an algorithmic reverb and a chorus, sidechain compression through a key bus, and a true-peak limiter on the master. Bus and master settings can be automated.
- Gain-reduction and loudness meters (momentary, short-term, integrated) while playing.
- 8-voice subtractive synth with two band-limited oscillators (pulse width, unison with detune), noise, a state-variable filter, two envelopes, glide, an LFO (to pitch, pulse width, cutoff and level), and velocity → cutoff.
- Synthesized kick, snare, closed and open hi-hat, and clap. The closed hat chokes the open hat.
- An 8-voice sampler that plays WAV files with windowed-sinc interpolation: root note and tuning, start/end points, loop, gate or one-shot, ADSR and a filter.
- Mixer with click-free volume, pan, mute and solo, plus peak meters.
- Undo/redo with a 1000-step history.
- Text `.tvo` project files that survive a save/load round trip exactly. Saves are atomic, and older format versions still load.
- Autosave every 5 seconds while there are unsaved edits. If TINVO crashes or is killed, the next start offers to recover them.
- The editor's keys and colours can be changed in a settings file. A light theme is built in.
- Mouse support: click steps, tracks, notes and parameters, drag to fill steps or stretch a note, turn the wheel over a value to change it.
- WAV export at 16 or 24 bit (dithered) or 32-bit float: the song, one pattern or the loop region, optionally as per-track stems. Each export reports its loudness (LUFS, BS.1770), true peak and any clipping.

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
tinvo demo demo.tvo       # write the demo project to a file
tinvo probe 3             # check the audio device for 3 seconds
tinvo devices             # list the output devices
tinvo --device usb song.tvo        # play on the device whose name contains "usb"
tinvo --device jack song.tvo       # Linux: play through JACK or PipeWire's JACK
tinvo probe --device realtek --low-latency 3
tinvo midi                # list the MIDI inputs
tinvo --midi keystation song.tvo   # play from that MIDI keyboard
tinvo inputs              # list the audio input devices
tinvo record take.wav 10  # record 10 seconds from the default input
tinvo bench               # measure rendering speed
tinvo bench --check       # fail if a scenario is over its time budget
tinvo fuzz wav --seconds 600       # fuzz the WAV reader for 10 minutes
tinvo settings            # check the settings file and print every key and colour
tinvo --settings my.conf song.tvo  # use the keys and colours in my.conf
```

The editor stays on the system default unless told otherwise, follows it when it changes, and reconnects by itself if the device disappears. It also opens the first MIDI input it finds: notes play the selected track, and with step entry on (`R`) they write steps at the cursor. `I` records from the audio input into the selected sampler track and loads the take when you press it again.

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
| `v` | piano roll for the track (`↑` `↓` pitch, `Enter` adds or removes a note) |
| `u` `U` | undo / redo (also `Ctrl+Z` / `Ctrl+Y`) |
| `t` `T` / `g` `G` / `l` `L` | tempo / swing / pattern length |
| `n` `N` | next / previous pattern |
| `y` `Y` `X` | copy / paste / clear the pattern |
| `S` | song view (`←` `→` entry, `↑` `↓` its pattern, `Enter` repeat, `Del` remove, `[` `]` loop) |
| `f` | parameter panel: instrument, channel strip and sends, buses and master |
| `A` | automate the selected parameter (panel) / show the automation lanes (grid) |
| in the automation view | `↑` `↓` lane, `Enter`/`x` add or remove a point, `[` `]` `{` `}` its value, `/` hold or ramp, `-` free the lane |
| `a` `D` `r` `m` `s` | add, delete, rename, mute, solo a track |
| `o` | load a WAV file into a sampler track (`a` `7` adds one) |
| `O` | choose the audio output and input devices (`←` `→` switch pages, `Tab` toggles low-latency output) |
| `I` | record from the audio input into the sampler track; `I` again stops and loads the take |
| `M` | choose the MIDI input |
| `R` | step entry: MIDI keys write the step at the cursor (keys held together make a chord) |
| `w` `W` `e` | save, save as, export WAV |
| `q` | quit |

The mouse works too. A click toggles a step, selects a track, or mutes and solos it (`M` `S`); in the piano roll it adds or removes a note, in the automation view a point. Dragging across a track's steps fills or clears them, and dragging from a piano-roll note sets its length. The wheel changes the value under the pointer: tempo, swing, pattern and length in the top bar, a parameter, a point, a song entry. The right button only moves the cursor, or resets a parameter. To select text in the terminal, hold Shift (in most terminals) or set `mouse = off`.

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
```

An action listed under `[keys]` gets exactly the keys given; an empty value leaves it with none. `base` picks a built-in theme (`default` or `light`), and each role's style is `[colour] [on colour] [bold]`, where a colour is a name, a palette index `0`–`255` or `#rrggbb`. `tinvo settings` checks the file and prints every action and role with its current value, so its output is a full settings file to start from. A mistake stops TINVO before it starts and says what is wrong: the line, or for one key given to two actions that work in the same view, the key and both actions. Answers to questions (`y`, `n`, `Esc`) and editing in text prompts always use the same keys. `mouse = off` leaves the mouse to the terminal.

### Contributing

Bug reports and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the rules, including the zero-dependency rule. A change has to pass `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. A change to a file format or the edit model should also survive a few minutes of `tinvo fuzz`.

### License

MIT. See [LICENSE](LICENSE). Author: 𝓐.𝓒.𝓑

[⬆ Back to top](#top)

---
<a id="turkce"></a>
## Türkçe

### Genel Bakış

TINVO, terminal penceresinde çalışan, adım sıralayıcı (step sequencer) tabanlı bir DAW. Synth ve davullar, gerçek zamanlı motor ve WAV yazıcı Rust'ın standart kütüphanesinden başka hiçbir şey kullanmadan yazıldı. Ses altyapıları (WASAPI, ALSA, JACK, CoreAudio) elle yazılmış FFI ile çağrılıyor. İndirilecek tek bir crate yok, bu yüzden internet olmadan da derleniyor. Release binary'si yaklaşık 0,5 MB.

Ses thread'i hiçbir zaman bellek ayırmaz, kilit almaz, beklemez. WAV dışa aktarma çalmayla aynı motoru kullanır: ne duyuyorsan dosyaya o yazılır.

### Özellikler

- 16 kanal, 64'e kadar 16'lık adım, 20–300 BPM tempo, %50–75 swing.
- Her adımda bir nota ya da en fazla 4 notalık akor, velocity, uzunluk, olasılık (%1–100), ratchet (1–4 vuruş) ve mikro zamanlama (yarım adıma kadar erken ya da geç).
- Melodik kanallar için piyano rulosu.
- 16 pattern (A–P) ve şarkı: döngü bölgesi olan, en fazla 256 pattern'lik bir zincir. Çalarken pattern değişikliği ölçü sonunda devreye girer.
- Otomasyon: her kanalda ses seviyesi, pan, herhangi bir enstrüman ya da kanal şeridi parametresi için en fazla 4 şerit. Adım adım konan noktalar arasında değer ya sabit kalır ya da kayarak geçer.
- Her kanalda bir kanal şeridi (channel strip): high-pass, üç bantlı EQ, kompresör ve aliasing'i bastırılmış drive.
- Her kanaldan tempoya bağlı bir delay'e, algoritmik bir reverb'e ve chorus'a send, key bus üzerinden sidechain kompresyon, master'da true-peak limiter. Bus ve master ayarları otomasyona bağlanabilir.
- Çalarken gain reduction ve loudness göstergeleri (momentary, short-term, integrated).
- 8 sesli subtractive synth: iki band-limited osilatör (pulse width, detune'lu unison), noise, state-variable filtre, iki zarf, glide, LFO (pitch, pulse width, cutoff ve seviyeye) ve velocity → cutoff.
- Sentezlenmiş kick, snare, kapalı ve açık hi-hat, clap. Kapalı hat açık hat'i susturur (choke).
- WAV dosyalarını windowed-sinc interpolasyonla çalan 8 sesli sampler: kök nota ve akort, başlangıç/bitiş noktası, loop, gate ya da one-shot, ADSR ve filtre.
- Tıklamasız ses, pan, mute ve solo; seviye göstergeleri.
- 1000 adımlık geri al / yinele.
- Kaydet/aç döngüsünde hiçbir değeri değiştirmeyen metin tabanlı `.tvo` proje dosyaları. Kayıtlar atomik, eski format sürümleri de açılıyor.
- Kaydedilmemiş değişiklik varken 5 saniyede bir otomatik yedek. TINVO çökerse ya da öldürülürse, bir sonraki açılışta bu değişiklikleri geri yüklemeyi önerir.
- Editörün tuşları ve renkleri bir ayar dosyasından değiştirilebilir. Açık renkli terminaller için hazır bir tema da var.
- Fare desteği: adımlara, kanallara, notalara ve parametrelere tıkla; sürükleyerek adımları doldur ya da bir notayı uzat; bir değerin üstünde tekerleği çevirerek onu değiştir.
- 16 veya 24 bit (dither'lı) ya da 32-bit float WAV dışa aktarma: şarkının tamamı, tek bir pattern ya da döngü bölgesi, istenirse her kanal ayrı dosyaya (stem). Her dışa aktarmada loudness (LUFS, BS.1770), true peak ve varsa clipping raporlanır.

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
tinvo demo demo.tvo       # write the demo project to a file
tinvo probe 3             # check the audio device for 3 seconds
tinvo devices             # list the output devices
tinvo --device usb song.tvo        # play on the device whose name contains "usb"
tinvo --device jack song.tvo       # Linux: play through JACK or PipeWire's JACK
tinvo probe --device realtek --low-latency 3
tinvo midi                # list the MIDI inputs
tinvo --midi keystation song.tvo   # play from that MIDI keyboard
tinvo inputs              # list the audio input devices
tinvo record take.wav 10  # record 10 seconds from the default input
tinvo bench               # measure rendering speed
tinvo bench --check       # fail if a scenario is over its time budget
tinvo fuzz wav --seconds 600       # fuzz the WAV reader for 10 minutes
tinvo settings            # check the settings file and print every key and colour
tinvo --settings my.conf song.tvo  # use the keys and colours in my.conf
```

Başka bir cihaz seçilmedikçe editör sistemin varsayılan çıkışını kullanır, varsayılan değişince onu takip eder, cihaz kaybolursa kendi kendine yeniden bağlanır. Bulduğu ilk MIDI girişini de açar: çalınan notalar seçili kanalda duyulur, adım girişi açıkken (`R`) imlecin olduğu adıma yazılır. `I` ses girişinden seçili sampler kanalına kayıt alır; tekrar basınca kayıt durur ve kanala yüklenir.

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
| `v` | kanalın piyano rulosu (`↑` `↓` perde, `Enter` nota ekler ya da siler) |
| `u` `U` | geri al / yinele (`Ctrl+Z` / `Ctrl+Y` de olur) |
| `t` `T` / `g` `G` / `l` `L` | tempo / swing / pattern uzunluğu |
| `n` `N` | sonraki / önceki pattern |
| `y` `Y` `X` | pattern'i kopyala / yapıştır / temizle |
| `S` | şarkı görünümü (`←` `→` giriş, `↑` `↓` pattern'i, `Enter` tekrarla, `Del` sil, `[` `]` döngü) |
| `f` | parametre paneli: enstrüman, kanal şeridi ve send'ler, buslar ve master |
| `A` | seçili parametreyi otomasyona al (panel) / otomasyon şeritlerini göster (ızgara) |
| otomasyon görünümünde | `↑` `↓` şerit, `Enter`/`x` nokta ekle ya da sil, `[` `]` `{` `}` değeri, `/` sabit ya da kayan, `-` şeridi boşalt |
| `a` `D` `r` `m` `s` | kanal ekle, sil, yeniden adlandır, mute, solo |
| `o` | sampler kanalına WAV dosyası yükle (`a` `7` sampler kanalı ekler) |
| `O` | ses çıkış ve giriş cihazlarını seç (`←` `→` sayfa değiştirir, `Tab` düşük gecikmeli çıkışı açıp kapatır) |
| `I` | ses girişinden sampler kanalına kayıt al; tekrar `I` kaydı durdurup kanala yükler |
| `M` | MIDI girişini seç |
| `R` | adım girişi: MIDI tuşları imleçteki adımı yazar (birlikte basılan tuşlar akor olur) |
| `w` `W` `e` | kaydet, farklı kaydet, WAV dışa aktar |
| `q` | çık |

Fare de çalışır. Tıklamak bir adımı açıp kapatır, bir kanalı seçer ya da onu susturur, solo yapar (`M` `S`); piyano rulosunda nota, otomasyon görünümünde nokta ekler ya da siler. Bir kanalın adımları üzerinde sürüklemek onları doldurur ya da temizler; piyano rulosunda bir notadan sürüklemek uzunluğunu ayarlar. Tekerlek, işaretçinin altındaki değeri değiştirir: üst çubukta tempo, swing, pattern ve uzunluk, bir parametre, bir nokta, bir şarkı girişi. Sağ tuş yalnızca imleci oraya götürür ya da parametreyi varsayılanına döndürür. Terminalde metin seçmek için Shift'e basılı tut (çoğu terminalde) ya da `mouse = off` yap.

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
```

`[keys]` altında yazılan bir eylem yalnızca verilen tuşlara bağlanır; değeri boş bırakılırsa hiç tuşu kalmaz. `base` hazır temalardan birini seçer (`default` ya da `light`). Her rolün stili `[renk] [on renk] [bold]` biçimindedir; renk bir isim, `0`–`255` arası palet numarası ya da `#rrggbb` olabilir. `tinvo settings` dosyayı denetler ve bütün eylemleri ve rolleri şu anki değerleriyle yazdırır; çıktısı başlamak için hazır, eksiksiz bir ayar dosyasıdır. Dosyada bir hata varsa TINVO açılmadan durur ve sorunu söyler: hatalı satırı, ya da aynı görünümde çalışan iki eyleme aynı tuş verildiyse o tuşu ve iki eylemi. Sorulara verilen cevaplar (`y`, `n`, `Esc`) ve metin kutularındaki düzenleme tuşları sabittir. `mouse = off` fareyi terminale bırakır.

### Katkı

Hata bildirimleri ve pull request'ler memnuniyetle karşılanır. Sıfır bağımlılık kuralı dahil tüm kurallar [CONTRIBUTING.md](CONTRIBUTING.md) dosyasında (İngilizce). Her değişiklik `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` ve `cargo test` adımlarından geçmeli. Dosya formatlarına ya da düzenleme modeline dokunan bir değişiklik birkaç dakikalık `tinvo fuzz` çalışmasından da sağ çıkmalı.

### Lisans

MIT. Ayrıntılar [LICENSE](LICENSE) dosyasında. Yazar: 𝓐.𝓒.𝓑

[⬆ Başa Dön](#top)
