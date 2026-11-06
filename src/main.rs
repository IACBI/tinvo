//! TINVO command-line entry point.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tinvo::audio::{self, AudioCallback};
use tinvo::dsp::loudness::ExportMeter;
use tinvo::engine::{Edit, MAX_LANES, MAX_PATTERNS, Step};
use tinvo::instrument::InstrumentKind;
use tinvo::project::{Project, pattern_name};
use tinvo::render::{self, RenderOptions, Source};
use tinvo::strip::sp;
use tinvo::wav::{self, SampleFormat};

const USAGE: &str = "\
TINVO - terminal digital audio workstation

USAGE:
    tinvo [PROJECT.tvo]                 open the editor (demo beat if no file)
    tinvo render PROJECT OUT.wav [--format 16|24|32f] [--loops N] [--rate HZ]
                 [--song | --region | --pattern A-P] [--stems]
                                        export WAV (the song if it has entries, else
                                        pattern A); --stems also writes one file per track
    tinvo demo OUT.tvo                  write the demo project to a file
    tinvo probe [SECONDS]               open the audio device, output silence, report timing
    tinvo bench                         measure rendering speed against real time
    tinvo --help | --version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None => tinvo::app::run(None),
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
        Some("bench") => cmd_bench(),
        Some(s) if s.starts_with('-') => Err(format!("unknown option '{s}'\n\n{USAGE}")),
        Some(path) if args.len() == 1 => tinvo::app::run(Some(PathBuf::from(path))),
        Some(_) => Err(format!("too many arguments\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_render(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut format = SampleFormat::Pcm24;
    let mut opts = RenderOptions::default();
    let mut stems = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--format" => {
                let v = value("--format")?;
                format = SampleFormat::from_id(&v).ok_or(format!("unknown format '{v}' (use 16, 24 or 32f)"))?;
            }
            "--loops" => {
                opts.loops = value("--loops")?
                    .parse()
                    .ok()
                    .filter(|n| (1..=1000).contains(n))
                    .ok_or("--loops must be 1..=1000")?;
            }
            "--song" => opts.source = Source::Song,
            "--region" => opts.source = Source::Region,
            "--pattern" => {
                let v = value("--pattern")?;
                let p = match v.as_bytes() {
                    [c @ b'A'..=b'P'] => c - b'A',
                    _ => return Err(format!("--pattern takes a letter A-P, not '{v}'")),
                };
                opts.source = Source::Pattern(p);
            }
            "--stems" => stems = true,
            "--rate" => {
                opts.sample_rate = value("--rate")?
                    .parse()
                    .ok()
                    .filter(|r| (8_000..=192_000).contains(r))
                    .ok_or("--rate must be 8000..=192000")?;
            }
            s if s.starts_with('-') => return Err(format!("unknown option '{s}'")),
            _ => positional.push(a.clone()),
        }
    }
    let [project, out] = positional.as_slice() else {
        return Err(format!("render needs PROJECT and OUT.wav\n\n{USAGE}"));
    };
    let project = Project::load(project.as_ref())?;
    let (track_samples, errors) = project.load_samples();
    if !errors.is_empty() {
        return Err(format!("cannot load samples:\n  {}", errors.join("\n  ")));
    }
    match (opts.source, render::resolve(&project, opts.source)) {
        (Source::Song, Source::Pattern(_)) => return Err("the song is empty".into()),
        (Source::Region, Source::Pattern(_)) => return Err("the project has no loop region inside the song".into()),
        _ => {}
    }
    let what = match render::resolve(&project, opts.source) {
        Source::Song => "the song".to_string(),
        Source::Region => "the loop region".to_string(),
        Source::Pattern(p) => format!("pattern {}", pattern_name(p)),
        Source::Auto => unreachable!("resolved"),
    };
    let start = Instant::now();
    let mut file = wav::FileWriter::create(out.as_ref(), 2, opts.sample_rate, format)?;
    let mut meter = ExportMeter::new(opts.sample_rate);
    let frames = render::render_into(&project, &track_samples, &opts, &mut |b| {
        meter.push(b);
        file.write(b)
    })?;
    let stats = file.finish()?;
    let elapsed = start.elapsed();
    let secs = frames as f64 / f64::from(opts.sample_rate);
    println!(
        "Rendered {what}, {secs:.2} s ({}, {} Hz), to {out} in {:.3} s; {}; sample peak {:.2} dBFS; {} clipped samples",
        format.label(),
        opts.sample_rate,
        elapsed.as_secs_f64(),
        meter.finish().summary(),
        tinvo::dsp::gain_to_db(stats.peak),
        stats.clipped
    );
    if stems {
        let out_path = std::path::Path::new(out);
        let stem = out_path.file_stem().map_or("out".into(), |s| s.to_string_lossy().into_owned());
        let mut files = Vec::new();
        for i in render::stem_tracks(&project) {
            let name: String =
                project.tracks[i].name.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
            let path = out_path.with_file_name(format!("{stem}-{:02}-{name}.wav", i + 1));
            let mut file = wav::FileWriter::create(&path, 2, opts.sample_rate, format)?;
            render::render_into(&render::stem_project(&project, i), &track_samples, &opts, &mut |b| file.write(b))?;
            files.push((path, file));
        }
        // Stems end at different times; pad them to one length so they line up.
        let len = files.iter().map(|(_, f)| f.frames()).max().unwrap_or(0);
        let silence = [0.0f32; 2 * 1024];
        for (path, mut file) in files {
            while file.frames() < len {
                let n = (len - file.frames()).min(1024) as usize;
                file.write(&silence[..2 * n])?;
            }
            let st = file.finish()?;
            println!("  stem {}: peak {:.2} dBFS", path.display(), tinvo::dsp::gain_to_db(st.peak));
        }
    }
    Ok(())
}

fn cmd_demo(args: &[String]) -> Result<(), String> {
    let [out] = args else { return Err("demo needs exactly one output path".into()) };
    Project::demo().save(out.as_ref()).map_err(|e| format!("{out}: {e}"))?;
    println!("Wrote demo project to {out}");
    Ok(())
}

/// Outputs silence through the real device and reports callback timing.
fn cmd_probe(args: &[String]) -> Result<(), String> {
    let seconds: f64 = match args.first() {
        Some(s) => s.parse().ok().filter(|v| *v > 0.0 && *v <= 60.0).ok_or("SECONDS must be in (0, 60]")?,
        None => 2.0,
    };
    struct Silence {
        frames: Arc<AtomicU64>,
        calls: Arc<AtomicU64>,
    }
    impl AudioCallback for Silence {
        fn process(&mut self, out: &mut [f32]) {
            out.fill(0.0);
            self.frames.fetch_add(out.len() as u64 / 2, Ordering::Relaxed);
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }
    let frames = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(AtomicU64::new(0));
    let (f, c) = (frames.clone(), calls.clone());
    let stream = audio::open(move |_| Silence { frames: f, calls: c })?;
    let info = stream.info().clone();
    println!(
        "Opened {}: {} Hz, {} channels, {} format, {}-frame buffer ({:.1} ms)",
        info.backend,
        info.sample_rate,
        info.device_channels,
        info.format,
        info.buffer_frames,
        f64::from(info.buffer_frames) * 1000.0 / f64::from(info.sample_rate)
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
    Ok(())
}

fn cmd_bench() -> Result<(), String> {
    bench("Demo project", &Project::demo())?;
    bench("Demo, every strip stage on, mid band swept", &strip_stress_project()?)?;
    bench("Worst-case synth (8 voices, unison 4, noise, LFO)", &synth_stress_project()?)
}

fn bench(label: &str, project: &Project) -> Result<(), String> {
    // The demo song is four patterns long, so both benches render 16 bars.
    let loops = if project.song.is_empty() { 16 } else { 4 };
    let opts = RenderOptions { sample_rate: 48_000, loops, max_tail_seconds: 0.0, source: Source::Auto };
    // Warm up caches and the allocator before timing.
    render::render(project, &[], &RenderOptions { loops: 1, ..opts });
    let start = Instant::now();
    let samples = render::render(project, &[], &opts);
    let elapsed = start.elapsed().as_secs_f64();
    let audio = samples.len() as f64 / 2.0 / 48_000.0;
    println!(
        "{label}, {} track(s): {audio:.1} s of audio in {elapsed:.3} s = {:.0}x real time ({:.2} % of one core)",
        project.tracks.len(),
        audio / elapsed,
        elapsed / audio * 100.0
    );
    Ok(())
}

/// The demo with every stage of every track's channel strip on, and the mid
/// band's frequency automated on a ramp so its coefficients never settle.
fn strip_stress_project() -> Result<Project, String> {
    let mut p = Project::demo();
    let ok = |r: Result<Edit, tinvo::project::EditError>| r.map(drop).map_err(|e| e.to_string());
    for track in 0..p.tracks.len() as u8 {
        let kind = p.tracks[usize::from(track)].kind;
        let base = kind.params().len() as u8;
        let settings = [
            (sp::HP_FREQ, 40.0),
            (sp::LOW_GAIN, 2.0),
            (sp::MID_GAIN, -3.0),
            (sp::HIGH_GAIN, 2.0),
            (sp::COMP_THRESHOLD, -30.0),
            (sp::COMP_RATIO, 4.0),
            (sp::DRIVE, 6.0),
        ];
        for (i, value) in settings {
            ok(p.apply(Edit::SetParam { track, index: base + i as u8, value }))?;
        }
        let lane = (0..MAX_LANES as u8)
            .find(|&l| p.tracks[usize::from(track)].lanes[usize::from(l)].target.is_none())
            .ok_or("no free lane")?;
        let target = Some(2 + base + sp::MID_FREQ as u8);
        ok(p.apply(Edit::SetLane { track, lane, target, ramp: true }))?;
        for pattern in 0..MAX_PATTERNS as u8 {
            for (step, value) in [(0, 300.0), (8, 4000.0)] {
                ok(p.apply(Edit::SetPoint { track, lane, pattern, step, value: Some(value) }))?;
            }
        }
    }
    Ok(p)
}

/// One synth track with every voice busy and every per-sample feature on.
fn synth_stress_project() -> Result<Project, String> {
    let mut p = Project::new();
    p.apply(Edit::AddTrack(InstrumentKind::Synth)).map_err(|e| e.to_string())?;
    let specs = InstrumentKind::Synth.params();
    let mut set = |id: &str, value: f32| -> Result<(), String> {
        let index = specs.iter().position(|s| s.id == id).ok_or(format!("no synth parameter {id}"))? as u8;
        p.apply(Edit::SetParam { track: 0, index, value }).map(drop).map_err(|e| e.to_string())
    };
    for (id, v) in [("osc1_wave", 1.0), ("unison", 3.0), ("noise", 0.2), ("lfo_pitch", 0.3), ("lfo_pw", 0.3)] {
        set(id, v)?;
    }
    // Two overlapping four-note chords keep all eight voices sounding.
    let chord = Step { chord: [3, 7, 10], ..Step::new(48, 100, 2) };
    for step in 0..16 {
        p.apply(Edit::SetStep { track: 0, pattern: 0, step, value: Some(chord) }).map_err(|e| e.to_string())?;
    }
    Ok(p)
}
