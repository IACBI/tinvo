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

/// Renders the project to interleaved stereo. `samples` holds each track's
/// sample (see [`Project::load_samples`]); missing entries play silence.
pub fn render(project: &Project, samples: &[Option<Arc<Sample>>], opts: &RenderOptions) -> Vec<f32> {
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
    let main = main_frames(project, opts) as usize;
    let mut out = vec![0.0f32; main * 2];
    engine.process(&mut out);
    engine.handle(Command::Stop);

    let chunk = 512usize;
    let max_tail = (opts.max_tail_seconds.max(0.0) * sr as f32) as usize;
    let quiet_needed = (TAIL_QUIET_SECONDS * sr as f32) as usize;
    let mut quiet = 0usize;
    let mut tail = 0usize;
    let mut buf = vec![0.0f32; chunk * 2];
    while tail < max_tail && quiet < quiet_needed {
        let n = chunk.min(max_tail - tail);
        let b = &mut buf[..n * 2];
        engine.process(b);
        out.extend_from_slice(b);
        tail += n;
        if b.iter().all(|x| x.abs() < TAIL_THRESHOLD) {
            quiet += n;
        } else {
            quiet = 0;
        }
    }
    // Drop the trailing silence used to detect the end of the tail.
    let keep = (main + tail - quiet.min(tail)) * 2;
    out.truncate(keep);
    out
}

/// Renders each audible track on its own (with the master level applied),
/// as `(track index, audio)`. All stems have the same length, so they line
/// up when imported elsewhere; added together they make the mix.
pub fn render_stems(
    project: &Project,
    samples: &[Option<Arc<Sample>>],
    opts: &RenderOptions,
) -> Vec<(usize, Vec<f32>)> {
    let any_solo = project.tracks.iter().any(|t| t.solo);
    let audible: Vec<usize> = (0..project.tracks.len())
        .filter(|&i| !project.tracks[i].mute && (!any_solo || project.tracks[i].solo))
        .collect();
    let mut stems: Vec<(usize, Vec<f32>)> = audible
        .iter()
        .map(|&i| {
            // Other tracks are muted, not removed: they still use the chance
            // generator and choke open hats exactly as in the mix.
            let mut solo = project.clone();
            for (j, t) in solo.tracks.iter_mut().enumerate() {
                t.mute = j != i;
                t.solo = false;
            }
            (i, render(&solo, samples, opts))
        })
        .collect();
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
        assert!(a.len() / 2 < main + 48_000 * 2, "tail too long: {}", a.len() / 2 - main);
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
    fn stems_add_up_to_the_mix() {
        let mut p = Project::demo();
        p.tracks[4].mute = true; // a muted track gets no stem
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
