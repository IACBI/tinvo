//! Parameter smoothing to avoid zipper noise and clicks.

/// One-pole low-pass smoother for control values (gains, pans).
#[derive(Clone, Copy, Debug)]
pub struct Smoother {
    current: f32,
    target: f32,
    coef: f32,
}

impl Smoother {
    /// `time` is the time constant in seconds (63 % of a step is covered in
    /// that time, 99 % in about 4.6× that time).
    pub fn new(value: f32, time: f32, sample_rate: f32) -> Self {
        Self {
            current: value,
            target: value,
            coef: (-1.0 / (time.max(1.0e-5) * sample_rate)).exp(),
        }
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Jumps to `value` immediately.
    pub fn snap(&mut self, value: f32) {
        self.current = value;
        self.target = value;
    }

    #[inline]
    pub fn tick(&mut self) -> f32 {
        if self.current != self.target {
            let next = self.target + (self.current - self.target) * self.coef;
            // Near the target, f32 rounding can make the step round back to
            // the same value forever; finish exactly instead of stalling.
            self.current = if next == self.current || (next - self.target).abs() < 1.0e-7 {
                self.target
            } else {
                next
            };
        }
        self.current
    }

    pub fn current(&self) -> f32 {
        self.current
    }

    pub fn target(&self) -> f32 {
        self.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_monotonically_and_exactly() {
        let mut s = Smoother::new(0.0, 0.005, 48_000.0);
        s.set_target(1.0);
        let mut prev = 0.0;
        for _ in 0..48_000 {
            let v = s.tick();
            assert!(v >= prev && v <= 1.0);
            prev = v;
        }
        assert_eq!(s.current(), 1.0);
    }

    #[test]
    fn time_constant() {
        let mut s = Smoother::new(0.0, 0.01, 48_000.0);
        s.set_target(1.0);
        for _ in 0..480 {
            s.tick();
        }
        assert!((s.current() - (1.0 - (-1.0f32).exp())).abs() < 1e-3);
    }
}
