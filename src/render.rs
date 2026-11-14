//! Offline (faster than real time) rendering.
//!
//! Rendering drives the very same [`Engine`] the audio thread uses, so an
//! export is bit-for-bit what the engine plays.

use crate::engine::transport::Transport;
use crate::engine::{Command, Edit, Engine, MAX_PATTERNS, Shared};
use crate::project::Project;
use crate::sample::Sample;
use std::sync::Arc;

/// What a render plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The song if it has entries, otherwise pattern A.
    Auto,
    /// One pattern on repeat.
    Pattern(u8),
    /// The whole song, start to end, ignoring the loop region.
    Song,
    /// The song's loop region.
    Region,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderOptions {
    pub sample_rate: u32,
    /// How many times the source plays.
    pub loops: u32,
    /// Upper bound on the release tail rendered after the last loop.
    pub max_tail_seconds: f32,
    pub source: Source,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { sample_rate: 48_000, loops: 4, max_tail_seconds: 10.0, source: Source::Auto }
    }
}

/// Level below which the tail is considered silent (-90 dBFS).
const TAIL_THRESHOLD: f32 = 3.2e-5;
/// The tail ends after this much continuous silence.
const TAIL_QUIET_SECONDS: f32 = 0.1;

/// The source a render actually plays: `Auto` resolved, and `Song` or
/// `Region` falling back to pattern A when there is nothing to play.
pub fn resolve(project: &Project, source: Source) -> Source {
    match source {
        Source::Auto | Source::Song if !project.song.is_empty() => Source::Song,
        Source::Region if project.active_loop().is_some() => Source::Region,
        Source::Pattern(p) if usize::from(p) < MAX_PATTERNS => source,
        _ => Source::Pattern(0),
    }
}

/// Steps in one pass of `source`.
pub fn source_steps(project: &Project, source: Source) -> u64 {
    match resolve(project, source) {
        Source::Song => project.song_steps(),
        Source::Region => {
            let (a, b) = project.active_loop().unwrap_or((0, 0));
            let lens = &project.pattern_lens;
            project.song[usize::from(a)..=usize::from(b)].iter().map(|&p| u64::from(lens[usize::from(p)])).sum()
        }
        Source::Pattern(p) => u64::from(project.pattern_lens[usize::from(p)]),
        Source::Auto => unreachable!("resolved above"),
    }
}

/// Frames covered by `opts.loops` passes of the source, before the tail.
pub fn main_frames(project: &Project, opts: &RenderOptions) -> u64 {
    let sps = Transport::samples_per_step(project.tempo, opts.sample_rate as f32);
    (sps * (source_steps(project, opts.source) * u64::from(opts.loops)) as f64).round() as u64
}

/// Frames the engine renders per call while exporting.
const CHUNK: usize = 4096;

/// Renders the project as interleaved stereo, handing it to `sink` block by
/// block, and returns the number of frames rendered. Memory use does not
/// depend on the length of the render. `samples` holds each track's sample
/// (see [`Project::load_samples`]); missing entries play silence. A `sink`
/// error stops the render and is returned.
pub fn render_into(
    project: &Project,
    samples: &[Option<Arc<Sample>>],
    opts: &RenderOptions,
    sink: &mut dyn FnMut(&[f32]) -> Result<(), String>,
) -> Result<u64, String> {
    let sr = opts.sample_rate;
    let mut engine = Engine::with_edits(sr as f32, &project.to_edits(), Arc::new(Shared::new()), None);
    for (track, sample) in samples.iter().enumerate() {
        engine.set_sample(track, sample.clone());
    }
    match resolve(project, opts.source) {
        Source::Song => {
            engine.apply(Edit::SetLoop(None));
            engine.handle(Command::PlaySong { entry: 0 });
        }
        Source::Region => {
            let (a, _) = project.active_loop().unwrap_or((0, 0));
            engine.handle(Command::PlaySong { entry: a });
        }
        Source::Pattern(p) => {
            engine.handle(Command::Cue(p));
            engine.handle(Command::Play);
        }
        Source::Auto => unreachable!("resolved above"),
    }
    let mut buf = vec![0.0f32; CHUNK * 2];
    let main = main_frames(project, opts);
    // The master limiter delays the output; render that much ahead and drop
    // it, so the file still starts on the first beat. The transport stops
    // at the same musical point either way.
    let mut skip = engine.latency() as u64;
    while skip > 0 {
        let n = skip.min(CHUNK as u64) as usize;
        engine.process(&mut buf[..n * 2]);
        skip -= n as u64;
    }
    let stop_at = main.saturating_sub(engine.latency() as u64);
    let mut done = 0u64;
    let mut stopped = false;
    while done < main {
        if !stopped && done == stop_at {
            engine.handle(Command::Stop);
            stopped = true;
        }
        let limit = if stopped { main } else { stop_at };
        let n = (limit - done).min(CHUNK as u64) as usize;
        engine.process(&mut buf[..n * 2]);
        sink(&buf[..n * 2])?;
        done += n as u64;
    }
    if !stopped {
        engine.handle(Command::Stop);
    }

    // The tail ends after `quiet_needed` frames of silence once the effects
    // hold nothing more (a long delay's echo can follow a quiet spell). The
    // silence is not part of the result: quiet blocks wait in `held` until
    // sound follows.
    let max_tail = (opts.max_tail_seconds.max(0.0) * sr as f32) as usize;
    let quiet_needed = (TAIL_QUIET_SECONDS * sr as f32) as usize;
    let mut held: Vec<f32> = Vec::new();
    let mut tail = 0usize;
    while tail < max_tail && (held.len() / 2 < quiet_needed || !engine.effects_idle()) {
        let n = 512.min(max_tail - tail);
        let b = &mut buf[..n * 2];
        engine.process(b);
        tail += n;
        if b.iter().all(|x| x.abs() < TAIL_THRESHOLD) {
            held.extend_from_slice(b);
        } else {
            if !held.is_empty() {
                sink(&held)?;
                done += (held.len() / 2) as u64;
                held.clear();
            }
            sink(b)?;
            done += n as u64;
        }
    }
    Ok(done)
}

/// [`render_into`] collected in memory.
pub fn render(project: &Project, samples: &[Option<Arc<Sample>>], opts: &RenderOptions) -> Vec<f32> {
    let mut out = Vec::new();
    let mut sink = |b: &[f32]| {
        out.extend_from_slice(b);
        Ok(())
    };
    render_into(project, samples, opts, &mut sink).expect("collecting cannot fail");
    out
}

/// The tracks that get a stem: the audible ones.
pub fn stem_tracks(project: &Project) -> Vec<usize> {
    let any_solo = project.tracks.iter().any(|t| t.solo);
    (0..project.tracks.len()).filter(|&i| !project.tracks[i].mute && (!any_solo || project.tracks[i].solo)).collect()
}

/// The project as heard in the stem of track `i`. Other tracks are muted,
/// not removed: they still use the chance generator and choke open hats
/// exactly as in the mix, so the stems add up to it. A stem carries the
/// track's own share of the delay and reverb returns, and is taken before
/// the master limiter.
pub fn stem_project(project: &Project, i: usize) -> Project {
    let mut solo = project.clone();
    for (j, t) in solo.tracks.iter_mut().enumerate() {
        t.mute = j != i;
        t.solo = false;
    }
    solo.mix[crate::mix::mp::LIMITER] = 0.0;
    // A lane could switch the limiter back on.
    let limiter = crate::engine::MIX_TARGET + crate::mix::mp::LIMITER as u8;
    for lane in solo.tracks.iter_mut().flat_map(|t| t.lanes.iter_mut()) {
        if lane.target == Some(limiter) {
            *lane = crate::project::LaneData::new(None, false);
        }
    }
    solo
}

/// Renders each audible track on its own (with the master level applied),
/// as `(track index, audio)`. All stems are padded to the same length, so
/// they line up when imported elsewhere; added together they make the mix.
pub fn render_stems(
    project: &Project,
    samples: &[Option<Arc<Sample>>],
    opts: &RenderOptions,
) -> Vec<(usize, Vec<f32>)> {
    let mut stems: Vec<(usize, Vec<f32>)> =
        stem_tracks(project).into_iter().map(|i| (i, render(&stem_project(project, i), samples, opts))).collect();
    let len = stems.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
    for (_, s) in &mut stems {
        s.resize(len, 0.0);
    }
    stems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Step;
    use crate::instrument::InstrumentKind;

    #[test]
    fn demo_renders_deterministically() {
        let p = Project::demo();
        let opts = RenderOptions { loops: 1, ..Default::default() };
        let a = render(&p, &[], &opts);
        let b = render(&p, &[], &opts);
        assert_eq!(a, b);
        let main = main_frames(&p, &opts) as usize;
        assert!(a.len() / 2 >= main);
        // The demo's delay repeats every 0.49 s at 35 % feedback: its last
        // repeat above the tail's threshold (-90 dB) ends about 2.3 s on.
        assert!(a.len() / 2 < main + 48_000 * 3, "tail too long: {}", a.len() / 2 - main);
        let peak = a.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.1 && peak <= 1.0, "peak {peak}");
        assert!(a.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn length_follows_tempo_loops_and_source() {
        let mut p = Project::new();
        p.tempo = 120.0;
        let opts = |loops, source| RenderOptions { loops, source, ..Default::default() };
        assert_eq!(main_frames(&p, &opts(1, Source::Auto)), 96_000);
        assert_eq!(main_frames(&p, &opts(3, Source::Auto)), 288_000);
        // An empty project renders exactly the pattern, no tail.
        let out = render(&p, &[], &opts(2, Source::Auto));
        assert_eq!(out.len(), 192_000 * 2);
        assert!(out.iter().all(|&x| x == 0.0));
        // Song and region follow each pattern's length.
        p.pattern_lens[1] = 8;
        p.pattern_lens[2] = 4;
        p.song = vec![0, 1, 2, 1];
        p.loop_region = Some((1, 2));
        assert_eq!(resolve(&p, Source::Auto), Source::Song);
        assert_eq!(main_frames(&p, &opts(1, Source::Song)), 36 * 6000);
        assert_eq!(main_frames(&p, &opts(2, Source::Region)), 2 * 12 * 6000);
        assert_eq!(main_frames(&p, &opts(1, Source::Pattern(2))), 4 * 6000);
        // A region past the end of the song is not active.
        p.loop_region = Some((2, 9));
        assert_eq!(resolve(&p, Source::Region), Source::Pattern(0));
    }

    /// Frames where sound starts after at least 1000 silent frames.
    fn onsets(out: &[f32]) -> Vec<usize> {
        let left: Vec<f32> = out.chunks(2).map(|f| f[0].abs()).collect();
        let mut v = Vec::new();
        let mut silent_run = usize::MAX;
        for (i, &x) in left.iter().enumerate() {
            if x > 1e-9 {
                if silent_run >= 1000 {
                    v.push(i);
                }
                silent_run = 0;
            } else {
                silent_run = silent_run.saturating_add(1);
            }
        }
        v
    }

    #[test]
    fn the_tail_keeps_an_echo_that_follows_a_quiet_spell() {
        use crate::mix::mp;
        // One short hat at 60 BPM into a half-note delay: the echo comes
        // 2 s after the hit, long after the pattern's 1 s is over.
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::HatClosed)).unwrap();
        p.apply(Edit::SetPatternLength { pattern: 0, len: 4 }).unwrap();
        p.tracks[0].steps[0][0] = Some(Step::new(60, 127, 1));
        p.tempo = 60.0;
        let delay_send = InstrumentKind::HatClosed.params().len() + crate::strip::sp::COUNT + crate::mix::send::DELAY;
        p.apply(Edit::SetParam { track: 0, index: delay_send as u8, value: 0.0 }).unwrap();
        p.mix[mp::DELAY_TIME] = (crate::mix::DIVISIONS.len() - 1) as f32;
        p.mix[mp::DELAY_FEEDBACK] = 0.0;
        let opts = RenderOptions { loops: 1, source: Source::Pattern(0), ..Default::default() };
        let out = render(&p, &[], &opts);
        assert!(out.len() / 2 > 96_000, "{} frames", out.len() / 2);
        let echo = out[2 * 95_000..].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(echo > 0.01, "{echo}");
    }

    /// A short closed hat on step 0 of pattern A and step 2 of pattern B
    /// (4 steps each).
    fn two_patterns() -> Project {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::HatClosed)).unwrap();
        for pattern in [0, 1] {
            p.apply(Edit::SetPatternLength { pattern, len: 4 }).unwrap();
        }
        p.tracks[0].steps[0][0] = Some(Step::new(60, 127, 1));
        p.tracks[0].steps[1][2] = Some(Step::new(60, 127, 1));
        p
    }

    #[test]
    fn song_and_region_play_patterns_in_order() {
        let mut p = two_patterns();
        let quick = |p: &Project, source| {
            let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source, ..Default::default() };
            onsets(&render(p, &[], &opts))
        };
        // Steps are 6000 frames at 120 BPM.
        assert_eq!(quick(&p, Source::Pattern(1)), [12_000]);
        p.song = vec![1, 0, 0, 1];
        assert_eq!(quick(&p, Source::Song), [12_000, 24_000, 48_000, 84_000]);
        // The region repeats entries 1..=2 (A, A); the song plays straight
        // through regardless.
        p.loop_region = Some((1, 2));
        let opts = RenderOptions { loops: 2, max_tail_seconds: 0.0, source: Source::Region, ..Default::default() };
        assert_eq!(onsets(&render(&p, &[], &opts)), [0, 24_000, 48_000, 72_000]);
        assert_eq!(quick(&p, Source::Song), [12_000, 24_000, 48_000, 84_000]);
    }

    #[test]
    fn exports_play_each_track_s_part() {
        let mut p = two_patterns();
        p.song = vec![0, 0, 0];
        p.tracks[0].parts[1] = crate::engine::Part::Pattern(1);
        p.tracks[0].parts[2] = crate::engine::Part::Rest;
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        // Steps are 6000 frames: A on entry 0, B's step 2 on entry 1, a rest.
        assert_eq!(onsets(&render(&p, &[], &opts)), [0, 36_000]);
        // Stems of a song with parts still add up to the mix.
        let mut q = Project::demo();
        q.mix[crate::mix::mp::LIMITER] = 0.0;
        q.tracks[1].parts[1] = crate::engine::Part::Rest;
        q.tracks[5].parts[0] = crate::engine::Part::Pattern(1);
        let mix = render(&q, &[], &opts);
        let stems = render_stems(&q, &[], &opts);
        let worst = (0..mix.len())
            .map(|k| (stems.iter().map(|(_, s)| s[k]).sum::<f32>() - mix[k]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "stems differ from the mix by {worst}");
        assert_ne!(
            mix,
            render(&Project { tracks: Project::demo().tracks, ..q.clone() }, &[], &opts),
            "the parts are heard"
        );
    }

    #[test]
    fn a_stem_has_no_limiter_even_where_a_lane_turns_it_on() {
        use crate::mix::mp;
        let mut p = two_patterns();
        p.mix[mp::LIMITER] = 0.0;
        p.mix[mp::LIMITER_CEILING] = -30.0;
        let plain = p.clone();
        p.apply(Edit::SetLane {
            track: 0,
            lane: 1,
            target: Some(crate::engine::MIX_TARGET + mp::LIMITER as u8),
            ramp: false,
        })
        .unwrap();
        p.apply(Edit::SetPoint { track: 0, lane: 1, pattern: 0, step: 0, value: Some(1.0) }).unwrap();
        let stem = stem_project(&p, 0);
        assert!(stem.tracks.iter().all(|t| t.lanes.iter().all(|l| l.target.is_none())));
        // The hat goes well past the -30 dB ceiling, unlimited.
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Pattern(0), ..Default::default() };
        let out = render(&stem, &[], &opts);
        assert_eq!(out, render(&stem_project(&plain, 0), &[], &opts));
        assert!(out.iter().fold(0.0f32, |m, x| m.max(x.abs())) > 0.1);
    }

    #[test]
    fn stems_add_up_to_the_mix() {
        let mut p = Project::demo();
        p.tracks[4].mute = true; // a muted track gets no stem
        // Stems are taken before the master limiter, so they add up to the
        // mix without it.
        p.mix[crate::mix::mp::LIMITER] = 0.0;
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        let mix = render(&p, &[], &opts);
        let stems = render_stems(&p, &[], &opts);
        assert_eq!(stems.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1, 2, 3, 5]);
        for (_, s) in &stems {
            assert_eq!(s.len(), mix.len());
        }
        let worst = (0..mix.len())
            .map(|k| (stems.iter().map(|(_, s)| s[k]).sum::<f32>() - mix[k]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "stems differ from the mix by {worst}");
    }

    #[test]
    fn the_limiter_keeps_exports_on_the_beat_and_under_the_ceiling() {
        use crate::mix::mp;
        let mut p = Project::demo();
        p.mix[mp::LIMITER] = 0.0;
        p.master_gain_db = 0.0;
        // No tail: a louder mix would take longer to fall silent.
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        let plain = render(&p, &[], &opts);
        // Below the ceiling the limiter changes nothing, not even timing.
        p.mix[mp::LIMITER] = 1.0;
        p.mix[mp::LIMITER_CEILING] = 0.0;
        let same = render(&p, &[], &opts);
        assert!(
            same == plain,
            "{} of {} samples differ",
            same.iter().zip(&plain).filter(|(a, b)| a != b).count(),
            plain.len()
        );
        // Pushed into it, the file starts at the same frame and stays under.
        p.master_gain_db = 12.0;
        p.mix[mp::LIMITER_CEILING] = -1.0;
        let loud = render(&p, &[], &opts);
        assert_eq!(loud.len(), plain.len());
        let first = |x: &[f32]| x.iter().position(|v| v.abs() > 1e-4);
        assert_eq!(first(&loud), first(&plain));
        let mut tp = [crate::dsp::limiter::TruePeak::new(), crate::dsp::limiter::TruePeak::new()];
        let peak = loud.chunks(2).fold(0.0f32, |m, f| m.max(tp[0].push(f[0])).max(tp[1].push(f[1])));
        assert!(crate::dsp::gain_to_db(peak) <= -0.95, "{}", crate::dsp::gain_to_db(peak));
    }

    #[test]
    fn tail_is_capped() {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
        // Ten-second release on a note held to the end of the pattern.
        p.tracks[0].params[16] = 10.0;
        p.tracks[0].steps[0][0] = Some(Step::new(48, 127, 16));
        let opts = RenderOptions { loops: 1, max_tail_seconds: 1.0, ..Default::default() };
        let out = render(&p, &[], &opts);
        assert_eq!(out.len() / 2, main_frames(&p, &opts) as usize + 48_000);
    }
}
