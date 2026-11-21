//! TINVO command-line entry point.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tinvo::app::MidiStart;
use tinvo::audio::{self, AudioCallback, StreamConfig};
use tinvo::bench;
use tinvo::dsp::loudness::ExportMeter;
use tinvo::midi;
use tinvo::project::{Project, pattern_name};
use tinvo::record::Recorder;
use tinvo::render::{self, RenderOptions, Source};
use tinvo::settings::{self, Settings};
use tinvo::smf;
use tinvo::tui::printable_line as shown;
use tinvo::wav::{self, SampleFormat};

/// Ends a usage error: the whole usage would push the error off a small
/// terminal.
const SEE_HELP: &str = "; tinvo --help shows the usage";

const USAGE: &str = "\
TINVO - terminal digital audio workstation

USAGE:
    tinvo [AUDIO] [PROJECT.tvo]         open the editor (demo beat if no file)
    tinvo render PROJECT OUT.wav [--format 16|24|32f] [--loops N] [--rate HZ]
                 [--song | --region | --pattern A-P | [--from MARKER] [--to MARKER]] [--stems]
                                        export WAV (the song if it has entries, else
                                        pattern A); --from alone is that marker's section,
                                        --to stops where its marker starts; --stems also
                                        writes one file per track
    tinvo render PROJECT OUT.mid [--loops N] [--song | --region | --pattern A-P | ...]
                                        export a Standard MIDI File instead (one pass
                                        unless --loops, at most 32)
    tinvo import SONG.mid OUT.tvo       read a MIDI file (format 0 or 1) into a new project;
                                        `tinvo SONG.mid` opens it in the editor unsaved
    tinvo sfz INSTRUMENT.sfz            check an SFZ instrument: what the sampler plays of
                                        it and what it leaves out (o on a sampler track
                                        imports it)
    tinvo slice SAMPLE.wav [N | t [S]]  where the slicer cuts a sample: into N equal parts,
                                        or at its transients with sensitivity S from 0 to
                                        100 (50 by default; % on a sampler track slices)
    tinvo demo OUT.tvo                  write the demo project to a file
    tinvo devices                       list the audio output devices
    tinvo midi                          list the MIDI inputs and outputs
    tinvo inputs                        list the audio input devices
    tinvo record OUT.wav [SECONDS] [--input NAME]
                                        record from an audio input (default 5 s) to a
                                        32-bit float WAV file
    tinvo probe [AUDIO] [SECONDS]       open the audio device, output silence, report timing
    tinvo loopback [AUDIO] [--input NAME]
                                        play five short sweeps on the output and find
                                        them on the input: checks both end to end and
                                        measures the round-trip latency
    tinvo bench [--check]               time each scenario in 128-frame callbacks; --check
                                        fails when one is over its time budget
    tinvo bench --instructions FILE     count instructions per scenario with Valgrind
                                        (x86-64 Linux) against the budgets in FILE
    tinvo fuzz project|wav|edits|midi|sfz [--seconds N | --runs N] [--seed N] [--render-every N]
                                        fuzz what TINVO reads (default 60 s); a failing
                                        input is reduced and saved to a file
    tinvo fuzz TARGET --check FILE      check one input, such as a saved failure
    tinvo plugins [--rescan]            list the CLAP plugins in CLAP's folders and in
                                        CLAP_PATH; files read before are taken from a
                                        cache until they change, unless --rescan
    tinvo settings [FILE]               check the settings file (keys and colours) and print
                                        every setting it gives, ready to copy and edit
    tinvo --help | --version

AUDIO:
    --device NAME                       play on this device (its id, or a unique part of
                                        its name; default: the system default)
    --low-latency                       use the device's smallest period where the
                                        backend supports it (WASAPI, CoreAudio)
    --midi NAME | none                  play from this MIDI input (its id, or a unique
                                        part of its name; default: the first one)
    --midi-out NAME                     send MIDI-out tracks and the clock to this MIDI
                                        output (Z in the editor; default: none)
    --input NAME                        record from this audio input (I in the editor;
                                        default: the system default)
    --settings FILE                     read keys and colours from FILE instead of the
                                        settings file (see tinvo settings); the editor's
                                        settings page (F2) writes its changes there
";

fn main() -> ExitCode {
    // The child a plugin scan runs for each file: its paths need not be
    // Unicode, so they are read before the arguments become strings.
    let mut raw = std::env::args_os().skip(1);
    if raw.next().is_some_and(|a| a == "scan-clap") {
        let rest: Vec<std::ffi::OsString> = raw.collect();
        return match rest.as_slice() {
            // SAFETY: this process is the child a scan started for this file.
            [file, result] => ExitCode::from(unsafe { tinvo::clap::scan::worker(file.as_ref(), result.as_ref()) }),
            _ => ExitCode::from(2),
        };
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None => cmd_edit(&[]),
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("tinvo {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("render") => cmd_render(&args[1..]),
        Some("demo") => cmd_demo(&args[1..]),
        Some("probe") => cmd_probe(&args[1..]),
        Some("loopback") => cmd_loopback(&args[1..]),
        Some("plugins") => cmd_plugins(&args[1..]),
        Some(c @ ("devices" | "midi" | "inputs")) if args.len() > 1 => {
            Err(format!("{c} takes no arguments, not '{}'{SEE_HELP}", shown(&args[1])))
        }
        Some("devices") => cmd_devices(),
        Some("midi") => cmd_midi(),
        Some("inputs") => cmd_inputs(),
        Some("record") => cmd_record(&args[1..]),
        Some("bench") => cmd_bench(&args[1..]),
        Some("fuzz") => cmd_fuzz(&args[1..]),
        Some("settings") => cmd_settings(&args[1..]),
        Some("import") => cmd_import(&args[1..]),
        Some("sfz") => cmd_sfz(&args[1..]),
        Some("slice") => cmd_slice(&args[1..]),
        Some(_) => cmd_edit(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", tinvo::tui::printable(&e));
            ExitCode::FAILURE
        }
    }
}

fn cmd_render(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut format = SampleFormat::Pcm24;
    let mut opts = RenderOptions::default();
    let mut stems = false;
    let (mut from, mut to) = (None, None);
    // Options that only a WAV file takes, and whether --loops was given.
    let mut wav_only = None;
    let mut loops_given = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if matches!(a.as_str(), "--format" | "--rate" | "--stems") {
            wav_only = Some(a.clone());
        }
        loops_given |= a == "--loops";
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--format" => {
                let v = value("--format")?;
                format =
                    SampleFormat::from_id(&v).ok_or(format!("unknown format '{}' (use 16, 24 or 32f)", shown(&v)))?;
            }
            "--loops" => {
                opts.loops = value("--loops")?
                    .parse()
                    .ok()
                    .filter(|n| (1..=1000).contains(n))
                    .ok_or("--loops must be 1..=1000")?;
            }
            "--song" | "--region" | "--pattern" if opts.source != Source::Auto => {
                return Err(format!("{}: --song, --region and --pattern each pick what to render; give one", shown(a)));
            }
            "--song" => opts.source = Source::Song,
            "--region" => opts.source = Source::Region,
            "--pattern" => {
                let v = value("--pattern")?;
                let p = match v.as_bytes() {
                    [c @ b'A'..=b'P'] => c - b'A',
                    _ => return Err(format!("--pattern takes a letter A-P, not '{}'", shown(&v))),
                };
                opts.source = Source::Pattern(p);
            }
            "--from" => from = Some(value("--from")?),
            "--to" => to = Some(value("--to")?),
            "--stems" => stems = true,
            "--rate" => {
                opts.sample_rate = value("--rate")?
                    .parse()
                    .ok()
                    .filter(|r| (8_000..=192_000).contains(r))
                    .ok_or("--rate must be 8000..=192000")?;
            }
            s if s.starts_with('-') => return Err(format!("unknown option '{}'", shown(s))),
            _ => positional.push(a.clone()),
        }
    }
    let [project, out] = positional.as_slice() else {
        return Err(format!("render needs PROJECT and OUT.wav (or OUT.mid){SEE_HELP}"));
    };
    let project_path = std::path::Path::new(project);
    let mut project = Project::load(project_path)?;
    render::check_destination(&project, Some(project_path), out.as_ref()).map_err(|e| format!("{} {e}", shown(out)))?;
    let mut range_name = None;
    if from.is_some() || to.is_some() {
        if opts.source != Source::Auto {
            return Err(
                "--from and --to pick part of the song; they do not go with --song, --region or --pattern".into()
            );
        }
        // The range plays as the loop region does.
        let range = project.marker_range(from.as_deref(), to.as_deref()).map_err(|e| shown(&e))?;
        range_name = project.range_name(range);
        project.loop_region = Some(range);
        opts.source = Source::Region;
    }
    match (opts.source, render::resolve(&project, opts.source)) {
        (Source::Song, Source::Pattern(_)) => return Err("the song is empty".into()),
        (Source::Region, Source::Pattern(_)) => return Err("the project has no loop region inside the song".into()),
        _ => {}
    }
    let what = match render::resolve(&project, opts.source) {
        Source::Song => "the song".to_string(),
        Source::Region => range_name.unwrap_or("the loop region".to_string()),
        Source::Pattern(p) => format!("pattern {}", pattern_name(p)),
        Source::Auto => unreachable!("resolved"),
    };
    if smf::is_midi_path(out.as_ref()) {
        if let Some(flag) = wav_only {
            return Err(format!("{flag} is for WAV files; {} is a MIDI file", shown(out)));
        }
        let loops = if loops_given { opts.loops } else { 1 };
        if loops > smf::MAX_LOOPS {
            return Err(format!("a MIDI file takes at most {} loops", smf::MAX_LOOPS));
        }
        smf::save(&project, opts.source, loops, out.as_ref()).map_err(|e| format!("{}: {e}", shown(out)))?;
        let passes = if loops == 1 { String::new() } else { format!(" {loops} times") };
        println!("Wrote {what}{passes} as a MIDI file to {}", shown(out));
        return Ok(());
    }
    let (bank, errors) = project.load_samples();
    let track_samples = project.sample_maps(&bank);
    if !errors.is_empty() {
        return Err(format!("cannot load samples:\n  {}", errors.join("\n  ")));
    }
    // Every stem's file is checked before the mix is written.
    let out_path = std::path::Path::new(out);
    let stem = out_path.file_stem().map_or("out".into(), |s| s.to_string_lossy().into_owned());
    let stem_paths: Vec<_> = if stems {
        render::stems(&project)
            .into_iter()
            .map(|part| (out_path.with_file_name(format!("{stem}-{}.wav", part.file_part(&project))), part))
            .collect()
    } else {
        Vec::new()
    };
    for (path, _) in &stem_paths {
        render::check_destination(&project, Some(project_path), path)
            .map_err(|e| format!("{} {e}", shown(&path.display().to_string())))?;
    }
    let mut plugins = plugin_source(&project)?;
    let start = Instant::now();
    let mut file = wav::FileWriter::create(out.as_ref(), 2, opts.sample_rate, format)?;
    let mut meter = ExportMeter::new(opts.sample_rate);
    let frames = render::render_into(&project, &track_samples, &opts, &mut *plugins, &mut |b| {
        meter.push(b);
        file.write(b)
    })?;
    let elapsed = start.elapsed();
    // Every file is written whole before any is put in place, so a stem
    // that fails leaves the mix as it was too.
    let mut files = Vec::new();
    for (path, part) in stem_paths {
        let mut file = wav::FileWriter::create(&path, 2, opts.sample_rate, format)?;
        let stem = render::stem_project(&project, part);
        render::render_into(&stem, &track_samples, &opts, &mut *plugins, &mut |b| file.write(b))?;
        files.push((path, file));
    }
    // Stems end at different times; padded to one length they line up.
    let len = files.iter().map(|(_, f)| f.frames()).max().unwrap_or(0);
    let silence = [0.0f32; 2 * 1024];
    for (_, file) in &mut files {
        while file.frames() < len {
            let n = (len - file.frames()).min(1024) as usize;
            file.write(&silence[..2 * n])?;
        }
    }
    let stats = file.finish()?;
    let secs = frames as f64 / f64::from(opts.sample_rate);
    println!(
        "Rendered {what}, {secs:.2} s ({}, {} Hz), to {} in {:.3} s; {}; sample peak {:.2} dBFS; {} clipped samples",
        format.label(),
        opts.sample_rate,
        shown(out),
        elapsed.as_secs_f64(),
        meter.finish().summary(),
        tinvo::dsp::gain_to_db(stats.peak),
        stats.clipped
    );
    for (path, file) in files {
        let st = file.finish()?;
        println!("  stem {}: peak {:.2} dBFS", shown(&path.display().to_string()), tinvo::dsp::gain_to_db(st.peak));
    }
    Ok(())
}

/// Reads a MIDI file into a new project file, which must not exist yet.
fn cmd_import(args: &[String]) -> Result<(), String> {
    let [input, out] = args else { return Err(format!("import needs SONG.mid and OUT.tvo{SEE_HELP}")) };
    let out_path = std::path::Path::new(out);
    if out_path.exists() {
        return Err(format!("{} exists; import writes a new project", shown(out)));
    }
    let imported = smf::load(input.as_ref())?;
    // Written beside it and given its name only if that is still free.
    let failed = |e: std::io::Error| format!("{}: {e}", shown(out));
    let (mut file, temp) = tinvo::fsutil::create_temp_beside(out_path).map_err(failed)?;
    let written = std::io::Write::write_all(&mut file, imported.project.to_text().as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            tinvo::fsutil::rename_new(&temp, out_path)
        });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} exists; import writes a new project", shown(out))
        } else {
            failed(e)
        });
    }
    println!("Imported {} to {}: {}", shown(input), shown(out), imported.summary());
    Ok(())
}

fn cmd_sfz(args: &[String]) -> Result<(), String> {
    let [input] = args else { return Err(format!("sfz needs one INSTRUMENT.sfz{SEE_HELP}")) };
    let path = std::path::Path::new(input);
    let inst = tinvo::sfz::read_file(path).map_err(|e| format!("{}: {e}", shown(input)))?;
    let dir = path.parent().unwrap_or(std::path::Path::new(""));
    // Each file once, by the name the editor gives it, and its header as
    // the editor reads it.
    let key = |r: &tinvo::sfz::Region| tinvo::sample::absolute(&dir.join(&r.path).to_string_lossy());
    let mut files = std::collections::BTreeMap::new();
    for r in &inst.regions {
        files.entry(key(r)).or_insert_with_key(|k| tinvo::sfz::FileInfo::read(k));
    }
    let mut skipped = std::collections::BTreeMap::<&str, usize>::new();
    let mut playing = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    for r in &inst.regions {
        let k = key(r);
        if let Ok(info) = &files[&k] {
            match r.resolve(info) {
                Ok(place) => {
                    playing.push((r, place));
                    used.insert(k);
                }
                Err(why) => *skipped.entry(why).or_default() += 1,
            }
        }
    }
    let (n, total) = (playing.len(), inst.regions.len());
    let good = files.values().filter(|f| f.is_ok()).count();
    let keys = playing.iter().fold(None, |k: Option<(u8, u8)>, (_, p)| {
        Some(k.map_or(p.keys, |(lo, hi)| (lo.min(p.keys.0), hi.max(p.keys.1))))
    });
    let over = keys.map_or_else(String::new, |(lo, hi)| format!(" over keys {lo} to {hi}"));
    println!("{}: {n} of {total} regions play{over}, from {good} of {} files", shown(input), files.len());
    for (f, e) in files.iter().filter_map(|(f, r)| r.as_ref().err().map(|e| (f, e))) {
        println!("- not loaded: {}: {}", tinvo::tui::printable(&f.display().to_string()), tinvo::tui::printable(e));
    }
    for (why, n) in skipped {
        println!("- {n} {} skipped: {why}", if n == 1 { "region" } else { "regions" });
    }
    // What the editor would load: the files a region plays.
    let frames: u64 = used.iter().filter_map(|k| files[k].as_ref().ok()).map(|f| f.frames).sum();
    if frames > tinvo::sample::PROJECT_FRAMES as u64 {
        println!(
            "- {frames} frames in all, more than a project holds ({}): the editor loads files in order until they fill it",
            tinvo::sample::PROJECT_FRAMES
        );
    }
    if let Some((r, _)) = playing.first() {
        let e = r.envelope;
        println!(
            "- envelope: attack {} s, decay {} s, sustain {}%, release {} s",
            e.attack,
            e.decay,
            e.sustain * 100.0,
            e.release
        );
    }
    for note in &inst.notes {
        println!("- {}", tinvo::tui::printable(note));
    }
    Ok(())
}

fn cmd_slice(args: &[String]) -> Result<(), String> {
    use tinvo::slice::{FIRST_KEY, How, MAX_SLICES};
    let (input, how) = match args {
        [input] => (input, How::Transients(tinvo::slice::DEFAULT_SENSITIVITY)),
        [input, how @ ..] if !how.is_empty() => (input, How::parse(&how.join(" "))?),
        _ => return Err(format!("slice needs a SAMPLE.wav{SEE_HELP}")),
    };
    let s = tinvo::sample::load(std::path::Path::new(input))?;
    let starts = tinvo::slice::starts(s.audio(), s.sample_rate, 0, s.frames(), how)?;
    let how = match how {
        How::Parts(_) => "equal parts".to_string(),
        How::Transients(x) => format!("at transients, sensitivity {x}"),
    };
    println!("{}: {} slices ({how}; at most {MAX_SLICES})", shown(input), starts.len());
    println!("  key   start frame  length   at");
    let sr = f64::from(s.sample_rate);
    let ends = starts.iter().skip(1).copied().chain(std::iter::once(s.frames()));
    for (k, (&start, end)) in starts.iter().zip(ends).enumerate() {
        let key = tinvo::instrument::note_name(FIRST_KEY + k as u8);
        println!("  {key:<5} {start:>11}  {:>7.3} s  {:.3} s", (end - start) as f64 / sr, start as f64 / sr);
    }
    Ok(())
}

fn cmd_demo(args: &[String]) -> Result<(), String> {
    let [out] = args else { return Err("demo needs exactly one output path".into()) };
    Project::demo().save(out.as_ref()).map_err(|e| format!("{out}: {e}"))?;
    println!("Wrote demo project to {}", shown(out));
    Ok(())
}

/// Outputs silence through the real device and reports callback timing.
/// Takes the audio options out of `args`, returning the rest.
fn audio_options(args: &[String]) -> Result<(StreamConfig, Vec<String>), String> {
    let mut config = StreamConfig::default();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => {
                let query = it.next().ok_or("--device needs a device name or id")?;
                let devices = audio::output_devices()?;
                config.device = Some(audio::find_device(&devices, query)?.id.clone());
            }
            "--low-latency" => config.low_latency = true,
            _ => rest.push(a.clone()),
        }
    }
    Ok((config, rest))
}

/// Takes `--midi` out of `args`, returning the rest.
fn midi_option(args: Vec<String>) -> Result<(MidiStart, Vec<String>), String> {
    let mut start = MidiStart::First;
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a != "--midi" {
            rest.push(a);
            continue;
        }
        let query = it.next().ok_or("--midi needs an input name or id, or none")?;
        start = if query == "none" {
            MidiStart::Off
        } else {
            let ports = midi::inputs()?;
            MidiStart::Port(midi::find_port(&ports, &query)?.id.clone())
        };
    }
    Ok((start, rest))
}

/// Takes `--midi-out` out of `args`, returning the output id and the rest.
fn midi_out_option(args: Vec<String>) -> Result<(Option<String>, Vec<String>), String> {
    let mut out = None;
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a != "--midi-out" {
            rest.push(a);
            continue;
        }
        let query = it.next().ok_or("--midi-out needs an output name or id")?;
        let ports = midi::outputs()?;
        out = Some(midi::find_output(&ports, &query)?.id.clone());
    }
    Ok((out, rest))
}

/// Takes `--input` out of `args`, returning the input id and the rest.
fn input_option(args: Vec<String>) -> Result<(Option<String>, Vec<String>), String> {
    let mut input = None;
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--input" {
            let query = it.next().ok_or("--input needs a device name or id")?;
            let devices = audio::input_devices()?;
            input = Some(audio::find_device(&devices, &query)?.id.clone());
        } else {
            rest.push(a);
        }
    }
    Ok((input, rest))
}

/// Takes `--settings` out of `args`: the settings file to read, if not the
/// usual one.
fn settings_option(args: Vec<String>) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut file = None;
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--settings" {
            file = Some(PathBuf::from(it.next().ok_or("--settings needs a file")?));
        } else {
            rest.push(a);
        }
    }
    Ok((file, rest))
}

/// The settings in `file`, or in the usual settings file; the path read
/// and whether a file was there. A named file must exist.
fn read_settings(file: Option<PathBuf>) -> Result<(Settings, Option<PathBuf>, bool), String> {
    match file {
        Some(f) => {
            let (s, found) = settings::load(&f)?;
            if !found {
                return Err(format!("{}: no such settings file", f.display()));
            }
            Ok((s, Some(f), true))
        }
        None => match settings::default_path() {
            Some(p) => settings::load(&p).map(|(s, found)| (s, Some(p), found)),
            None => Ok((Settings::default(), None, false)),
        },
    }
}

fn cmd_edit(args: &[String]) -> Result<(), String> {
    // Settings first: a mistake in them is reported before any device opens.
    let (file, rest) = settings_option(args.to_vec())?;
    let (settings, settings_path, _) = read_settings(file)?;
    let (config, rest) = audio_options(&rest)?;
    let (midi_start, rest) = midi_option(rest)?;
    let (midi_out, rest) = midi_out_option(rest)?;
    let (input, rest) = input_option(rest)?;
    let path = match &rest[..] {
        [] => None,
        [s] if s.starts_with('-') => return Err(format!("unknown option '{}'{SEE_HELP}", shown(s))),
        [path] => Some(PathBuf::from(path)),
        _ => return Err(format!("too many arguments{SEE_HELP}")),
    };
    // SAFETY: the program's main thread; the plugins are the ones installed
    // where CLAP says to look, which the project or the user asks to play.
    unsafe { tinvo::app::run(path, config, midi_start, midi_out, input, settings, settings_path) }
}

/// Checks the settings file and prints every setting it gives.
fn cmd_settings(args: &[String]) -> Result<(), String> {
    let file = match args {
        [] => None,
        [f] if !f.starts_with('-') => Some(PathBuf::from(f)),
        _ => return Err(format!("settings takes at most a file{SEE_HELP}")),
    };
    let (settings, path, found) = read_settings(file)?;
    let source = match (&path, found) {
        (Some(p), true) => format!("# Read from {}.", shown(&p.display().to_string())),
        (Some(p), false) => format!(
            "# {} does not exist: these are the defaults. Copy there the lines to change.",
            shown(&p.display().to_string())
        ),
        (None, _) => "# No settings folder could be found (no APPDATA or HOME): these are the defaults.".to_string(),
    };
    print!("{source}\n{}", settings.to_text());
    Ok(())
}

fn cmd_inputs() -> Result<(), String> {
    let devices = audio::input_devices()?;
    if devices.is_empty() {
        println!("No input devices.");
    }
    for d in &devices {
        println!("{} {}\n    {}", if d.is_default { "*" } else { " " }, shown(&d.name), shown(&d.id));
    }
    if devices.iter().any(|d| d.is_default) {
        println!("(* is the system default)");
    }
    Ok(())
}

fn cmd_record(args: &[String]) -> Result<(), String> {
    let (input, rest) = input_option(args.to_vec())?;
    let (out, seconds) = match &rest[..] {
        [out] => (out, 5.0),
        [out, s] => (out, s.parse::<f64>().map_err(|_| format!("'{}' is not a number of seconds", shown(s)))?),
        _ => return Err(format!("record needs an output file{SEE_HELP}")),
    };
    if !(seconds > 0.0 && seconds <= 3600.0) {
        return Err("record length must be between 0 and 3600 seconds".into());
    }
    let path = PathBuf::from(out);
    let mut r = Recorder::start(input.as_deref(), &path)?;
    println!("Recording {seconds} s from {} ...", shown(r.device()));
    let start = Instant::now();
    let mut peak = 0.0f32;
    while start.elapsed().as_secs_f64() < seconds {
        if r.drain()? {
            break;
        }
        peak = peak.max(r.take_peak());
        std::thread::sleep(Duration::from_millis(20));
    }
    peak = peak.max(r.take_peak());
    let rec = r.finish()?;
    let peak_db = if peak > 0.0 { format!("{:.1} dBFS", 20.0 * peak.log10()) } else { "silence".into() };
    println!(
        "Wrote {}: {:.2} s, {} Hz, {} channel(s), peak {peak_db}, {} frames dropped",
        shown(&rec.path.display().to_string()),
        rec.seconds(),
        rec.sample_rate,
        rec.channels,
        rec.dropped
    );
    Ok(())
}

fn cmd_midi() -> Result<(), String> {
    for (what, ports) in [("inputs", midi::inputs()?), ("outputs", midi::outputs()?)] {
        if ports.is_empty() {
            println!("No MIDI {what}.");
        } else {
            println!("MIDI {what}:");
        }
        for p in &ports {
            println!("  {}\n    {}", shown(&p.name), shown(&p.id));
        }
    }
    Ok(())
}

fn cmd_devices() -> Result<(), String> {
    let devices = audio::output_devices()?;
    if devices.is_empty() {
        println!("No output devices.");
    }
    for d in &devices {
        println!("{} {}\n    {}", if d.is_default { "*" } else { " " }, shown(&d.name), shown(&d.id));
    }
    if devices.iter().any(|d| d.is_default) {
        println!("(* is the system default)");
    }
    Ok(())
}

/// What makes the plugins a render of `project` plays: the scan's catalog
/// if it names any.
fn plugin_source(project: &Project) -> Result<Box<dyn render::PluginSource>, String> {
    if project.plugin_places().is_empty() {
        return Ok(Box::new(render::NoPlugins));
    }
    let catalog = tinvo::clap::scan::installed(false, &std::sync::atomic::AtomicBool::new(false))?.0;
    // SAFETY: the program's main thread; the plugins installed where CLAP
    // says to look, which the project asks to run.
    Ok(Box::new(unsafe { render::CatalogPlugins::new(catalog) }))
}

fn cmd_plugins(args: &[String]) -> Result<(), String> {
    let rescan = match args {
        [] => false,
        [a] if a == "--rescan" => true,
        [a, ..] => return Err(format!("plugins takes only --rescan, not '{}'{SEE_HELP}", shown(a))),
    };
    let (catalog, roots) = tinvo::clap::scan::installed(rescan, &std::sync::atomic::AtomicBool::new(false))?;
    let path = |p: &std::path::Path| shown(&p.display().to_string());
    let plugins = catalog.plugins();
    for (file, p) in &plugins {
        let mut title = shown(&p.name);
        for (before, text) in [("", &p.version), ("by ", &p.vendor)].into_iter().filter(|(_, t)| !t.is_empty()) {
            title.push_str(&format!(" {before}{}", shown(text)));
        }
        println!("{title} ({})\n    {}\n    {}", p.roles.label(), shown(&p.id), path(file));
    }
    if plugins.is_empty() {
        println!("No CLAP plugins found. Searched:");
        for root in &roots {
            println!("    {}", path(root));
        }
    }
    let failures = catalog.failures();
    if !failures.is_empty() {
        println!("\nNot loaded:");
        for (file, why) in failures {
            println!("  {}\n    {}", path(file), shown(why));
        }
    }
    for (id, used, other) in catalog.duplicates() {
        println!("\n{} is in two files: {} is used, not {}", shown(id), path(used), path(other));
    }
    match catalog.left {
        0 => {}
        1 => println!("\n1 file is not read yet; tinvo plugins reads it next time."),
        n => println!("\n{n} files are not read yet; tinvo plugins reads them next time."),
    }
    for note in &catalog.notes {
        eprintln!("note: {}", shown(note));
    }
    Ok(())
}

fn cmd_probe(args: &[String]) -> Result<(), String> {
    let (config, args) = audio_options(args)?;
    let seconds: f64 = match args.as_slice() {
        [s] => s.parse().ok().filter(|v| *v > 0.0 && *v <= 60.0).ok_or("SECONDS must be in (0, 60]")?,
        [] => 2.0,
        _ => return Err(format!("probe takes at most SECONDS{SEE_HELP}")),
    };
    struct Silence {
        frames: Arc<AtomicU64>,
        calls: Arc<AtomicU64>,
        longest_gap_ns: Arc<AtomicU64>,
        last: Option<Instant>,
    }
    impl AudioCallback for Silence {
        fn process(&mut self, out: &mut [f32]) {
            out.fill(0.0);
            let now = Instant::now();
            if let Some(last) = self.last {
                let gap = now.duration_since(last).as_nanos().min(u128::from(u64::MAX)) as u64;
                self.longest_gap_ns.fetch_max(gap, Ordering::Relaxed);
            }
            self.last = Some(now);
            self.frames.fetch_add(out.len() as u64 / 2, Ordering::Relaxed);
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }
    let frames = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(AtomicU64::new(0));
    let longest_gap_ns = Arc::new(AtomicU64::new(0));
    let (f, c, g) = (frames.clone(), calls.clone(), longest_gap_ns.clone());
    let stream = audio::open_with(&config, move |_| Silence { frames: f, calls: c, longest_gap_ns: g, last: None })?;
    let info = stream.info().clone();
    let ms = |frames: u32| f64::from(frames) * 1000.0 / f64::from(info.sample_rate);
    println!(
        "Opened {} on {}: {} Hz, {} channels, {} format, {}-frame buffer ({:.1} ms), {}-frame period ({:.2} ms){}",
        info.backend,
        if info.device.is_empty() { "the default device".into() } else { shown(&info.device) },
        info.sample_rate,
        info.device_channels,
        info.format,
        info.buffer_frames,
        ms(info.buffer_frames),
        info.period_frames,
        ms(info.period_frames),
        match (config.low_latency, info.low_latency) {
            (true, true) => ", low latency",
            (true, false) => ", low latency unavailable",
            _ => "",
        }
    );
    let start = Instant::now();
    let f0 = frames.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs_f64(seconds));
    let elapsed = start.elapsed().as_secs_f64();
    let delivered = frames.load(Ordering::Relaxed) - f0;
    if let Some(e) = stream.error() {
        return Err(format!("stream failed: {e}"));
    }
    drop(stream);
    let rate = delivered as f64 / elapsed;
    println!(
        "{} callbacks, {delivered} frames in {elapsed:.2} s = {rate:.0} frames/s ({:+.2} % vs nominal)",
        calls.load(Ordering::Relaxed),
        (rate / f64::from(info.sample_rate) - 1.0) * 100.0
    );
    let gap = longest_gap_ns.load(Ordering::Relaxed) as f64 / 1e6;
    // A callback that comes later than the buffer lasts has let it run dry.
    let late = if gap > ms(info.buffer_frames) { ", longer than the buffer: the output may have glitched" } else { "" };
    println!("Longest gap between callbacks: {gap:.2} ms{late}");
    Ok(())
}

fn cmd_loopback(args: &[String]) -> Result<(), String> {
    use tinvo::loopback::{self, MARKS, MIN_HEARD};
    use tinvo::record::{self as rec, Meter, RING_FRAMES};
    use tinvo::rt::spsc;
    const SWEEPS: usize = MARKS.len();
    const LEVEL: f32 = 0.25;
    let (config, args) = audio_options(args)?;
    let (input, rest) = input_option(args)?;
    if let Some(a) = rest.first() {
        return Err(format!("unknown argument '{}'{SEE_HELP}", shown(a)));
    }
    let t0 = Instant::now();
    let secs = |t: Instant| t.saturating_duration_since(t0).as_secs_f64();

    // The input runs first, so it is listening when the first sweep plays.
    // Both callbacks note when each buffer passes (the end of a captured
    // one, the start of a played one) to put both streams on one clock.
    let (mut tx, mut rx) = spsc::channel::<[f32; 2]>(RING_FRAMES);
    let (mut in_tx, mut in_rx) = spsc::channel::<(Instant, u64)>(1 << 14);
    let meter = Arc::new(Meter::default());
    let m = meter.clone();
    let instream = audio::open_input(input.as_deref(), move |_, channels| {
        let ch = usize::from(channels);
        let mut frames = 0u64;
        move |samples: &[f32]| {
            let now = Instant::now();
            rec::capture_into(&mut tx, &m, samples, ch);
            frames += (samples.len() / ch) as u64;
            let _ = in_tx.push((now, frames));
        }
    })?;
    let in_info = instream.info().clone();

    struct Sweeps {
        sweep: Vec<f32>,
        starts: [u64; SWEEPS],
        frame: u64,
        times: spsc::Producer<(Instant, u64)>,
        played: Arc<AtomicU64>,
    }
    impl AudioCallback for Sweeps {
        fn process(&mut self, out: &mut [f32]) {
            let _ = self.times.push((Instant::now(), self.frame));
            for (i, f) in out.chunks_exact_mut(2).enumerate() {
                let n = self.frame + i as u64;
                let x = self
                    .starts
                    .iter()
                    .find_map(|&s| n.checked_sub(s).and_then(|k| self.sweep.get(k as usize)))
                    .copied()
                    .unwrap_or(0.0);
                f.fill(x);
            }
            self.frame += out.len() as u64 / 2;
            self.played.store(self.frame, Ordering::Release);
        }
    }
    let (out_tx, mut out_rx) = spsc::channel::<(Instant, u64)>(1 << 14);
    let played = Arc::new(AtomicU64::new(0));
    let p = played.clone();
    let outstream = audio::open_with(&config, move |rate| {
        // Half a second to settle, then the sweeps at the ruler's marks.
        let starts = loopback::offsets(rate).map(|o| u64::from(rate) / 2 + o as u64);
        Sweeps { sweep: loopback::sweep(rate, LEVEL), starts, frame: 0, times: out_tx, played: p }
    })?;
    let out_info = outstream.info().clone();
    let rate = out_info.sample_rate;
    println!(
        "Output: {} on {}, {rate} Hz, {:.2} ms period",
        out_info.backend,
        if out_info.device.is_empty() { "the default device".into() } else { shown(&out_info.device) },
        f64::from(out_info.period_frames) * 1000.0 / f64::from(rate)
    );
    println!(
        "Input:  {} on {}, {} Hz, {} channel(s)",
        in_info.backend,
        shown(&in_info.device),
        in_info.sample_rate,
        in_info.device_channels
    );
    if in_info.sample_rate != rate {
        return Err(format!(
            "the output runs at {rate} Hz and the input at {} Hz; set both to the same rate in the system's sound settings",
            in_info.sample_rate
        ));
    }
    println!("Playing {SWEEPS} short sweeps at -12 dBFS ...");
    let r = u64::from(rate);
    let offsets = loopback::offsets(rate);
    let sweep = loopback::sweep(rate, LEVEL);
    // Up to a second for the last sweep to come back.
    let end = r / 2 + offsets[SWEEPS - 1] as u64 + sweep.len() as u64 + r;
    let deadline = Instant::now() + Duration::from_secs(10);
    // Both channels: the signal may come into either of a stereo input.
    let mut heard: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
    let channels = usize::from(in_info.channels);
    while played.load(Ordering::Acquire) < end {
        if Instant::now() > deadline {
            return Err("the output stopped asking for audio".into());
        }
        if let Some(e) = outstream.error().or_else(|| instream.error()) {
            return Err(format!("stream failed: {e}"));
        }
        while let Some(f) = rx.pop() {
            heard[0].push(f[0]);
            heard[1].push(f[1]);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(outstream);
    drop(instream);
    while let Some(f) = rx.pop() {
        heard[0].push(f[0]);
        heard[1].push(f[1]);
    }
    let (mut in_times, mut out_times) = (Vec::new(), Vec::new());
    while let Some(t) = in_rx.pop() {
        in_times.push(t);
    }
    while let Some(t) = out_rx.pop() {
        out_times.push(t);
    }
    let dropped = meter.dropped.load(Ordering::Relaxed);
    if dropped > 0 {
        return Err(format!("{dropped} input frames were lost; try again"));
    }

    // Where frame 0 of each stream was on the clock: a played frame left
    // TINVO when its buffer was asked for, a captured one arrived when its
    // buffer was handed over. The median steadies the scheduler's jitter.
    let fr = f64::from(rate);
    let out_zero = loopback::median(&mut out_times.iter().map(|&(t, f)| secs(t) - f as f64 / fr).collect::<Vec<_>>());
    let in_zero = loopback::median(&mut in_times.iter().map(|&(t, f)| secs(t) - f as f64 / fr).collect::<Vec<_>>());
    let (Some(out_zero), Some(in_zero)) = (out_zero, in_zero) else {
        return Err("no audio came through".into());
    };
    let slack = rate as usize / 1000;
    // The channel that heard more of the sweeps, or more clearly.
    let mut best = None;
    for (c, samples) in heard.iter().enumerate().take(channels) {
        let train =
            loopback::find_train(samples, &sweep, &offsets, slack).ok_or("the input delivered too little audio")?;
        let peak = samples.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let better = |(_, t, _): &(usize, loopback::Train, f32)| {
            (train.heard_count(), train.train_clarity) > (t.heard_count(), t.train_clarity)
        };
        if best.as_ref().is_none_or(better) {
            best = Some((c, train, peak));
        }
    }
    let (channel, train, peak) = best.ok_or("the input has no channels")?;
    let peak_db = if peak > 0.0 { format!("{:.1} dBFS", 20.0 * peak.log10()) } else { "silence".into() };
    let on = if channels > 1 { format!(" on input channel {}", channel + 1) } else { String::new() };
    println!(
        "Input peak {peak_db}; heard {} of {SWEEPS} sweeps{on} (clarity {:.0})",
        train.heard_count(),
        train.train_clarity
    );
    if !train.is_heard() {
        return Err(format!(
            "fewer than {MIN_HEARD} sweeps were heard: turn the output up, unmute the input, move the microphone nearer, or turn off the input's noise removal"
        ));
    }
    // Only sweeps heard count: a lost one has no peak of its own.
    let mut latencies: Vec<f64> = (0..SWEEPS)
        .filter(|&k| train.heard(k))
        .map(|k| {
            let played_at = out_zero + (r / 2 + offsets[k] as u64) as f64 / fr;
            in_zero + train.starts[k] as f64 / fr - played_at
        })
        .collect();
    let spread = latencies.iter().fold(f64::MIN, |m, &x| m.max(x)) - latencies.iter().fold(f64::MAX, |m, &x| m.min(x));
    let latency = loopback::median(&mut latencies).unwrap_or(0.0);
    if latency < 0.0 {
        return Err(format!(
            "the sweeps were heard {:.1} ms before they were played: something else made them",
            -latency * 1000.0
        ));
    }
    println!(
        "Round trip: {:.1} ms ({:.0} frames); the sweeps agree within {:.2} ms",
        latency * 1000.0,
        latency * fr,
        spread * 1000.0
    );
    Ok(())
}

fn cmd_fuzz(args: &[String]) -> Result<(), String> {
    use tinvo::fuzz::{self, Options, Target};
    let names = Target::ALL.map(Target::id).join(", ");
    let target = args.first().and_then(|a| Target::from_id(a)).ok_or(format!("fuzz needs a target: {names}"))?;
    // A different seed each time explores further; it is printed, so a run
    // can be repeated.
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let mut opts = Options { seed, runs: u64::MAX, time: None, render_every: 16 };
    let mut check = None;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        let mut number = |name: &str| -> Result<u64, String> {
            let v = it.next().ok_or(format!("{name} needs a value"))?;
            v.parse().map_err(|_| format!("{name} takes a whole number, not '{}'", shown(v)))
        };
        match a.as_str() {
            "--seconds" => opts.time = Some(Duration::from_secs(number("--seconds")?)),
            "--runs" => opts.runs = number("--runs")?,
            "--seed" => opts.seed = number("--seed")?,
            "--render-every" => opts.render_every = number("--render-every")?,
            "--check" => check = Some(PathBuf::from(it.next().ok_or("--check needs a file")?)),
            s => return Err(format!("unknown option '{}'", shown(s))),
        }
    }
    if let Some(path) = check {
        let input = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let accepted = fuzz::check(target, &input, true).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("{}", if accepted { "Accepted, and every invariant holds" } else { "Rejected, as it should be" });
        return Ok(());
    }
    if opts.time.is_none() && opts.runs == u64::MAX {
        opts.time = Some(Duration::from_secs(60));
    }
    println!("Fuzzing {} with seed {}", target.id(), opts.seed);
    // A panic is a finding, reported once at the end, not every time the
    // failing input is tried while it is reduced.
    std::panic::set_hook(Box::new(|_| {}));
    let mut last = Instant::now();
    let result = fuzz::run(target, &opts, &mut |r| {
        if last.elapsed() >= Duration::from_secs(10) {
            last = Instant::now();
            println!("  {} inputs, {} accepted, corpus {}", r.runs, r.accepted, r.corpus);
        }
    });
    drop(std::panic::take_hook());
    match result {
        Ok(r) => {
            println!(
                "{} inputs in {:.1} s ({:.0}/s): {} accepted, {} rendered; corpus {}; slowest input {:.1} ms",
                r.runs,
                r.elapsed.as_secs_f64(),
                r.runs as f64 / r.elapsed.as_secs_f64().max(1e-9),
                r.accepted,
                r.rendered,
                r.corpus,
                r.slowest.as_secs_f64() * 1e3
            );
            Ok(())
        }
        Err(f) => {
            let name = format!("fuzz-{}-{}.input", target.id(), opts.seed);
            std::fs::write(&name, &f.input).map_err(|e| format!("{}; and saving the input failed: {e}", f.message))?;
            Err(format!(
                "after {} inputs: {}\nThe input ({} bytes) is in {name}; check it again with: tinvo fuzz {} --check {name}",
                f.runs,
                f.message,
                f.input.len(),
                target.id()
            ))
        }
    }
}

fn cmd_bench(args: &[String]) -> Result<(), String> {
    match args {
        [] => bench_time(false),
        [flag] if flag == "--check" => bench_time(true),
        [flag, id, seconds] if flag == "--count" => {
            let scenario = bench_scenario(id)?;
            let seconds: f64 =
                seconds.parse().ok().filter(|s| (0.0..=600.0).contains(s)).ok_or("SECONDS must be 0..=600")?;
            println!("{}", bench::render(&scenario, seconds));
            Ok(())
        }
        [flag, file] if flag == "--instructions" => bench_instructions(file),
        _ => {
            Err(format!("bench takes no arguments, --check, --count SCENARIO SECONDS or --instructions FILE{SEE_HELP}"))
        }
    }
}

fn bench_scenario(id: &str) -> Result<bench::Scenario, String> {
    let ids: Vec<&str> = bench::SCENARIOS.iter().map(|s| s.id).collect();
    bench::find(id).ok_or(format!("no scenario '{}' (there are {})", shown(id), ids.join(", ")))
}

/// Times every scenario in device-sized callbacks; with `check`, fails when
/// one is over its budget.
fn bench_time(check: bool) -> Result<(), String> {
    let mut over = Vec::new();
    for s in bench::SCENARIOS {
        let t = bench::time(&s, 20.0);
        println!(
            "{}: {}, {} track(s): {:.0}x real time. Per {}-frame callback: mean {:.1} %, p99 {:.1} %, worst {:.1} % of the period (budget: mean {:.0} %)",
            s.id,
            s.label,
            s.tracks(),
            t.audio / t.elapsed,
            bench::PERIOD,
            t.mean * 100.0,
            t.p99 * 100.0,
            t.worst * 100.0,
            s.budget * 100.0
        );
        if t.mean > s.budget {
            over.push(s.id);
        }
    }
    match (check, over.is_empty()) {
        (true, false) => Err(format!("over the time budget: {}", over.join(", "))),
        _ => Ok(()),
    }
}

/// How far a scenario's instruction count may rise above its budget.
const INSTRUCTION_TOLERANCE: f64 = 0.10;
/// Seconds of audio counted per scenario (under Valgrind, which is slow).
const COUNTED_SECONDS: f64 = 2.0;

/// Counts each scenario's instructions per second of audio with Valgrind's
/// cachegrind and compares them with the budgets in `file`.
fn bench_instructions(file: &str) -> Result<(), String> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err("instruction budgets are counted on x86-64 Linux, where Valgrind runs".into());
    }
    let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
    let budgets = bench::parse_budgets(&text).map_err(|e| format!("{file}: {e}"))?;
    let missing: Vec<&str> =
        bench::SCENARIOS.iter().map(|s| s.id).filter(|id| !budgets.iter().any(|(b, _)| b == id)).collect();
    if !missing.is_empty() {
        return Err(format!("{file}: no budget for {}", missing.join(", ")));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let count = |id: &str, seconds: f64| -> Result<u64, String> {
        let out = std::env::temp_dir().join(format!("tinvo-cachegrind-{}-{id}-{seconds}", std::process::id()));
        let status = std::process::Command::new("valgrind")
            .args(["--tool=cachegrind", "--cache-sim=no"])
            .arg(format!("--cachegrind-out-file={}", out.display()))
            .arg(&exe)
            .args(["bench", "--count", id, &seconds.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("cannot run valgrind: {e}"))?;
        let text = std::fs::read_to_string(&out);
        let _ = std::fs::remove_file(&out);
        if !status.success() {
            return Err(format!("valgrind failed on scenario {id} ({status})"));
        }
        let text = text.map_err(|e| format!("cachegrind's output: {e}"))?;
        bench::cachegrind_total(&text).ok_or_else(|| "no total in cachegrind's output".to_string())
    };
    let mut over = Vec::new();
    let mut measured = Vec::new();
    for (id, budget) in &budgets {
        // The set-up alone, subtracted, leaves the cost of rendering.
        let per_second = count(id, COUNTED_SECONDS)?.saturating_sub(count(id, 0.0)?) as f64 / COUNTED_SECONDS;
        let change = per_second / *budget as f64 - 1.0;
        println!("{id}: {per_second:.0} instructions per second of audio, {:+.1} % against its budget", change * 100.0);
        if change > INSTRUCTION_TOLERANCE {
            over.push(id.as_str());
        } else if change < -INSTRUCTION_TOLERANCE {
            println!(
                "  {id} now costs less than its budget by more than the tolerance; lower the budget to keep it tight"
            );
        }
        measured.push(format!("{id} {per_second:.0}"));
    }
    println!("\nMeasured, in the budget file's format:\n{}", measured.join("\n"));
    if over.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "over the instruction budget by more than {:.0} %: {}",
            INSTRUCTION_TOLERANCE * 100.0,
            over.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn usage_errors_are_one_line() {
        let e = cmd_render(&args(&["--song", "--pattern", "B", "a.tvo", "b.wav"])).unwrap_err();
        assert_eq!(e, "--pattern: --song, --region and --pattern each pick what to render; give one");
        let e = cmd_probe(&args(&["1", "extra"])).unwrap_err();
        assert_eq!(e, format!("probe takes at most SECONDS{SEE_HELP}"));
        let e = cmd_render(&args(&["a.tvo"])).unwrap_err();
        assert!(!e.contains('\n') && e.ends_with(SEE_HELP), "{e}");
        // What was typed is shown escaped, on the one line.
        let e = cmd_render(&args(&["--odd\nflag", "a.tvo", "b.wav"])).unwrap_err();
        assert_eq!(e, "unknown option '--odd\\nflag'");
        let e = cmd_bench(&args(&["--count", "odd\nscenario", "1"])).unwrap_err();
        assert!(e.starts_with("no scenario 'odd\\nscenario' (there are "), "{e}");
    }

    #[test]
    fn render_writes_neither_the_project_nor_another_kind_of_file() {
        let dir = std::env::temp_dir().join(format!("tinvo-cli-dest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.tvo");
        Project::demo().save(&song).unwrap();
        let before = std::fs::read(&song).unwrap();
        let s = song.display().to_string();
        let e = cmd_render(&args(&[&s, &s])).unwrap_err();
        assert!(e.ends_with("is not a .wav or .mid file"), "{e}");
        assert_eq!(std::fs::read(&song).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
