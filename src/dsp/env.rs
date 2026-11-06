//! ADSR envelope generator.

use super::{decay_coef, flush_denormal};

/// Envelope settings. Times are in seconds, `sustain` is a level in `[0, 1]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdsrParams {
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Idle,
    Attack,
    /// Exponential approach to the sustain level; also tracks sustain changes.
    Decay,
    Release,
}

/// Shortest allowed segment, long enough to avoid audible clicks.
const MIN_TIME: f32 = 0.0005;
/// Release ends below this level (-100 dB).
const END_LEVEL: f32 = 1.0e-5;

/// Linear-attack, exponential-decay/release envelope.
///
/// Decay and release times are the time to fall by 60 dB. Re-triggering
/// starts the attack from the current level, so retriggers never jump.
#[derive(Clone, Copy, Debug)]
pub struct Adsr {
    stage: Stage,
    level: f32,
    attack_step: f32,
    decay_coef: f32,
    release_coef: f32,
    sustain: f32,
}

impl Default for Adsr {
    fn default() -> Self {
        Self::new()
    }
}

impl Adsr {
    pub const fn new() -> Self {
        Self { stage: Stage::Idle, level: 0.0, attack_step: 1.0, decay_coef: 0.0, release_coef: 0.0, sustain: 1.0 }
    }

    pub fn set_params(&mut self, p: &AdsrParams, sample_rate: f32) {
        self.attack_step = 1.0 / (p.attack.max(MIN_TIME) * sample_rate);
        self.decay_coef = decay_coef(p.decay.max(MIN_TIME), sample_rate);
        self.release_coef = decay_coef(p.release.max(MIN_TIME), sample_rate);
        self.sustain = p.sustain.clamp(0.0, 1.0);
    }

    pub fn gate_on(&mut self) {
        self.stage = Stage::Attack;
    }

    pub fn gate_off(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }

    /// Silences the envelope immediately.
    pub fn reset(&mut self) {
        self.stage = Stage::Idle;
        self.level = 0.0;
    }

    #[inline]
    pub fn tick(&mut self) -> f32 {
        match self.stage {
            Stage::Idle => {}
            Stage::Attack => {
                self.level += self.attack_step;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level = self.sustain + flush_denormal((self.level - self.sustain) * self.decay_coef);
            }
            Stage::Release => {
                self.level *= self.release_coef;
                if self.level < END_LEVEL {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level
    }

    pub fn is_active(&self) -> bool {
        self.stage != Stage::Idle
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn level(&self) -> f32 {
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn env(a: f32, d: f32, s: f32, r: f32) -> Adsr {
        let mut e = Adsr::new();
        e.set_params(&AdsrParams { attack: a, decay: d, sustain: s, release: r }, SR);
        e
    }

    #[test]
    fn attack_takes_the_requested_time() {
        let mut e = env(0.01, 0.1, 0.5, 0.1);
        e.gate_on();
        let n = (0.01 * SR) as usize;
        for _ in 0..n - 2 {
            e.tick();
        }
        assert_eq!(e.stage(), Stage::Attack);
        for _ in 0..3 {
            e.tick();
        }
        assert_eq!(e.stage(), Stage::Decay);
    }

    #[test]
    fn decay_settles_on_sustain() {
        let mut e = env(0.001, 0.1, 0.5, 0.1);
        e.gate_on();
        for _ in 0..(0.001 * SR) as usize + 1 + (0.1 * SR) as usize {
            e.tick();
        }
        // After the decay time the distance to sustain is down by 60 dB.
        assert!((e.level() - 0.5).abs() < 0.5 * 1.1e-3, "{}", e.level());
    }

    #[test]
    fn release_ends_idle_in_bounded_time() {
        let mut e = env(0.001, 0.05, 1.0, 0.2);
        e.gate_on();
        for _ in 0..1000 {
            e.tick();
        }
        e.gate_off();
        // -100 dB end level = 5/3 of the -60 dB release time.
        let bound = (0.2 * SR * 5.0 / 3.0) as usize + 2;
        let mut n = 0;
        while e.is_active() {
            e.tick();
            n += 1;
            assert!(n <= bound, "release did not finish");
        }
        assert_eq!(e.level(), 0.0);
        assert!(n + 100 > bound, "release finished too early: {n}");
    }

    #[test]
    fn retrigger_continues_from_current_level() {
        let mut e = env(0.01, 0.1, 0.8, 0.5);
        e.gate_on();
        for _ in 0..5000 {
            e.tick();
        }
        e.gate_off();
        for _ in 0..2000 {
            e.tick();
        }
        let before = e.level();
        e.gate_on();
        let after = e.tick();
        assert!(after > before && after - before < 0.01, "{before} -> {after}");
    }

    #[test]
    fn sustain_change_is_smooth() {
        let mut e = env(0.001, 0.05, 1.0, 0.1);
        e.gate_on();
        for _ in 0..10_000 {
            e.tick();
        }
        e.set_params(&AdsrParams { attack: 0.001, decay: 0.05, sustain: 0.2, release: 0.1 }, SR);
        let mut prev = e.level();
        for _ in 0..10_000 {
            let l = e.tick();
            assert!((l - prev).abs() < 0.01);
            prev = l;
        }
        assert!((prev - 0.2).abs() < 1e-3);
    }
}
