//! Offline (faster than real time) rendering.
//!
//! Rendering drives the very same [`Engine`] the audio thread uses, so an
//! export is bit-for-bit what the engine plays.

use crate::engine::transport::Transport;
use crate::engine::{Command, Engine, Shared};
use crate::project::Project;
use crate::sample::Sample;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderOptions {
    pub sample_rate: u32,
    /// How many times the pattern plays.
    pub loops: u32,
    /// Upper bound on the release tail rendered after the last loop.
    pub max_tail_seconds: f32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { sample_rate: 48_000, loops: 4, max_tail_seconds: 10.0 }
    }
}

/// Level below which the tail is considered silent (-90 dBFS).
const TAIL_THRESHOLD: f32 = 3.2e-5;
/// The tail ends after this much continuous silence.
const TAIL_QUIET_SECONDS: f32 = 0.1;

/// Frames covered by `loops` repetitions of the pattern.
pub fn pattern_frames(project: &Project, sample_rate: u32, loops: u32) -> u64 {
    let sps = Transport::samples_per_step(project.tempo, sample_rate as f32);
    (sps * f64::from(project.pattern_len) * f64::from(loops)).round() as u64
}

/// Renders the project to interleaved stereo. `samples` holds each track's
/// sample (see [`Project::load_samples`]); missing entries play silence.
pub fn render(project: &Project, samples: &[Option<Arc<Sample>>], opts: &RenderOptions) -> Vec<f32> {
    let sr = opts.sample_rate;
    let mut engine = Engine::with_edits(sr as f32, &project.to_edits(), Arc::new(Shared::new()), None);
    for (track, sample) in samples.iter().enumerate() {
        engine.set_sample(track, sample.clone());
    }
    engine.handle(Command::Play);
    let main = pattern_frames(project, sr, opts.loops) as usize;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_renders_deterministically() {
        let p = Project::demo();
        let opts = RenderOptions { loops: 1, ..Default::default() };
        let a = render(&p, &[], &opts);
        let b = render(&p, &[], &opts);
        assert_eq!(a, b);
        let main = pattern_frames(&p, 48_000, 1) as usize;
        assert!(a.len() / 2 >= main);
        assert!(a.len() / 2 < main + 48_000 * 2, "tail too long: {}", a.len() / 2 - main);
        let peak = a.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.1 && peak <= 1.0, "peak {peak}");
        assert!(a.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn length_follows_tempo_and_loops() {
        let mut p = Project::new();
        p.tempo = 120.0;
        p.pattern_len = 16;
        assert_eq!(pattern_frames(&p, 48_000, 1), 96_000);
        assert_eq!(pattern_frames(&p, 48_000, 3), 288_000);
        // An empty project renders exactly the pattern, no tail.
        let out = render(&p, &[], &RenderOptions { loops: 2, ..Default::default() });
        assert_eq!(out.len(), 192_000 * 2);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn tail_is_capped() {
        let mut p = Project::new();
        p.apply(crate::engine::Edit::AddTrack(crate::instrument::InstrumentKind::Synth)).unwrap();
        // Ten-second release on a note held to the end of the pattern.
        p.tracks[0].params[16] = 10.0;
        p.tracks[0].steps[0] = Some(crate::engine::Step::new(48, 127, 16));
        let opts = RenderOptions { loops: 1, max_tail_seconds: 1.0, ..Default::default() };
        let out = render(&p, &[], &opts);
        assert_eq!(out.len() / 2, pattern_frames(&p, 48_000, 1) as usize + 48_000);
    }
}
