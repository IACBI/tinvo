//! What rendering costs: the scenarios `tinvo bench` measures, and their
//! budgets.
//!
//! Two measures, because neither is enough alone:
//!
//! - **Time.** [`time`] plays a scenario through the engine in callbacks of
//!   [`PERIOD`] frames, as an audio device asks for them, and compares each
//!   callback's duration with the period it fills. That is what decides
//!   whether playback keeps up, but it depends on the machine and on what
//!   else runs there, so the budgets on it are loose: a scenario must keep
//!   real time with room to spare on a shared CI runner.
//! - **Instructions.** [`render`] plays a scenario with nothing else, so that
//!   a counter outside the process (Valgrind's cachegrind) can count the
//!   instructions it takes. The count depends only on the code and the
//!   compiler, not on the machine's load, so the budgets on it, in
//!   `bench/instructions.txt`, catch a regression of a few percent.

use crate::engine::{ChainId, Command, Edit, Engine, LaneTarget, MAX_LANES, MAX_PATTERNS, MAX_TRACKS, Shared, Step};
use crate::group::MAX_GROUPS;
use crate::insert::{EffectKind, MAX_INSERTS, MAX_SLOTS};
use crate::instrument::{InstrumentKind, SampleMap};
use crate::mix::send;
use crate::project::Project;
use crate::sample::Sample;
use crate::strip::sp;
use std::sync::Arc;
use std::time::Instant;

/// Frames per callback in [`time`]: 2.7 ms at 48 kHz, a common low-latency
/// setting.
pub const PERIOD: usize = 128;
const SAMPLE_RATE: u32 = 48_000;

/// Something to render, with its time budget.
#[derive(Clone, Copy)]
pub struct Scenario {
    /// Short name for the command line and the instruction budgets.
    pub id: &'static str,
    pub label: &'static str,
    /// The highest mean cost of a callback allowed, as a fraction of the
    /// period it fills.
    pub budget: f64,
    build: fn() -> Setup,
}

/// A scenario's project, the samples of its tracks, and how it plays.
struct Setup {
    project: Project,
    samples: Vec<Option<Arc<SampleMap>>>,
    play: Command,
}

/// Every scenario, cheapest first.
pub const SCENARIOS: [Scenario; 7] = [
    Scenario { id: "demo", label: "Demo song", budget: 0.15, build: demo },
    Scenario { id: "strip", label: "Demo song, every strip stage on, mid band swept", budget: 0.25, build: strip },
    Scenario {
        id: "groups",
        label: "Demo song's tracks repeated to 8, each through a group, every stage on",
        budget: 0.4,
        build: groups,
    },
    Scenario {
        id: "inserts",
        label: "The groups scenario with as many insert effects as a project holds, every kind busy",
        budget: 0.75,
        build: inserts,
    },
    Scenario { id: "tracks32", label: "Demo song's tracks repeated to 32", budget: 0.5, build: tracks32 },
    Scenario { id: "synth", label: "Worst-case synth (8 voices, unison 4, noise, LFO)", budget: 0.25, build: synth },
    Scenario {
        id: "sampler",
        label: "Sampler, 16 voices resampled through sinc and filter",
        budget: 0.1,
        build: sampler,
    },
];

pub fn find(id: &str) -> Option<Scenario> {
    SCENARIOS.into_iter().find(|s| s.id == id)
}

impl Scenario {
    /// Tracks in the scenario's project.
    pub fn tracks(&self) -> usize {
        (self.build)().project.tracks.len()
    }

    fn engine(&self) -> Box<Engine> {
        let setup = (self.build)();
        let edits = setup.project.to_edits();
        let mut engine = Engine::with_edits(SAMPLE_RATE as f32, &edits, Arc::new(Shared::new()), None);
        for (track, sample) in setup.samples.into_iter().enumerate() {
            engine.set_samples(track, sample);
        }
        engine.handle(setup.play);
        engine
    }
}

/// How long callbacks took, as fractions of the period each one filled.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Seconds of audio rendered, and how long that took.
    pub audio: f64,
    pub elapsed: f64,
    pub mean: f64,
    /// Exceeded by one callback in a hundred.
    pub p99: f64,
    pub worst: f64,
}

/// Plays `seconds` of `scenario` in callbacks of [`PERIOD`] frames and
/// times each one, after a warm-up of half a second.
pub fn time(scenario: &Scenario, seconds: f64) -> Timing {
    let mut engine = scenario.engine();
    let mut buf = vec![0.0f32; 2 * PERIOD];
    for _ in 0..SAMPLE_RATE as usize / 2 / PERIOD {
        engine.process(&mut buf);
    }
    let callbacks = (seconds * f64::from(SAMPLE_RATE) / PERIOD as f64).ceil().max(1.0) as usize;
    let mut costs = Vec::with_capacity(callbacks);
    let start = Instant::now();
    for _ in 0..callbacks {
        let t = Instant::now();
        engine.process(&mut buf);
        costs.push(t.elapsed().as_secs_f64());
    }
    let elapsed = start.elapsed().as_secs_f64();
    std::hint::black_box(&buf);
    let period = PERIOD as f64 / f64::from(SAMPLE_RATE);
    costs.sort_by(f64::total_cmp);
    let at = |q: f64| costs[((costs.len() - 1) as f64 * q).round() as usize] / period;
    Timing {
        audio: (callbacks * PERIOD) as f64 / f64::from(SAMPLE_RATE),
        elapsed,
        mean: costs.iter().sum::<f64>() / costs.len() as f64 / period,
        p99: at(0.99),
        worst: at(1.0),
    }
}

/// Renders `seconds` of `scenario` and nothing else, for an instruction
/// counter; with 0 seconds only the set-up runs, so the difference between
/// the two is the cost of rendering. Returns a checksum of the output, so
/// the work cannot be optimised away.
pub fn render(scenario: &Scenario, seconds: f64) -> f64 {
    let mut engine = scenario.engine();
    let mut buf = vec![0.0f32; 2 * 1024];
    let mut left = (seconds * f64::from(SAMPLE_RATE)).round() as usize;
    let mut sum = 0.0f64;
    while left > 0 {
        let n = left.min(1024);
        engine.process(&mut buf[..2 * n]);
        sum += buf[..2 * n].iter().map(|&x| f64::from(x.abs())).sum::<f64>();
        left -= n;
    }
    sum
}

fn ok(r: Result<Edit, crate::project::EditError>) {
    if let Err(e) = r {
        panic!("a bench scenario's edit was refused: {e}");
    }
}

fn demo() -> Setup {
    Setup { project: Project::demo(), samples: Vec::new(), play: Command::PlaySong { entry: 0 } }
}

/// The demo with every stage of every track's channel strip on, and the mid
/// band's frequency automated on a ramp so its coefficients never settle.
fn strip() -> Setup {
    let mut p = Project::demo();
    for track in 0..p.tracks.len() as u8 {
        let kind = p.tracks[usize::from(track)].kind;
        let base = kind.params().len() as u8;
        for (i, value) in [
            (sp::HP_FREQ, 40.0),
            (sp::LOW_GAIN, 2.0),
            (sp::MID_GAIN, -3.0),
            (sp::HIGH_GAIN, 2.0),
            (sp::COMP_THRESHOLD, -30.0),
            (sp::COMP_RATIO, 4.0),
            (sp::DRIVE, 6.0),
        ] {
            ok(p.apply(Edit::SetParam { track, index: base + i as u8, value }));
        }
        let lanes = &p.tracks[usize::from(track)].lanes;
        let lane = (0..MAX_LANES as u8).find(|&l| lanes[usize::from(l)].target.is_none()).expect("a free lane");
        let target = Some(LaneTarget::Track(2 + base + sp::MID_FREQ as u8));
        ok(p.apply(Edit::SetLane { track, lane, target, ramp: true }));
        for pattern in 0..MAX_PATTERNS as u8 {
            for (step, value) in [(0, 300.0), (8, 4000.0)] {
                ok(p.apply(Edit::SetPoint { track, lane, pattern, step, value: Some(value) }));
            }
        }
    }
    Setup { project: p, samples: Vec::new(), play: Command::PlaySong { entry: 0 } }
}

/// The demo's tracks repeated to as many as a project has groups, each
/// through a group of its own with every strip stage and send on.
fn groups() -> Setup {
    let mut p = Project::demo();
    let demo = p.tracks.clone();
    p.tracks.extend(demo.iter().cycle().take(MAX_GROUPS - demo.len()).cloned());
    for group in 0..MAX_GROUPS as u8 {
        ok(p.apply(Edit::InsertGroup(group)));
        for (i, value) in [
            (sp::HP_FREQ, 40.0),
            (sp::LOW_GAIN, 2.0),
            (sp::MID_GAIN, -3.0),
            (sp::HIGH_GAIN, 2.0),
            (sp::COMP_THRESHOLD, -30.0),
            (sp::COMP_RATIO, 4.0),
            (sp::DRIVE, 6.0),
            (sp::COUNT + send::DELAY, -12.0),
            (sp::COUNT + send::REVERB, -12.0),
            (sp::COUNT + send::CHORUS, -12.0),
        ] {
            ok(p.apply(Edit::SetGroupParam { group, index: i as u8, value }));
        }
    }
    for track in 0..p.tracks.len() {
        ok(p.apply(Edit::SetTrackGroup { track: track as u8, group: Some(track as u8) }));
    }
    Setup { project: p, samples: Vec::new(), play: Command::PlaySong { entry: 0 } }
}

/// The groups scenario with [`MAX_INSERTS`] effects, the kinds in turn:
/// four on each track, three on each group and a full master chain, every
/// one set to work, and the first track's EQ swept on a ramp.
fn inserts() -> Setup {
    let mut setup = groups();
    let p = &mut setup.project;
    let mut chains: Vec<(ChainId, u8)> = Vec::new();
    for t in 0..p.tracks.len() as u8 {
        chains.extend((0..4).map(|s| (ChainId::Track(t), s)));
    }
    for g in 0..MAX_GROUPS as u8 {
        chains.extend((0..3).map(|s| (ChainId::Group(g), s)));
    }
    chains.extend((0..MAX_SLOTS as u8).map(|s| (ChainId::Master, s)));
    assert_eq!(chains.len(), MAX_INSERTS);
    for (k, &(chain, slot)) in chains.iter().enumerate() {
        let kind = EffectKind::ALL[k % EffectKind::ALL.len()];
        ok(p.apply(Edit::SetSlot { chain, slot, kind: Some(kind) }));
        let busy: &[(&str, f32)] = match kind {
            EffectKind::Eq => &[("hp_freq", 40.0), ("eq_low_gain", 2.0), ("eq_mid_gain", -3.0), ("eq_high_gain", 2.0)],
            EffectKind::Compressor => &[("comp_threshold", -30.0), ("comp_ratio", 4.0)],
            EffectKind::Drive => &[("drive", 6.0)],
            EffectKind::Delay | EffectKind::Reverb | EffectKind::Chorus | EffectKind::Phaser => &[("mix", 0.4)],
            EffectKind::Filter => &[("filter_mode", 1.0), ("filter_cutoff", 2000.0), ("filter_resonance", 0.4)],
            EffectKind::Crusher => &[("crusher_bits", 6.0), ("crusher_hold", 2.5), ("mix", 0.5)],
            EffectKind::Gate => &[("gate_threshold", -50.0), ("gate_range", -30.0)],
            EffectKind::Plugin(_) => &[],
        };
        for &(id, value) in busy {
            let index = kind.params().iter().position(|s| s.id == id).unwrap_or_else(|| panic!("no parameter {id}"));
            ok(p.apply(Edit::SetSlotParam { chain, slot, index: index as u8, value }));
        }
    }
    let lanes = &p.tracks[0].lanes;
    let lane = (0..MAX_LANES as u8).find(|&l| lanes[usize::from(l)].target.is_none()).expect("a free lane");
    let index = EffectKind::Eq.params().iter().position(|s| s.id == "eq_mid_freq").expect("the mid band") as u8;
    let target = Some(LaneTarget::Slot { slot: 0, kind: EffectKind::Eq, index });
    ok(p.apply(Edit::SetLane { track: 0, lane, target, ramp: true }));
    for pattern in 0..MAX_PATTERNS as u8 {
        for (step, value) in [(0, 300.0), (8, 4000.0)] {
            ok(p.apply(Edit::SetPoint { track: 0, lane, pattern, step, value: Some(value) }));
        }
    }
    setup
}

/// A full project: the demo's tracks again and again, every one playing.
fn tracks32() -> Setup {
    let mut p = Project::demo();
    let demo = p.tracks.clone();
    p.tracks.extend(demo.iter().cycle().take(MAX_TRACKS - demo.len()).cloned());
    Setup { project: p, samples: Vec::new(), play: Command::PlaySong { entry: 0 } }
}

/// Sets instrument parameters of track 0 by id.
fn set(p: &mut Project, kind: InstrumentKind, values: &[(&str, f32)]) {
    for &(id, value) in values {
        let index = kind.params().iter().position(|s| s.id == id).unwrap_or_else(|| panic!("no parameter {id}"));
        ok(p.apply(Edit::SetParam { track: 0, index: index as u8, value }));
    }
}

/// A four-note chord on every step, each `len` steps long: `len` of them
/// overlap, keeping `4 * len` voices of track 0 sounding.
fn chords(p: &mut Project, root: u8, len: u8) {
    let chord = Step { chord: [3, 7, 10], ..Step::new(root, 100, len) };
    for step in 0..16 {
        ok(p.apply(Edit::SetStep { track: 0, pattern: 0, step, value: Some(chord) }));
    }
}

/// One synth track with every voice busy and every per-sample feature on.
fn synth() -> Setup {
    let mut p = Project::new();
    ok(p.apply(Edit::AddTrack(InstrumentKind::Synth)));
    set(
        &mut p,
        InstrumentKind::Synth,
        &[("osc1_wave", 1.0), ("unison", 3.0), ("noise", 0.2), ("lfo_pitch", 0.3), ("lfo_pw", 0.3)],
    );
    chords(&mut p, 48, 2);
    Setup { project: p, samples: Vec::new(), play: Command::Play }
}

/// One sampler track with every voice busy, playing a 44.1 kHz sample at
/// other pitches (so every read interpolates), looping, through the filter.
fn sampler() -> Setup {
    let mut p = Project::new();
    ok(p.apply(Edit::AddTrack(InstrumentKind::Sampler)));
    set(&mut p, InstrumentKind::Sampler, &[("interp", 0.0), ("loop", 1.0), ("cutoff", 3000.0), ("resonance", 0.5)]);
    chords(&mut p, 55, 4);
    let rate = 44_100;
    let audio: Vec<f32> = (0..rate * 2)
        .map(|i| {
            let t = i as f32 / rate as f32;
            // A saw with some harmonics to filter.
            ((t * 110.0).fract() * 2.0 - 1.0) * 0.5
        })
        .collect();
    let sample = Sample::from_mono(&audio, rate as u32).expect("a valid sample");
    Setup { project: p, samples: vec![Some(Arc::new(SampleMap::single(Arc::new(sample))))], play: Command::Play }
}

/// Instruction budgets: `id instructions-per-second` lines, `#` comments.
pub fn parse_budgets(text: &str) -> Result<Vec<(String, u64)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("line {}: expected '<scenario> <instructions per second>'", n + 1);
        let (id, count) = line.split_once(char::is_whitespace).ok_or_else(bad)?;
        let count: u64 = count.trim().parse().ok().filter(|&c| c > 0).ok_or_else(bad)?;
        if find(id).is_none() {
            return Err(format!("line {}: no scenario '{id}'", n + 1));
        }
        out.push((id.to_string(), count));
    }
    Ok(out)
}

/// The total instruction count in a cachegrind output file.
pub fn cachegrind_total(text: &str) -> Option<u64> {
    let line = text.lines().find_map(|l| l.strip_prefix("summary:"))?;
    line.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_builds_and_sounds() {
        for s in SCENARIOS {
            assert!(s.tracks() > 0 && s.budget > 0.0 && s.budget < 1.0, "{}", s.id);
            assert!(render(&s, 0.25) > 1.0, "{} renders silence", s.id);
            assert_eq!(render(&s, 0.0), 0.0);
            assert_eq!(find(s.id).map(|f| f.id), Some(s.id));
        }
        assert!(find("nothing").is_none());
        assert_eq!(find("tracks32").map(|s| s.tracks()), Some(MAX_TRACKS));
        let groups = (find("groups").expect("a scenario").build)().project;
        assert_eq!(groups.groups.len(), MAX_GROUPS);
        assert!(groups.tracks.iter().all(|t| t.group.is_some()));
        let inserts = (find("inserts").expect("a scenario").build)().project;
        assert_eq!(inserts.insert_count(), MAX_INSERTS);
    }

    #[test]
    fn timing_counts_every_callback() {
        let t = time(&SCENARIOS[0], 0.1);
        assert!((t.audio - 0.1).abs() < PERIOD as f64 / 48_000.0, "{t:?}");
        assert!(t.mean > 0.0 && t.mean <= t.worst && t.p99 <= t.worst, "{t:?}");
    }

    #[test]
    fn budgets_and_cachegrind_output_are_read() {
        let b = parse_budgets("# comment\n\ndemo 1200\nsynth   340\n").unwrap();
        assert_eq!(b, [("demo".to_string(), 1200), ("synth".to_string(), 340)]);
        assert!(parse_budgets("demo x").is_err());
        assert!(parse_budgets("demo 0").is_err());
        assert!(parse_budgets("nothing 5").is_err());
        let out = "desc: I1 cache:\ncmd: tinvo bench\nevents: Ir\nfl=a.rs\nfn=main\n1 5\nsummary: 123456789\n";
        assert_eq!(cachegrind_total(out), Some(123_456_789));
        assert_eq!(cachegrind_total("events: Ir\n"), None);
    }
}
