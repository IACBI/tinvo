//! The audio thread's work never allocates or frees memory.
//!
//! This binary replaces the global allocator with one that counts calls
//! while armed, and arms it only around work that runs on a real-time
//! thread: `Engine::process` (with whatever commands, samples and live
//! notes are waiting for it), the capture side of recording, and the MIDI
//! byte parser and mailboxes the backend threads use. It runs without the
//! test harness, on one thread, so nothing else allocates while armed.
//!
//! On the first allocation while armed it prints a backtrace, so a failure
//! names the code that allocated.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tinvo::dsp::noise::Rng;
use tinvo::engine::{Command, Edit, Engine, LiveNote, MAX_TRACKS, MIX_TARGET, Shared, Step};
use tinvo::instrument::InstrumentKind;
use tinvo::midi::Parser;
use tinvo::mix::{MIX_PARAMS, mp, send};
use tinvo::project::Project;
use tinvo::record::{Meter, capture_into};
use tinvo::rt::mailbox::Mailbox;
use tinvo::rt::spsc::{self, Consumer, Producer};
use tinvo::sample::Sample;
use tinvo::strip::{sp, track_param, track_param_count};

struct Counting;

static ARMED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static REPORTED: AtomicBool = AtomicBool::new(false);

impl Counting {
    fn note(&self) {
        if ARMED.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            // Disarmed first: capturing the backtrace allocates, and comes
            // back through here.
            ARMED.store(false, Ordering::Relaxed);
            if !REPORTED.swap(true, Ordering::Relaxed) {
                let bt = std::backtrace::Backtrace::force_capture();
                let _ = writeln!(std::io::stderr(), "allocator called on the real-time path:\n{bt}");
            }
        }
    }
}

// SAFETY: every call is passed on to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.note();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.note();
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `f` armed and returns how often it called the allocator.
fn armed(f: impl FnOnce()) -> usize {
    let before = CALLS.load(Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    f();
    ARMED.store(false, Ordering::Relaxed);
    CALLS.load(Ordering::Relaxed) - before
}

const SR: f32 = 48_000.0;
/// Buffer sizes a backend may ask for, in frames, odd ones included.
const SIZES: [usize; 8] = [1, 31, 64, 128, 256, 480, 1024, 4096];

/// An engine fed through a command queue, with samples it releases coming
/// back through the retirement queue, as in the editor.
struct Rig {
    engine: Box<Engine>,
    commands: Producer<Command>,
    retired: Consumer<Arc<Sample>>,
    live: Arc<Mailbox<LiveNote>>,
    buf: Vec<f32>,
    rng: Rng,
}

impl Rig {
    fn new(project: &Project) -> Self {
        let (commands, rx) = spsc::channel(4096);
        let (tx, retired) = spsc::channel(256);
        let mut engine = Engine::with_edits(SR, &project.to_edits(), Arc::new(Shared::new()), Some(rx));
        engine.attach_retirement(tx);
        let live = Arc::new(Mailbox::new(256));
        engine.attach_live(live.clone());
        Self { engine, commands, retired, live, buf: vec![0.0; 2 * 4096], rng: Rng::new(1) }
    }

    fn send(&mut self, cmd: Command) {
        assert!(self.commands.push(cmd).is_ok(), "command queue full");
    }

    /// One callback of a random buffer size; returns allocator calls.
    fn callback(&mut self) -> usize {
        let frames = SIZES[(self.rng.next_u64() % SIZES.len() as u64) as usize];
        let (engine, buf) = (&mut self.engine, &mut self.buf[..2 * frames]);
        let calls = armed(|| engine.process(buf));
        // The UI thread's side: released samples are dropped here.
        while self.retired.pop().is_some() {}
        calls
    }
}

fn sample(frames: usize, seed: u64) -> Arc<Sample> {
    let mut rng = Rng::new(seed);
    let audio: Vec<f32> = (0..frames).map(|_| rng.bipolar() * 0.5).collect();
    Arc::new(Sample::from_mono(&audio, [22_050, 44_100, 48_000, 96_000][(seed % 4) as usize]).unwrap())
}

/// The demo with every strip stage, every send, every bus and the limiter
/// on, ramps on a strip parameter and a bus setting, and a sampler track.
fn heavy_project() -> Project {
    let mut p = Project::demo();
    let ok = |r: Result<Edit, _>| r.map(drop).unwrap();
    ok(p.apply(Edit::AddTrack(InstrumentKind::Sampler)));
    for track in 0..p.tracks.len() {
        let kind = p.tracks[track].kind;
        let n = kind.params().len();
        let t = track as u8;
        for (i, at) in [
            (sp::HP_FREQ, 0.3),
            (sp::LOW_GAIN, 0.8),
            (sp::MID_GAIN, 0.2),
            (sp::HIGH_GAIN, 0.7),
            (sp::COMP_THRESHOLD, 0.1),
            (sp::COMP_RATIO, 0.9),
            (sp::COMP_MAKEUP, 0.3),
            (sp::COMP_KEY, 1.0),
            (sp::DRIVE, 0.5),
        ] {
            let spec = track_param(kind, n + i).unwrap();
            ok(p.apply(Edit::SetParam {
                track: t,
                index: (n + i) as u8,
                value: spec.min + (spec.max - spec.min) * at,
            }));
        }
        for s in [send::DELAY, send::REVERB, send::CHORUS, send::KEY] {
            ok(p.apply(Edit::SetParam { track: t, index: (n + sp::COUNT + s) as u8, value: -6.0 }));
        }
        // Volume, then the mid band's frequency, on ramps.
        let mid = (2 + n + sp::MID_FREQ) as u8;
        ok(p.apply(Edit::SetLane { track: t, lane: 0, target: Some(mid), ramp: true }));
        let spec = track_param(kind, n + sp::MID_FREQ).unwrap();
        for (step, value) in [(0, spec.min), (8, spec.max)] {
            ok(p.apply(Edit::SetPoint { track: t, lane: 0, pattern: 0, step, value: Some(value) }));
        }
    }
    let decay = MIX_TARGET + mp::REVERB_DECAY as u8;
    ok(p.apply(Edit::SetLane { track: 0, lane: 1, target: Some(decay), ramp: true }));
    for (step, value) in [(0, 0.5), (12, 8.0)] {
        ok(p.apply(Edit::SetPoint { track: 0, lane: 1, pattern: 0, step, value: Some(value) }));
    }
    for (i, value) in [
        (mp::LIMITER, MIX_PARAMS[mp::LIMITER].max),
        (mp::DELAY_FEEDBACK, 0.9),
        (mp::CHORUS_DEPTH, MIX_PARAMS[mp::CHORUS_DEPTH].max),
    ] {
        ok(p.apply(Edit::SetMixParam { index: i as u8, value }));
    }
    let sampler = (p.tracks.len() - 1) as u8;
    for s in 0..16 {
        let step = Step { ratchet: 1 + s % 4, chord: [4, 7, 0], ..Step::new(40 + s * 3, 100, 2) };
        ok(p.apply(Edit::SetStep { track: sampler, pattern: 0, step: s, value: Some(step) }));
    }
    p
}

fn play_song(rig: &mut Rig, callbacks: usize) -> usize {
    rig.send(Command::PlaySong { entry: 0 });
    (0..callbacks).map(|_| rig.callback()).sum()
}

fn demo_song() -> usize {
    play_song(&mut Rig::new(&Project::demo()), 2000)
}

fn heavy_song() -> usize {
    let p = heavy_project();
    let mut rig = Rig::new(&p);
    let sampler = (p.tracks.len() - 1) as u8;
    rig.send(Command::SetSample { track: sampler, sample: Some(sample(30_000, 1)) });
    play_song(&mut rig, 2000)
}

/// A random edit or command, about what a busy user sends.
fn random_command(rng: &mut Rng, samples: &[Arc<Sample>]) -> Command {
    let mut r = |n: u64| rng.next_u64() % n;
    let track = r(MAX_TRACKS as u64 + 1) as u8;
    let kind = InstrumentKind::ALL[r(InstrumentKind::ALL.len() as u64) as usize];
    let edit = match r(22) {
        0 => Edit::SetTempo(20.0 + r(281) as f32),
        1 => Edit::SetSwing(50.0 + r(26) as f32),
        2 => Edit::SetPatternLength { pattern: r(16) as u8, len: 1 + r(64) as u8 },
        3 => Edit::AddTrack(kind),
        4 => Edit::InsertTrack { index: r(MAX_TRACKS as u64) as u8, kind },
        5 => Edit::RemoveTrack(track),
        6..=8 => {
            let step = Step {
                ratchet: 1 + r(4) as u8,
                offset: r(25) as i8 - 12,
                ..Step::new(r(128) as u8, 1 + r(127) as u8, 1 + r(16) as u8)
            };
            Edit::SetStep { track, pattern: r(16) as u8, step: r(64) as u8, value: (r(4) > 0).then_some(step) }
        }
        9 => Edit::InsertSongEntry { index: r(8) as u16, pattern: r(16) as u8 },
        10 => Edit::RemoveSongEntry(r(8) as u16),
        11 => Edit::SetLoop((r(3) > 0).then(|| (r(4) as u16, 4 + r(4) as u16))),
        12 => {
            let target = match r(3) {
                0 => None,
                1 => Some(MIX_TARGET + r(MIX_PARAMS.len() as u64) as u8),
                _ => Some(r(30) as u8),
            };
            Edit::SetLane { track, lane: r(4) as u8, target, ramp: r(2) == 0 }
        }
        13 => Edit::SetPoint { track, lane: r(4) as u8, pattern: r(16) as u8, step: r(64) as u8, value: Some(0.0) },
        14 | 15 => {
            let index = r(track_param_count(kind) as u64) as u8;
            let spec = track_param(kind, usize::from(index)).unwrap();
            Edit::SetParam { track, index, value: spec.min + (spec.max - spec.min) * (r(101) as f32 / 100.0) }
        }
        16 => Edit::SetGain { track, db: r(73) as f32 - 60.0 },
        17 => Edit::SetPan { track, pan: r(201) as f32 / 100.0 - 1.0 },
        18 => Edit::SetMute { track, mute: r(2) == 0 },
        19 => Edit::SetSolo { track, solo: r(2) == 0 },
        20 => {
            let index = r(MIX_PARAMS.len() as u64) as usize;
            let spec = &MIX_PARAMS[index];
            Edit::SetMixParam { index: index as u8, value: spec.min + (spec.max - spec.min) * (r(101) as f32 / 100.0) }
        }
        _ => {
            return match r(7) {
                0 => Command::Play,
                1 => Command::Stop,
                2 => Command::PlaySong { entry: r(4) as u16 },
                3 => Command::Cue(r(16) as u8),
                4 => Command::Audition { track, step: Step::new(60, 100, 1), frames: 4800 },
                5 => Command::SetLiveTrack(track),
                _ => {
                    let s = (r(3) > 0).then(|| samples[r(samples.len() as u64) as usize].clone());
                    Command::SetSample { track, sample: s }
                }
            };
        }
    };
    Command::Edit(edit)
}

fn edits_while_playing() -> usize {
    let mut rig = Rig::new(&heavy_project());
    let samples: Vec<Arc<Sample>> = (0..6).map(|i| sample([1, 7, 64, 4800, 48_000, 200_000][i], i as u64)).collect();
    let mut rng = Rng::new(42);
    rig.send(Command::PlaySong { entry: 0 });
    let mut calls = 0;
    for _ in 0..4000 {
        for _ in 0..rng.next_u64() % 6 {
            let cmd = random_command(&mut rng, &samples);
            rig.send(cmd);
        }
        calls += rig.callback();
    }
    calls
}

fn live_notes() -> usize {
    let mut rig = Rig::new(&heavy_project());
    let mut rng = Rng::new(7);
    rig.send(Command::Play);
    let mut calls = 0;
    for i in 0..3000u64 {
        if i % 50 == 0 {
            rig.send(Command::SetLiveTrack((i / 50 % 8) as u8));
        }
        for _ in 0..rng.next_u64() % 12 {
            let note = LiveNote { note: (rng.next_u64() % 128) as u8, velocity: (rng.next_u64() % 128) as u8 };
            let _ = rig.live.push(note);
        }
        calls += rig.callback();
    }
    calls
}

/// The capture thread's side of recording, at both channel counts and with
/// the ring overflowing.
fn capture() -> usize {
    let (mut tx, mut rx) = spsc::channel::<[f32; 2]>(1 << 12);
    let meter = Meter::default();
    let block: Vec<f32> = (0..2 * 1024).map(|i| (i as f32 * 0.01).sin()).collect();
    let mut calls = 0;
    for i in 0..400 {
        let channels = 1 + i % 2;
        calls += armed(|| capture_into(&mut tx, &meter, &block[..channels * 1000], channels));
        if i % 3 == 0 {
            while rx.pop().is_some() {}
        }
    }
    calls
}

/// What a MIDI backend's thread does with each byte.
fn midi() -> usize {
    let live = Mailbox::new(64);
    let mut parser = Parser::default();
    let mut rng = Rng::new(3);
    let bytes: Vec<u8> = (0..20_000).map(|_| rng.next_u64() as u8).collect();
    let calls = armed(|| {
        for &b in &bytes {
            if let Some(m) = parser.feed(b) {
                let _ = live.push(m);
            }
            if b == 0xf8 {
                live.clear();
            }
        }
    });
    while live.pop().is_some() {}
    calls
}

/// A check: how often the allocator was called while armed.
type Check = fn() -> usize;

fn main() {
    let checks: [(&str, Check); 6] = [
        ("the demo song", demo_song),
        ("every strip stage, bus and ramp, with a sample", heavy_song),
        ("edits, samples and transport commands while playing", edits_while_playing),
        ("live notes", live_notes),
        ("the capture side of recording", capture),
        ("MIDI parsing and mailboxes", midi),
    ];
    let mut failed = 0;
    for (name, check) in checks {
        match check() {
            0 => println!("ok: {name}"),
            n => {
                println!("FAILED: {name}: {n} allocator call(s) on the real-time path");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
}
