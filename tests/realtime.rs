//! The audio thread's work never allocates or frees memory.
//!
//! This binary replaces the global allocator with one that counts calls
//! while armed, and arms it only around work that runs on a real-time
//! thread: `Engine::process` (with whatever commands, samples and live
//! notes are waiting for it), the capture side of recording, the MIDI
//! byte parser and mailboxes the backend threads use, and CLAP plugin
//! instances' notes and processing, on their own and in the engine (the
//! test plugin allocates through its own copy of the standard library, so
//! what is counted is TINVO's side). It runs without the test harness, one
//! thread at a time, so nothing else allocates while armed.
//!
//! On the first allocation while armed it prints a backtrace, so a failure
//! names the code that allocated.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tinvo::clap::PluginRef;
use tinvo::clap::host::{Instance, Mode, PluginHost, Role, prepare_audio_thread};
use tinvo::dsp::noise::Rng;
use tinvo::engine::{
    ChainId, Command, Edit, Engine, LaneTarget, MAX_TRACKS, Part, PluginAt, PluginPlace, Retired, Shared, Step, Take,
};
use tinvo::group::{GROUP_PARAMS, MAX_GROUPS, group_param};
use tinvo::insert::{EffectKind, Insert, MAX_INSERTS, MAX_SLOTS};
use tinvo::instrument::zone::{Placement, Trigger};
use tinvo::instrument::{InstrumentKind, SampleMap};
use tinvo::midi::{MOD_WHEEL, Message, OutEvent, Parser, SUSTAIN};
use tinvo::mix::{MIX_PARAMS, mp, send};
use tinvo::project::{LaneData, Project};
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
    retired: Consumer<Retired>,
    live: Arc<Mailbox<Message>>,
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
        // The UI thread's side: released samples and effects are dropped
        // here.
        while self.retired.pop().is_some() {}
        calls
    }
}

/// A sampler's samples: the whole sample over the keyboard, and layered
/// over it zones of parts of it that take turns and sound on release.
fn sample(frames: usize, seed: u64) -> Arc<SampleMap> {
    let mut rng = Rng::new(seed);
    let audio: Vec<f32> = (0..frames).map(|_| rng.bipolar() * 0.5).collect();
    let s = Arc::new(Sample::from_mono(&audio, [22_050, 44_100, 48_000, 96_000][(seed % 4) as usize]).unwrap());
    let part = |k: u8, trigger| Placement {
        root: Some(60 + k),
        seq: (1 + k % 2, 2),
        trigger,
        tune: f32::from(k) * 10.0,
        start: u32::from(k) * 100,
        end: Some(u32::from(k) * 100 + 2000),
        ..Placement::WHOLE
    };
    let zones = (0..4).map(|k| (part(k, if k == 3 { Trigger::Release } else { Trigger::Attack }), s.clone()));
    Arc::new(SampleMap::new(std::iter::once((Placement::WHOLE, s.clone())).chain(zones)))
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
        ok(p.apply(Edit::SetLane { track: t, lane: 0, target: Some(LaneTarget::Track(mid)), ramp: true }));
        let spec = track_param(kind, n + sp::MID_FREQ).unwrap();
        for (step, value) in [(0, spec.min), (8, spec.max)] {
            ok(p.apply(Edit::SetPoint { track: t, lane: 0, pattern: 0, step, value: Some(value) }));
        }
    }
    let decay = LaneTarget::Mix(mp::REVERB_DECAY as u8);
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
    rig.send(Command::SetSamples { track: sampler, map: Some(sample(30_000, 1)) });
    play_song(&mut rig, 2000)
}

/// Effect `k` of a project, at settings of its own away from the defaults.
fn busy_slot(p: &mut Project, chain: ChainId, slot: u8, k: usize) {
    let kind = EffectKind::ALL[k % EffectKind::ALL.len()];
    p.apply(Edit::SetSlot { chain, slot, kind: Some(kind) }).unwrap();
    // Bypass stays off.
    for (i, spec) in kind.params().iter().enumerate().skip(1) {
        let value = spec.min + (spec.max - spec.min) * ((i + k) % 7) as f32 / 7.0;
        p.apply(Edit::SetSlotParam { chain, slot, index: i as u8, value }).unwrap();
    }
}

/// The heavy project's tracks again and again up to the limit, each sampler
/// with a sample of its own, spread over as many groups as a project holds,
/// each with every strip stage and send busy and its pan on a lane, and as
/// many busy insert effects as a project holds, some on ramps. A copy's bus
/// and master lanes are freed: only one lane in a project holds each.
fn full_song() -> usize {
    let mut p = heavy_project();
    let base = p.tracks.clone();
    for mut t in base.iter().cycle().take(MAX_TRACKS - base.len()).cloned() {
        for l in &mut t.lanes {
            if l.target.is_some_and(LaneTarget::is_global) {
                *l = LaneData::new(None, false);
            }
        }
        p.tracks.push(t);
    }
    assert_eq!(p.tracks.len(), 32);
    let ok = |r: Result<Edit, _>| r.map(drop).unwrap();
    for g in 0..MAX_GROUPS as u8 {
        ok(p.apply(Edit::InsertGroup(g)));
        for i in 0..GROUP_PARAMS {
            let spec = group_param(i).unwrap();
            let at = (i + usize::from(g)) % 5;
            let value = spec.min + (spec.max - spec.min) * at as f32 / 5.0;
            ok(p.apply(Edit::SetGroupParam { group: g, index: i as u8, value }));
        }
        let pan = Some(LaneTarget::Group { group: g, index: 1 });
        let t = 1 + g;
        ok(p.apply(Edit::SetLane { track: t, lane: 3, target: pan, ramp: true }));
        for (step, value) in [(0, -1.0), (8, 1.0)] {
            ok(p.apply(Edit::SetPoint { track: t, lane: 3, pattern: 0, step, value: Some(value) }));
        }
    }
    assert_eq!(p.groups.len(), 8);
    // Every few tracks go straight to the master.
    for t in 0..p.tracks.len() {
        let group = (t % 5 != 0).then_some((t % MAX_GROUPS) as u8);
        ok(p.apply(Edit::SetTrackGroup { track: t as u8, group }));
    }
    // A full master chain, four effects on each group and the rest on
    // tracks, three apiece.
    let mut chains = vec![ChainId::Master; MAX_SLOTS];
    chains.extend((0..MAX_GROUPS as u8).flat_map(|g| [ChainId::Group(g); 4]));
    chains.extend((0..).flat_map(|t| [ChainId::Track(t); 3]).take(MAX_INSERTS - chains.len()));
    let mut next = [0u8; 2 * MAX_TRACKS];
    for (k, &chain) in chains.iter().enumerate() {
        let n = match chain {
            ChainId::Track(t) => usize::from(t),
            ChainId::Group(g) => MAX_TRACKS + usize::from(g),
            ChainId::Master => MAX_TRACKS + MAX_GROUPS,
        };
        busy_slot(&mut p, chain, next[n], k);
        next[n] += 1;
    }
    assert_eq!(p.insert_count(), MAX_INSERTS);
    // Each track's first effect on a ramp, and the groups' second.
    for t in 0..8u8 {
        let kind = p.tracks[usize::from(t)].chain[0].as_ref().unwrap().kind;
        let target = Some(LaneTarget::Slot { slot: 0, kind, index: 1 });
        ok(p.apply(Edit::SetLane { track: t, lane: 2, target, ramp: true }));
        let g = p.groups[usize::from(t)].chain[1].as_ref().unwrap().kind;
        let target = Some(LaneTarget::GroupSlot { group: t, slot: 1, kind: g, index: 1 });
        ok(p.apply(Edit::SetLane { track: 8 + t, lane: 2, target, ramp: true }));
        for track in [t, 8 + t] {
            let spec = target_spec(&p, track, 2);
            for (step, value) in [(0, spec.min), (8, spec.max)] {
                ok(p.apply(Edit::SetPoint { track, lane: 2, pattern: 0, step, value: Some(value) }));
            }
        }
    }
    let mut rig = Rig::new(&p);
    for (t, track) in p.tracks.iter().enumerate() {
        if track.kind == InstrumentKind::Sampler {
            rig.send(Command::SetSamples { track: t as u8, map: Some(sample(30_000, t as u64)) });
        }
    }
    play_song(&mut rig, 2000)
}

/// What lane `lane` of track `track` automates.
fn target_spec(p: &Project, track: u8, lane: usize) -> &'static tinvo::instrument::ParamSpec {
    let t = &p.tracks[usize::from(track)];
    t.lanes[lane].target.unwrap().spec(t.kind).unwrap()
}

/// A random insert-chain edit, with the effect built here as the editor
/// builds it, off the audio thread.
fn random_slot_command(rng: &mut Rng) -> Command {
    let mut r = |n: u64| rng.next_u64() % n;
    let chain = match r(4) {
        0 => ChainId::Master,
        1 => ChainId::Group(r(MAX_GROUPS as u64 + 1) as u8),
        _ => ChainId::Track(r(MAX_TRACKS as u64 + 1) as u8),
    };
    let slot = r(MAX_SLOTS as u64 + 1) as u8;
    let kind = EffectKind::ALL[r(EffectKind::ALL.len() as u64) as usize];
    let edit = match r(6) {
        0 => {
            let insert = (r(4) > 0).then(|| Box::new(Insert::new(kind, SR, 120.0)));
            return Command::SetSlot { chain, slot, insert };
        }
        1 => Edit::SetSlot { chain, slot, kind: None },
        2 => Edit::SwapSlots { chain, a: slot, b: r(MAX_SLOTS as u64) as u8 },
        3 => {
            let target = match chain {
                ChainId::Master => LaneTarget::MasterSlot { slot, kind, index: r(10) as u8 },
                ChainId::Group(group) => LaneTarget::GroupSlot { group, slot, kind, index: r(10) as u8 },
                ChainId::Track(_) => LaneTarget::Slot { slot, kind, index: r(10) as u8 },
            };
            let track = r(MAX_TRACKS as u64) as u8;
            Edit::SetLane { track, lane: r(4) as u8, target: Some(target), ramp: r(2) == 0 }
        }
        _ => Edit::SetSlotParam { chain, slot, index: r(10) as u8, value: r(101) as f32 / 10.0 - 2.0 },
    };
    Command::Edit(edit)
}

/// A random edit or command, about what a busy user sends.
fn random_command(rng: &mut Rng, samples: &[Arc<SampleMap>]) -> Command {
    let mut r = |n: u64| rng.next_u64() % n;
    let track = r(MAX_TRACKS as u64 + 1) as u8;
    let kind = InstrumentKind::ALL[r(InstrumentKind::ALL.len() as u64) as usize];
    let group = r(MAX_GROUPS as u64 + 1) as u8;
    let edit = match r(27) {
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
            let target = match r(4) {
                0 => None,
                1 => Some(LaneTarget::Mix(r(MIX_PARAMS.len() as u64) as u8)),
                2 => Some(LaneTarget::Group { group, index: r(GROUP_PARAMS as u64 + 2) as u8 }),
                _ => Some(LaneTarget::Track(r(30) as u8)),
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
        21 => {
            let part = match r(3) {
                0 => Part::Follow,
                1 => Part::Rest,
                _ => Part::Pattern(r(16) as u8),
            };
            Edit::SetPart { track, index: r(8) as u16, part }
        }
        22 | 23 => match r(7) {
            0 => Edit::InsertGroup(group),
            1 => Edit::RemoveGroup(group),
            2 => Edit::SetGroupGain { group, db: r(73) as f32 - 60.0 },
            3 => Edit::SetGroupPan { group, pan: r(201) as f32 / 100.0 - 1.0 },
            4 => Edit::SetGroupMute { group, mute: r(2) == 0 },
            5 => {
                let index = r(GROUP_PARAMS as u64) as u8;
                let spec = group_param(usize::from(index)).unwrap();
                Edit::SetGroupParam { group, index, value: spec.min + (spec.max - spec.min) * (r(101) as f32 / 100.0) }
            }
            _ => Edit::SetTrackGroup { track, group: (r(4) > 0).then_some(group) },
        },
        24 | 25 => return random_slot_command(rng),
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
                    Command::SetSamples { track, map: s }
                }
            };
        }
    };
    Command::Edit(edit)
}

fn edits_while_playing() -> usize {
    let mut rig = Rig::new(&heavy_project());
    let samples: Vec<Arc<SampleMap>> = (0..6).map(|i| sample([1, 7, 64, 4800, 48_000, 200_000][i], i as u64)).collect();
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

/// A key, a pedal, a wheel or a bend on one of four channels.
fn random_message(rng: &mut Rng) -> Message {
    let channel = (rng.next_u64() % 4) as u8;
    let (a, b) = ((rng.next_u64() % 128) as u8, (rng.next_u64() % 128) as u8);
    match rng.next_u64() % 8 {
        0 => Message::Control { channel, controller: SUSTAIN, value: b },
        1 => Message::Control { channel, controller: MOD_WHEEL, value: b },
        2 => Message::Bend { channel, value: (rng.next_u64() % 16384) as i16 - 8192 },
        _ => Message::note(channel, a, b),
    }
}

fn live_notes() -> usize {
    let mut rig = Rig::new(&heavy_project());
    let mut rng = Rng::new(7);
    rig.send(Command::Play);
    let mut calls = 0;
    for i in 0..3000u64 {
        if i % 50 == 0 {
            rig.send(Command::SetLiveTrack((i / 50 % 8) as u8));
            let channel = Some((rng.next_u64() % 5) as u8).filter(|&c| c < 4);
            rig.send(Command::Edit(Edit::SetChannel { track: (rng.next_u64() % 8) as u8, channel }));
        }
        if i % 400 == 399 {
            let _ = rig.live.push(Message::Lost);
        }
        for _ in 0..rng.next_u64() % 12 {
            let _ = rig.live.push(random_message(&mut rng));
        }
        calls += rig.callback();
    }
    calls
}

/// Notes recorded while playing and after count-ins, in the pattern and the
/// song, with the take queue overflowing at times.
fn recording_notes() -> usize {
    let mut rig = Rig::new(&heavy_project());
    let (tx, mut takes) = spsc::channel::<Take>(64);
    rig.engine.attach_takes(tx);
    let mut rng = Rng::new(11);
    rig.send(Command::RecordNotes(true));
    let (mut calls, mut got) = (0, 0);
    for i in 0..3000u64 {
        match i % 600 {
            0 => rig.send(Command::CountIn { bars: 1, bar_beats: 3, beat_pulses: 12, entry: None }),
            300 => rig.send(Command::Stop),
            301 => rig.send(Command::CountIn { bars: 2, bar_beats: 4, beat_pulses: 24, entry: Some(1) }),
            599 => rig.send(Command::Stop),
            _ => {}
        }
        if i % 50 == 0 {
            rig.send(Command::SetLiveTrack((i / 50 % 8) as u8));
        }
        for _ in 0..rng.next_u64() % 6 {
            let _ = rig.live.push(random_message(&mut rng));
        }
        calls += rig.callback();
        // The UI thread's side, now and then.
        if i % 7 == 0 {
            while takes.pop().is_some() {
                got += 1;
            }
        }
    }
    assert!(got > 1000, "the engine recorded only {got} events");
    calls
}

/// MIDI out: tracks switched to it and back, live notes, the clock across
/// song entries and stops, with the output queue overflowing at times.
fn midi_out() -> usize {
    let mut rig = Rig::new(&heavy_project());
    let (tx, mut out) = spsc::channel::<OutEvent>(256);
    rig.engine.attach_midi_out(tx);
    let mut rng = Rng::new(13);
    rig.send(Command::SetMidiClock(true));
    let (mut calls, mut got) = (0, 0);
    for i in 0..3000u64 {
        match i % 500 {
            0 => rig.send(Command::PlaySong { entry: (i / 500 % 3) as u16 }),
            400 => rig.send(Command::Stop),
            450 => rig.send(Command::Play),
            499 => rig.send(Command::Stop),
            _ => {}
        }
        if i % 40 == 0 {
            let track = (rng.next_u64() % 8) as u8;
            rig.send(Command::Edit(Edit::SetMidiOut { track, on: rng.next_u64() % 3 != 0 }));
        }
        if i == 1500 {
            rig.send(Command::SetMidiClock(false));
        }
        for _ in 0..rng.next_u64() % 6 {
            let _ = rig.live.push(random_message(&mut rng));
        }
        calls += rig.callback();
        // The output thread's side, now and then.
        if i % 5 == 0 {
            while out.pop().is_some() {
                got += 1;
            }
        }
    }
    assert!(got > 5000, "the engine sent only {got} events");
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

/// The test plugin built from `examples/clap_fixture.rs`, which `cargo
/// test` builds beside this binary.
fn fixture() -> std::path::PathBuf {
    let name = if cfg!(windows) {
        "clap_fixture.dll"
    } else if cfg!(target_os = "macos") {
        "libclap_fixture.dylib"
    } else {
        "libclap_fixture.so"
    };
    let exe = std::env::current_exe().unwrap();
    exe.ancestors()
        .skip(1)
        .take(6)
        .map(|dir| dir.join("examples").join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} is not built: run cargo test, or cargo build --example clap_fixture"))
}

/// A plugin instrument and a plugin effect: notes, releases, parameter
/// changes (some flushed while the effect is skipped) and processing at
/// every buffer size, a start, and the stop before they leave, on a
/// fresh thread that has called `prepare_audio_thread`, as an audio
/// callback's would. (On macOS a thread's first use of thread-local storage
/// may call the system's allocator directly, which this allocator cannot
/// see; preparing the thread is what keeps that out of the callback.)
fn plugins() -> usize {
    let mut host = PluginHost::new();
    let file = fixture();
    // SAFETY: the test plugin, loaded only through this host.
    let create =
        |host: &mut PluginHost, id, role| unsafe { host.create(&file, id, role, Mode::Realtime, SR as f64, 64) };
    let mut sine = create(&mut host, "org.tinvo.test.sine", Role::Instrument).unwrap();
    let mut gain = create(&mut host, "org.tinvo.test.gain", Role::Effect).unwrap();
    let mut buf = vec![0.0f32; 2 * 4096];
    let mut rng = Rng::new(5);
    let mut held = Vec::with_capacity(1024);
    let mut calls = 0;
    let mut played = true;
    std::thread::scope(|s| {
        s.spawn(|| {
            prepare_audio_thread();
            for i in 0..400 {
                let frames = SIZES[i % SIZES.len()];
                let io = &mut buf[..2 * frames];
                calls += armed(|| {
                    for _ in 0..rng.next_u64() % 8 {
                        if let Some(n) = sine.note_on((rng.next_u64() % 128) as u8, 0.7) {
                            held.push(n);
                        }
                    }
                    for _ in 0..rng.next_u64() % 6 {
                        if let Some(n) = held.pop() {
                            sine.note_off(n);
                        }
                    }
                    if i % 97 == 0 {
                        sine.release_all();
                        held.clear();
                    }
                    let serial = i as u64 + 1;
                    for _ in 0..rng.next_u64() % 4 {
                        let value = (rng.next_u64() % 100) as f64 / 100.0;
                        sine.set_param(20, value, serial);
                        gain.set_param(10 + (rng.next_u64() % 2) as u32, 2.0 * value, serial);
                    }
                    played &= sine.process(io);
                    if i % 5 == 0 {
                        gain.skip(frames);
                    } else {
                        played &= gain.process(io);
                    }
                    if i == 399 {
                        sine.stop();
                        gain.stop();
                    }
                });
            }
        });
    });
    drop((sine, gain));
    let _ = host.poll();
    assert!(played, "the plugins played");
    calls
}

/// Removes the folders earlier runs left: a run cannot remove its own while
/// the plugin it loaded stays loaded, as on Windows it does until the
/// process ends.
fn sweep_earlier_runs() {
    let own = format!("tinvo-realtime-{}", std::process::id());
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("tinvo-realtime-") || name == own {
            continue;
        }
        let age = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if age.is_some_and(|a| a > std::time::Duration::from_secs(3600)) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The test plugin copied as `latency` (its output late, and later at each
/// restart) to a folder of its own.
fn late_fixture() -> std::path::PathBuf {
    sweep_earlier_runs();
    let from = fixture();
    let dir = std::env::temp_dir().join(format!("tinvo-realtime-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let to = dir.join(from.file_name().unwrap().to_string_lossy().replace("clap_fixture", "latency"));
    std::fs::copy(&from, &to).unwrap();
    to
}

/// Plugin tracks and plugin inserts in the engine, through its command
/// queue: instances put in (an insert's too), replaced, lent out and returned to be
/// restarted (one coming later each time), a plugin track added and removed
/// (its instance coming back when its fade ends), lanes and commands
/// setting their parameters, while a song plays at every buffer size.
fn plugins_in_the_engine() -> usize {
    let mut host = PluginHost::new();
    let (file, late) = (fixture(), late_fixture());
    let (sine, gain) = (PluginRef::new("org.tinvo.test.sine").unwrap(), PluginRef::new("org.tinvo.test.gain").unwrap());
    // SAFETY: the test plugin, loaded only through this host.
    let mut make_from = |file: &std::path::Path, id: PluginRef, role| {
        Box::new(unsafe { host.create(file, id.id(), role, Mode::Realtime, SR as f64, 64) }.unwrap())
    };
    let mut edits = vec![Edit::AddTrack(InstrumentKind::Plugin(sine)), Edit::AddTrack(InstrumentKind::Synth)];
    for s in 0..16u8 {
        edits.push(Edit::SetStep { track: 0, pattern: 0, step: s, value: Some(Step::new(48 + 3 * s, 100, 1 + s % 3)) });
    }
    // A ramp on the sine's level, held points on the gains', one with a
    // base.
    let lanes = [
        (LaneTarget::Plugin { at: PluginAt::Instrument, plugin: sine, id: 20 }, true),
        (LaneTarget::Plugin { at: PluginAt::Slot(0), plugin: gain, id: 10 }, false),
        (LaneTarget::Plugin { at: PluginAt::Slot(1), plugin: gain, id: 14 }, false),
    ];
    for (k, (target, ramp)) in lanes.into_iter().enumerate() {
        let lane = k as u8;
        edits.push(Edit::SetLane { track: 0, lane, target: Some(target), ramp });
        for (step, value) in [(2, 0.2), (7, 0.9), (12, 0.05)] {
            edits.push(Edit::SetPoint { track: 0, lane, pattern: 0, step, value: Some(value) });
        }
    }
    edits.push(Edit::SetLaneBase { track: 0, lane: 1, value: Some(0.6) });
    let late_gain = make_from(&late, gain, Role::Effect);
    let late_token = late_gain.token();
    let mut late_insert = Box::new(Insert::new(EffectKind::Plugin(gain), SR, 120.0));
    assert!(late_insert.attach(late_gain).unwrap().is_none());
    let mut make = |id: PluginRef, role| make_from(&file, id, role);
    let (mut commands, rx) = spsc::channel(4096);
    let (tx, mut retired) = spsc::channel(256);
    let mut engine = Engine::with_edits(SR, &edits, Arc::new(Shared::new()), Some(rx));
    engine.attach_retirement(tx);
    let mut send = |cmd| assert!(commands.push(cmd).is_ok(), "command queue full");
    let first: Box<Instance> = make(sine, Role::Instrument);
    let mut sine_token = first.token();
    send(Command::SetPlugin { track: 0, instance: first });
    let effect = make(gain, Role::Effect);
    let mut gain_token = effect.token();
    let mut insert = Box::new(Insert::new(EffectKind::Plugin(gain), SR, 120.0));
    assert!(insert.attach(effect).unwrap().is_none());
    send(Command::SetSlot { chain: ChainId::Track(0), slot: 0, insert: Some(insert) });
    send(Command::SetSlot { chain: ChainId::Track(0), slot: 1, insert: Some(late_insert) });
    send(Command::Play);
    let mut buf = vec![0.0f32; 2 * 4096];
    let (mut calls, mut heard, mut lent, mut added_token) = (0, false, 0, None);
    // The instances removed and replaced, and those that came back.
    let (mut expected, mut gone) = (Vec::new(), Vec::new());
    for i in 0..3000 {
        match i {
            1200 => {
                send(Command::Edit(Edit::AddTrack(InstrumentKind::Plugin(sine))));
                let added = make(sine, Role::Instrument);
                added_token = Some(added.token());
                expected.push(added.token());
                send(Command::SetPlugin { track: 2, instance: added });
                send(Command::Edit(Edit::SetStep { track: 2, pattern: 0, step: 3, value: Some(Step::new(70, 90, 2)) }));
            }
            1800 => {
                let next = make(gain, Role::Effect);
                expected.push(gain_token);
                gain_token = next.token();
                send(Command::SetSlotPlugin { chain: ChainId::Track(0), slot: 0, instance: next });
            }
            2000 => send(Command::Edit(Edit::RemoveTrack(2))),
            2500 => {
                let next = make(sine, Role::Instrument);
                expected.push(sine_token);
                sine_token = next.token();
                send(Command::SetPlugin { track: 0, instance: next });
            }
            _ if i % 300 == 150 => send(Command::Lend(if i % 600 == 150 { sine_token } else { gain_token })),
            _ if i % 500 == 400 => send(Command::Lend(late_token)),
            _ if i % 7 == 3 => send(Command::SetPluginParam {
                place: PluginPlace::Slot(ChainId::Track(0), 1),
                plugin: gain,
                id: 10,
                value: (i % 10) as f64 / 10.0,
                serial: i as u64,
            }),
            _ if i % 400 == 50 => {
                let value = (i % 3 == 0).then_some(0.3);
                send(Command::Edit(Edit::SetLaneBase { track: 0, lane: 2, value }));
            }
            _ => {}
        }
        let frames = SIZES[i % SIZES.len()];
        let io = &mut buf[..2 * frames];
        calls += armed(|| engine.process(io));
        heard |= io.iter().any(|x| x.abs() > 1e-6);
        // The UI thread's side: what is lent is restarted and sent back,
        // the rest dropped.
        while let Some(r) = retired.pop() {
            match r {
                Retired::Lent(mut i) => {
                    i.restart().unwrap();
                    lent += 1;
                    send(Command::Return(i));
                }
                Retired::Plugin(p) => {
                    // The added track's, only once the track is gone.
                    assert!(Some(p.token()) != added_token || i >= 2000, "track 2 played");
                    gone.push(p.token());
                }
                _ => {}
            }
        }
    }
    drop(engine);
    drop(make);
    while retired.pop().is_some() {}
    let _ = host.poll();
    assert!(heard && lent == 16, "the plugins played ({heard}) and were lent out 16 times ({lent})");
    let _ = std::fs::remove_dir_all(late.parent().unwrap());
    gone.sort_unstable();
    expected.sort_unstable();
    assert_eq!(gone, expected, "the removed and replaced instances came back");
    calls
}

/// A check: how often the allocator was called while armed.
type Check = fn() -> usize;

fn main() {
    let checks: [(&str, Check); 11] = [
        ("the demo song", demo_song),
        ("every strip stage, bus and ramp, with a sample", heavy_song),
        ("as many tracks as a project holds, likewise", full_song),
        ("edits, samples, effects and transport commands while playing", edits_while_playing),
        ("live notes", live_notes),
        ("recording notes and count-ins", recording_notes),
        ("MIDI out and its clock", midi_out),
        ("the capture side of recording", capture),
        ("MIDI parsing and mailboxes", midi),
        ("a CLAP instrument and effect playing", plugins),
        ("CLAP plugins in the engine, put in, lent out, returned and removed", plugins_in_the_engine),
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
