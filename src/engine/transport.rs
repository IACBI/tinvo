//! Sample-accurate step clock.
//!
//! Step boundaries are computed from an origin rather than accumulated, so
//! timing never drifts no matter how long playback runs. Tempo changes move
//! the origin to the current position, keeping the playhead continuous.

/// Sequencer resolution: steps per quarter note (16th notes).
pub const STEPS_PER_BEAT: u32 = 4;

#[derive(Clone, Debug)]
pub struct Transport {
    playing: bool,
    samples_per_step: f64,
    origin_frame: u64,
    origin_step: f64,
    /// Absolute index (since play) of the next step to fire.
    next_step: u64,
}

impl Transport {
    pub fn new(bpm: f32, sample_rate: f32) -> Self {
        Self {
            playing: false,
            samples_per_step: Self::samples_per_step(bpm, sample_rate),
            origin_frame: 0,
            origin_step: 0.0,
            next_step: 0,
        }
    }

    pub fn samples_per_step(bpm: f32, sample_rate: f32) -> f64 {
        f64::from(sample_rate) * 60.0 / (f64::from(bpm) * f64::from(STEPS_PER_BEAT))
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Starts from step 0 at `now`.
    pub fn start(&mut self, now: u64) {
        self.playing = true;
        self.origin_frame = now;
        self.origin_step = 0.0;
        self.next_step = 0;
    }

    pub fn stop(&mut self) {
        self.playing = false;
    }

    pub fn set_tempo(&mut self, bpm: f32, sample_rate: f32, now: u64) {
        if self.playing {
            self.origin_step = self.position_at(now).min(self.next_step as f64);
            self.origin_frame = now;
        }
        self.samples_per_step = Self::samples_per_step(bpm, sample_rate);
    }

    /// Fractional step position at `frame` (which must not precede the origin).
    pub fn position_at(&self, frame: u64) -> f64 {
        self.origin_step + frame.saturating_sub(self.origin_frame) as f64 / self.samples_per_step
    }

    /// Frame at which absolute step `step` starts.
    pub fn frame_of_step(&self, step: u64) -> u64 {
        let offset = ((step as f64 - self.origin_step) * self.samples_per_step).round();
        self.origin_frame + offset.max(0.0) as u64
    }

    /// Frame of the next step to fire, if playing.
    pub fn next_step_frame(&self) -> Option<u64> {
        self.playing.then(|| self.frame_of_step(self.next_step))
    }

    /// Consumes the next step and returns its absolute index.
    pub fn advance(&mut self) -> u64 {
        let s = self.next_step;
        self.next_step += 1;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_step_frames_at_120_bpm() {
        let mut t = Transport::new(120.0, 48_000.0);
        assert_eq!(Transport::samples_per_step(120.0, 48_000.0), 6000.0);
        t.start(1000);
        for k in 0..1000u64 {
            assert_eq!(t.next_step_frame(), Some(1000 + k * 6000));
            assert_eq!(t.advance(), k);
        }
    }

    #[test]
    fn no_drift_with_fractional_step_length() {
        // 44.1 kHz at 97 BPM: 6819.587... samples per step.
        let mut t = Transport::new(97.0, 44_100.0);
        t.start(0);
        let sps = Transport::samples_per_step(97.0, 44_100.0);
        let mut last = 0;
        for k in 0..100_000u64 {
            let f = t.next_step_frame().unwrap();
            assert!((f as f64 - k as f64 * sps).abs() <= 0.5);
            assert!(k == 0 || f > last);
            last = f;
            t.advance();
        }
    }

    #[test]
    fn tempo_change_keeps_playhead_continuous() {
        let mut t = Transport::new(120.0, 48_000.0);
        t.start(0);
        t.advance(); // step 0 fired at frame 0
        // Halfway to step 1, double the tempo.
        t.set_tempo(240.0, 48_000.0, 3000);
        assert!((t.position_at(3000) - 0.5).abs() < 1e-12);
        // The remaining half step now takes 1500 frames.
        assert_eq!(t.next_step_frame(), Some(4500));
        t.advance();
        assert_eq!(t.next_step_frame(), Some(7500));
    }

    #[test]
    fn stopped_transport_has_no_events() {
        let mut t = Transport::new(120.0, 48_000.0);
        assert_eq!(t.next_step_frame(), None);
        t.start(0);
        t.stop();
        assert_eq!(t.next_step_frame(), None);
    }
}
