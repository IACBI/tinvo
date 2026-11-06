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

TINVO is a step-sequencer DAW that runs in a terminal window. Everything is written in Rust on top of the standard library alone: the synthesizer and drums, the real-time engine, the WAV writer, and the audio backends (WASAPI, ALSA, CoreAudio) are called through hand-written FFI. There are no crates to download, so it builds offline. The release binary is about 0.5 MB.

The audio thread never allocates, locks or blocks. WAV export runs the same engine as playback, so what you hear is what you export.

### Features

- 16 tracks, up to 64 sixteenth-note steps, tempo 20–300 BPM, swing 50–75 %.
- Per step: a note or a chord of up to 4 notes, velocity, length, chance (1–100 %), ratchets (1–4 hits) and micro-timing (up to half a step early or late).
- A piano roll for melodic tracks.
- 16 patterns (A–P) and a song: a chain of up to 256 patterns with a loop region. Patterns switch at the end of the bar while playing.
- Automation: up to 4 lanes per track for volume, pan or any instrument parameter, holding or ramping between points set per step.
- 8-voice subtractive synth with two band-limited oscillators (pulse width, unison with detune), noise, a state-variable filter, two envelopes, glide, an LFO (to pitch, pulse width, cutoff and level), and velocity → cutoff.
- Synthesized kick, snare, closed and open hi-hat, and clap. The closed hat chokes the open hat.
- An 8-voice sampler that plays WAV files with windowed-sinc interpolation: root note and tuning, start/end points, loop, gate or one-shot, ADSR and a filter.
- Mixer with click-free volume, pan, mute and solo, plus peak meters.
- Undo/redo with a 1000-step history.
- Text `.tvo` project files that survive a save/load round trip exactly. Saves are atomic, and older format versions still load.
- WAV export at 16 or 24 bit (dithered) or 32-bit float, with clipping reported: the song, one pattern or the loop region, optionally as per-track stems.

The roadmap is in [docs/ROADMAP.md](docs/ROADMAP.md) and the design is described in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

### Requirements

- Rust 1.85 or newer.
- Linux: the ALSA development package (`libasound2-dev` on Debian/Ubuntu, `alsa-lib-devel` on Fedora).
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
tinvo bench               # measure rendering speed
```

The main keys in the editor (press `?` for the full list):

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
| `A` | automate the selected parameter (panel) / show the automation lanes (grid) |
| in the automation view | `↑` `↓` lane, `Enter`/`x` add or remove a point, `[` `]` `{` `}` its value, `/` hold or ramp, `-` free the lane |
| `a` `D` `r` `m` `s` | add, delete, rename, mute, solo a track |
| `o` | load a WAV file into a sampler track (`a` `7` adds one) |
| `w` `W` `e` | save, save as, export WAV |
| `q` | quit |

### Contributing

Bug reports and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the rules, including the zero-dependency rule. A change has to pass `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`.

### License

MIT. See [LICENSE](LICENSE). Author: 𝓐.𝓒.𝓑

[⬆ Back to top](#top)

---
<a id="turkce"></a>
## Türkçe

### Genel Bakış

TINVO, terminal penceresinde çalışan, adım sıralayıcı (step sequencer) tabanlı bir DAW. Synth ve davullar, gerçek zamanlı motor ve WAV yazıcı Rust'ın standart kütüphanesinden başka hiçbir şey kullanmadan yazıldı. Ses altyapıları (WASAPI, ALSA, CoreAudio) elle yazılmış FFI ile çağrılıyor. İndirilecek tek bir crate yok, bu yüzden internet olmadan da derleniyor. Release binary'si yaklaşık 0,5 MB.

Ses thread'i hiçbir zaman bellek ayırmaz, kilit almaz, beklemez. WAV dışa aktarma çalmayla aynı motoru kullanır: ne duyuyorsan dosyaya o yazılır.

### Özellikler

- 16 kanal, 64'e kadar 16'lık adım, 20–300 BPM tempo, %50–75 swing.
- Her adımda bir nota ya da en fazla 4 notalık akor, velocity, uzunluk, olasılık (%1–100), ratchet (1–4 vuruş) ve mikro zamanlama (yarım adıma kadar erken ya da geç).
- Melodik kanallar için piyano rulosu.
- 16 pattern (A–P) ve şarkı: döngü bölgesi olan, en fazla 256 pattern'lik bir zincir. Çalarken pattern değişikliği ölçü sonunda devreye girer.
- Otomasyon: her kanalda ses seviyesi, pan ya da herhangi bir enstrüman parametresi için en fazla 4 şerit. Adım adım konan noktalar arasında değer ya sabit kalır ya da kayarak geçer.
- 8 sesli subtractive synth: iki band-limited osilatör (pulse width, detune'lu unison), noise, state-variable filtre, iki zarf, glide, LFO (pitch, pulse width, cutoff ve seviyeye) ve velocity → cutoff.
- Sentezlenmiş kick, snare, kapalı ve açık hi-hat, clap. Kapalı hat açık hat'i susturur (choke).
- WAV dosyalarını windowed-sinc interpolasyonla çalan 8 sesli sampler: kök nota ve akort, başlangıç/bitiş noktası, loop, gate ya da one-shot, ADSR ve filtre.
- Tıklamasız ses, pan, mute ve solo; seviye göstergeleri.
- 1000 adımlık geri al / yinele.
- Kaydet/aç döngüsünde hiçbir değeri değiştirmeyen metin tabanlı `.tvo` proje dosyaları. Kayıtlar atomik, eski format sürümleri de açılıyor.
- 16 veya 24 bit (dither'lı) ya da 32-bit float WAV dışa aktarma; clipping olursa raporlanır. Şarkının tamamı, tek bir pattern ya da döngü bölgesi aktarılabilir, istenirse her kanal ayrı dosyaya (stem).

Yol haritası [docs/ROADMAP.md](docs/ROADMAP.md), mimari [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) dosyasında (İngilizce).

### Gereksinimler

- Rust 1.85 veya üstü.
- Linux: ALSA geliştirme paketi (Debian/Ubuntu'da `libasound2-dev`, Fedora'da `alsa-lib-devel`).
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
tinvo bench               # measure rendering speed
```

Editördeki temel tuşlar (tam liste için `?`):

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
| `A` | seçili parametreyi otomasyona al (panel) / otomasyon şeritlerini göster (ızgara) |
| otomasyon görünümünde | `↑` `↓` şerit, `Enter`/`x` nokta ekle ya da sil, `[` `]` `{` `}` değeri, `/` sabit ya da kayan, `-` şeridi boşalt |
| `a` `D` `r` `m` `s` | kanal ekle, sil, yeniden adlandır, mute, solo |
| `o` | sampler kanalına WAV dosyası yükle (`a` `7` sampler kanalı ekler) |
| `w` `W` `e` | kaydet, farklı kaydet, WAV dışa aktar |
| `q` | çık |

### Katkı

Hata bildirimleri ve pull request'ler memnuniyetle karşılanır. Sıfır bağımlılık kuralı dahil tüm kurallar [CONTRIBUTING.md](CONTRIBUTING.md) dosyasında (İngilizce). Her değişiklik `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` ve `cargo test` adımlarından geçmeli.

### Lisans

MIT. Ayrıntılar [LICENSE](LICENSE) dosyasında. Yazar: 𝓐.𝓒.𝓑

[⬆ Başa Dön](#top)
