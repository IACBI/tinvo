//! Bit crusher: the signal held for a number of frames (a lower sample
//! rate, without the filter that would stop it aliasing) and rounded to
//! fewer levels.

#[derive(Clone, Debug)]
pub struct Crusher {
    /// Steps per unit of amplitude.
    levels: f32,
    /// Frames each held value lasts, and how far into one it is.
    every: f32,
    phase: f32,
    held: [f32; 2],
}

impl Default for Crusher {
    fn default() -> Self {
        Self::new()
    }
}

impl Crusher {
    pub fn new() -> Self {
        let mut c = Self { levels: 0.0, every: 1.0, phase: 0.0, held: [0.0; 2] };
        c.set(8.0, 1.0);
        c
    }

    /// `bits` of resolution (a full-scale signal spans `2^bits` levels) and
    /// the frames each value is `held` for, both fractional.
    pub fn set(&mut self, bits: f32, held: f32) {
        self.levels = (bits.clamp(1.0, 24.0) - 1.0).exp2();
        self.every = held.max(1.0);
    }

    /// Forgets the value it holds.
    pub fn clear(&mut self) {
        self.phase = 0.0;
        self.held = [0.0; 2];
    }

    /// Crushes `buf` (interleaved stereo) in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        for f in buf.chunks_exact_mut(2) {
            if self.phase <= 0.0 {
                self.phase += self.every;
                self.held = [(f[0] * self.levels).round() / self.levels, (f[1] * self.levels).round() / self.levels];
            }
            self.phase -= 1.0;
            f[0] = self.held[0];
            f[1] = self.held[1];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_rounds_to_its_levels_and_holds_each_value() {
        let mut c = Crusher::new();
        c.set(3.0, 1.0);
        let mut buf = vec![0.1, -0.3, 0.2, 0.6, 0.9, -1.0];
        c.process(&mut buf);
        // Three bits: steps of a quarter.
        assert_eq!(buf, [0.0, -0.25, 0.25, 0.5, 1.0, -1.0]);
        c.set(16.0, 3.0);
        c.clear();
        let mut buf: Vec<f32> = (0..12).map(|i| i as f32 / 16.0).collect();
        c.process(&mut buf);
        let left: Vec<f32> = buf.iter().step_by(2).copied().collect();
        assert_eq!(left, [0.0, 0.0, 0.0, 0.375, 0.375, 0.375]);
    }
}
