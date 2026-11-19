//! Latency compensation: a delay on a track's or group's output that lines
//! it up with paths through effects that look ahead.
//!
//! The engine sets how many frames each one delays so that every path
//! reaches the master equally late. A change of delay crossfades from the
//! old tap to the new one over 10 ms, counted in frames, once the history
//! reaches back to both; a change made during a crossfade follows it. With
//! no delay and
//! no crossfade it leaves the signal untouched and writes nothing, so a
//! project without look-ahead costs nothing.

use crate::dsp::{ring_back, ring_next};

/// Length of a crossfade between taps.
const FADE_SECONDS: f32 = 0.01;

/// The longest delay a path can need at `sample_rate`: every slot of a
/// track's chain and of its group's holding a gate, and the plugins on it
/// coming as late as is made up for
/// ([`MAX_LATENCY_SECONDS`](crate::clap::host::MAX_LATENCY_SECONDS)).
pub fn longest(sample_rate: f32) -> usize {
    2 * crate::insert::MAX_SLOTS * crate::dsp::gate::lookahead_frames(sample_rate)
        + crate::clap::host::max_latency(f64::from(sample_rate))
}

#[derive(Clone, Debug)]
pub struct Align {
    /// Interleaved stereo history, `len` frames.
    buf: Vec<f32>,
    len: usize,
    /// The frame written last.
    pos: usize,
    /// Frames written since it last began, up to `len`; older places read
    /// as silence.
    fresh: usize,
    /// The delay crossfaded from and to, and the frames of that left.
    from: usize,
    to: usize,
    left: u32,
    fade: u32,
    /// The delay set during a crossfade, to crossfade to after it.
    next: Option<usize>,
}

impl Align {
    /// An aligner delaying up to `max` frames. Allocates.
    pub fn new(max: usize, sample_rate: f32) -> Self {
        let len = max + 1;
        Self {
            buf: vec![0.0; 2 * len],
            len,
            pos: 0,
            fresh: 0,
            from: 0,
            to: 0,
            left: 0,
            fade: ((FADE_SECONDS * sample_rate) as u32).max(1),
            next: None,
        }
    }

    /// The longest delay it makes, in frames.
    pub fn longest(&self) -> usize {
        self.len - 1
    }

    /// The delay it is set to, in frames.
    pub fn delay(&self) -> usize {
        self.to
    }

    /// Whether it does anything: a delay or a crossfade.
    pub fn is_active(&self) -> bool {
        self.to > 0 || self.left > 0
    }

    /// Delays by `frames` (at most its longest), crossfading from where it
    /// is, or after the crossfade under way.
    pub fn set(&mut self, frames: usize) {
        let frames = frames.min(self.len - 1);
        // A crossfade that has not moved yet (set twice on one frame, or
        // waiting for its history) just takes the new target.
        if self.left == self.fade && self.fade > 0 {
            self.next = None;
            if frames == self.from {
                (self.to, self.left) = (frames, 0);
            } else {
                self.to = frames;
            }
            return;
        }
        if self.left > 0 {
            self.next = (frames != self.to).then_some(frames);
            return;
        }
        if frames == self.to {
            return;
        }
        if !self.is_active() {
            self.fresh = 0;
        }
        self.from = self.to;
        self.to = frames;
        self.left = self.fade;
    }

    /// Lands a crossfade, and one waiting, for a project that is opened.
    pub fn snap(&mut self) {
        if let Some(n) = self.next.take() {
            self.to = n;
        }
        self.from = self.to;
        self.left = 0;
    }

    /// Forgets what it holds, keeping its delay: silence comes out until
    /// new input has gone through.
    pub fn forget(&mut self) {
        self.fresh = 0;
    }

    /// Back to no delay with no history, for a fresh track.
    pub fn reset(&mut self) {
        (self.from, self.to, self.left, self.fresh, self.next) = (0, 0, 0, 0, None);
    }

    #[inline]
    fn tap(&self, back: usize) -> (f32, f32) {
        if back < self.fresh {
            let at = 2 * ring_back(self.pos, back, self.len);
            (self.buf[at], self.buf[at + 1])
        } else {
            (0.0, 0.0)
        }
    }

    /// Delays `buf` (interleaved stereo) in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        if !self.is_active() {
            return;
        }
        for f in buf.chunks_exact_mut(2) {
            self.pos = ring_next(self.pos, self.len);
            self.buf[2 * self.pos] = f[0];
            self.buf[2 * self.pos + 1] = f[1];
            if self.fresh < self.len {
                self.fresh += 1;
            }
            let new = self.tap(self.to);
            if self.left == 0 {
                (f[0], f[1]) = new;
            } else if self.fresh <= self.to.max(self.from) {
                // The new tap reaches back before the history: the old one
                // plays on until it is there.
                (f[0], f[1]) = self.tap(self.from);
            } else {
                let old = self.tap(self.from);
                let w = self.left as f32 / self.fade as f32;
                f[0] = new.0 + w * (old.0 - new.0);
                f[1] = new.1 + w * (old.1 - new.1);
                self.left -= 1;
                if self.left == 0 {
                    if let Some(n) = self.next.take() {
                        (self.from, self.to, self.left) = (self.to, n, self.fade);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames).flat_map(|i| [i as f32 + 1.0, -(i as f32) - 1.0]).collect()
    }

    #[test]
    fn with_no_delay_it_leaves_the_signal_alone() {
        let mut a = Align::new(100, SR);
        let x = ramp(64);
        let mut y = x.clone();
        a.process(&mut y);
        assert_eq!(y, x);
    }

    /// The largest step from frame to frame, either side, past the first
    /// `skip` frames.
    fn largest_step(v: &[f32], skip: usize) -> f32 {
        let mut m = 0.0f32;
        for c in 0..2 {
            let steps = v.chunks_exact(2).zip(v.chunks_exact(2).skip(1)).map(|(p, q)| (q[c] - p[c]).abs());
            m = m.max(steps.skip(skip).fold(0.0f32, f32::max));
        }
        m
    }

    #[test]
    fn a_delay_longer_than_its_crossfade_waits_for_its_history() {
        let mut a = Align::new(1000, SR);
        a.set(700);
        let x = ramp(3000);
        let mut y = x.clone();
        for c in y.chunks_mut(2 * 64) {
            a.process(c);
        }
        // The input plays on until 700 frames are there, then crossfades.
        assert_eq!(&y[..2 * 700], &x[..2 * 700]);
        assert!(largest_step(&y, 0) < 1.0 + 700.0 / 480.0 + 0.01, "{}", largest_step(&y, 0));
        assert_eq!(&y[2 * 1180..], &x[2 * 480..x.len() - 2 * 700]);
    }

    #[test]
    fn a_delay_that_comes_back_forgets_what_it_held() {
        let mut a = Align::new(100, SR);
        a.set(50);
        a.snap();
        a.process(&mut ramp(600));
        a.set(0);
        a.process(&mut ramp(600));
        assert!(!a.is_active());
        // Off, it hears nothing; on again, the old ramp must not come back.
        a.set(50);
        let mut y = vec![0.0; 2 * 600];
        a.process(&mut y);
        assert!(y.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn a_target_set_again_before_it_moves_replaces_it() {
        let mut a = Align::new(100, SR);
        a.set(50);
        a.set(0);
        assert!(!a.is_active());
        a.set(30);
        a.snap();
        a.set(60);
        a.set(10);
        let x = ramp(2000);
        let mut y = x.clone();
        a.process(&mut y);
        // One crossfade, from 30 straight to 10.
        assert_eq!(&y[2 * 600..], &x[2 * 590..x.len() - 2 * 10]);
    }

    #[test]
    fn a_change_during_a_crossfade_follows_it() {
        let mut a = Align::new(100, SR);
        a.set(40);
        a.snap();
        let x = ramp(4000);
        let mut y = x.clone();
        let (head, tail) = y.split_at_mut(2 * 1000);
        a.process(head);
        a.set(10);
        let (mid, rest) = tail.split_at_mut(2 * 200);
        a.process(mid);
        a.set(70);
        a.process(rest);
        // From 40 to 10, then from 10 to 70: never a jump.
        assert!(largest_step(&y, 40) < 1.0 + 60.0 / 480.0 + 0.01, "{}", largest_step(&y, 40));
        let end = 1000 + 960;
        assert_eq!(&y[2 * end..], &x[2 * (end - 70)..x.len() - 2 * 70]);
    }

    #[test]
    fn a_settled_delay_is_exact_and_a_change_crossfades() {
        let mut a = Align::new(100, SR);
        a.set(30);
        a.snap();
        let x = ramp(2000);
        let mut y = x.clone();
        for c in y.chunks_mut(2 * 64) {
            a.process(c);
        }
        assert!(y[..60].iter().all(|&v| v == 0.0));
        assert_eq!(&y[60..], &x[..x.len() - 60]);
        // Down to 10: for 480 frames both taps, then the new one alone.
        a.set(10);
        let more = ramp(2000).iter().map(|v| v + 2000.0f32.copysign(*v)).collect::<Vec<_>>();
        let mut z = more.clone();
        for c in z.chunks_mut(2 * 64) {
            a.process(c);
        }
        assert_eq!(&z[2 * 480..], &more[2 * (480 - 10)..more.len() - 2 * 10]);
        let all = [&y[..], &z[..]].concat();
        for c in 0..2 {
            let steps = all.chunks_exact(2).zip(all.chunks_exact(2).skip(1)).map(|(p, q)| (q[c] - p[c]).abs());
            assert!(steps.skip(30).fold(0.0f32, f32::max) < 1.05, "no jump along the way");
        }
    }
}
