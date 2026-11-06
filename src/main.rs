//! TINVO command-line entry point.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tinvo::audio::{self, AudioCallback};
use tinvo::engine::{Edit, Step};
use tinvo::instrument::InstrumentKind;
use tinvo::project::Project;
use tinvo::render::{self, RenderOptions};
use tinvo::wav::{self, SampleFormat};

const USAGE: &str = "\
TINVO - terminal digital audio workstation

USAGE:
    tinvo [PROJECT.tvo]                 open the editor (demo beat if no file)
    tinvo render PROJECT OUT.wav [--format 16|24|32f] [--loops N] [--rate HZ]
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
    let start = Instant::now();
    let samples = render::render(&project, &opts);
    let elapsed = start.elapsed();
    let stats = wav::write_file(out.as_ref(), &samples, 2, opts.sample_rate, format)?;
    let secs = samples.len() as f64 / 2.0 / f64::from(opts.sample_rate);
    println!(
        "Rendered {secs:.2} s ({}, {} Hz) to {out} in {:.3} s; peak {:.2} dBFS; {} clipped samples",
        format.label(),
        opts.sample_rate,
        elapsed.as_secs_f64(),
        tinvo::dsp::gain_to_db(stats.peak),
        stats.clipped
    );
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
    bench("Worst-case synth (8 voices, unison 4, noise, LFO)", &synth_stress_project()?)
}

fn bench(label: &str, project: &Project) -> Result<(), String> {
    let opts = RenderOptions { sample_rate: 48_000, loops: 16, max_tail_seconds: 0.0 };
    // Warm up caches and the allocator before timing.
    render::render(project, &RenderOptions { loops: 1, ..opts });
    let start = Instant::now();
    let samples = render::render(project, &opts);
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
        p.apply(Edit::SetStep { track: 0, step, value: Some(chord) }).map_err(|e| e.to_string())?;
    }
    Ok(p)
}
