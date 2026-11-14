//! Sample-accurate musical clock.
//!
//! Positions are counted in pulses, 24 to the quarter note, from the moment
//! playback starts. Every step division is a whole number of pulses, so the
//! steps of any pattern fall exactly on the clock. Frames are computed from
//! an origin rather than accumulated, so timing never drifts however long
//! playback runs. Tempo changes move the origin to the current position,
//! keeping the playhead continuous.

/// Clock resolution: pulses per quarter note.
pub const PULSES_PER_BEAT: u32 = 24;
/// Pulses in a 16th note, the unit frame positions are computed in.
const PULSES_PER_16TH: u32 = PULSES_PER_BEAT / 4;

/// How long a pattern's steps are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Division {
    Eighth,
    EighthTriplet,
    #[default]
    Sixteenth,
    SixteenthTriplet,
    ThirtySecond,
}

impl Division {
    /// From the longest step to the shortest.
    pub const ALL: [Division; 5] = [
        Division::Eighth,
        Division::EighthTriplet,
        Division::Sixteenth,
        Division::SixteenthTriplet,
        Division::ThirtySecond,
    ];

    /// A step's length in clock pulses.
    pub const fn pulses(self) -> u32 {
        match self {
            Division::Eighth => 12,
            Division::EighthTriplet => 8,
            Division::Sixteenth => 6,
            Division::SixteenthTriplet => 4,
            Division::ThirtySecond => 3,
        }
    }

    /// Steps in a quarter note.
    pub const fn steps_per_beat(self) -> u32 {
        PULSES_PER_BEAT / self.pulses()
    }

    /// The name used on screen and in project files.
    pub const fn name(self) -> &'static str {
        match self {
            Division::Eighth => "1/8",
            Division::EighthTriplet => "1/8T",
            Division::Sixteenth => "1/16",
            Division::SixteenthTriplet => "1/16T",
            Division::ThirtySecond => "1/32",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.name() == name)
    }

    /// Swing delays the second step of each pair; triplet steps already
    /// have their own feel and are never swung.
    pub const fn swings(self) -> bool {
        !matches!(self, Division::EighthTriplet | Division::SixteenthTriplet)
    }

    /// A step's length in 16th notes.
    pub fn sixteenths(self) -> f64 {
        f64::from(self.pulses()) / f64::from(PULSES_PER_16TH)
    }
}

#[derive(Clone, Debug)]
pub struct Transport {
    playing: bool,
    samples_per_step: f64,
    origin_frame: u64,
    /// Position at `origin_frame`, in 16th notes.
    origin: f64,
    /// Delay of a swung step as a fraction of a step, in `[0, 0.5]`.
    swing_delay: f64,
}

impl Transport {
    pub fn new(bpm: f32, sample_rate: f32) -> Self {
        Self {
            playing: false,
            samples_per_step: Self::samples_per_step(bpm, sample_rate),
            origin_frame: 0,
            origin: 0.0,
            swing_delay: 0.0,
        }
    }

    /// Sets swing in percent (50 = straight, 75 = maximum). The second step
    /// of each pair lands at `percent` of the pair, i.e. it is delayed by
    /// `(percent / 50 - 1)` of a step.
    pub fn set_swing(&mut self, percent: f32) {
        let p = f64::from(percent.clamp(super::MIN_SWING, super::MAX_SWING));
        self.swing_delay = p / 50.0 - 1.0;
    }

    /// Frames in a 16th note.
    pub fn samples_per_step(bpm: f32, sample_rate: f32) -> f64 {
        f64::from(sample_rate) * 60.0 / (f64::from(bpm) * 4.0)
    }

    /// Length of an unswung 16th note at the current tempo, in frames.
    pub fn step_frames(&self) -> f64 {
        self.samples_per_step
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Starts from pulse 0 at `now`.
    pub fn start(&mut self, now: u64) {
        self.playing = true;
        self.origin_frame = now;
        self.origin = 0.0;
    }

    pub fn stop(&mut self) {
        self.playing = false;
    }

    /// Changes the tempo at `now`. `pending` is the position (in 16th notes,
    /// swing included) of the earliest event not yet fired: swing delays a
    /// step past its straight position, so the playhead may be anywhere up
    /// to it, but never past it.
    pub fn set_tempo(&mut self, bpm: f32, sample_rate: f32, now: u64, pending: f64) {
        if self.playing {
            self.origin = self.position_at(now).min(pending);
            self.origin_frame = now;
        }
        self.samples_per_step = Self::samples_per_step(bpm, sample_rate);
    }

    /// Position at `frame` (which must not precede the origin), in 16th
    /// notes.
    pub fn position_at(&self, frame: u64) -> f64 {
        self.origin + frame.saturating_sub(self.origin_frame) as f64 / self.samples_per_step
    }

    /// Where a step of `division` starting at `pulse` sounds, in 16th notes:
    /// its straight position, plus the swing delay when it is the second of
    /// a pair.
    pub fn step_position(&self, pulse: u64, division: Division) -> f64 {
        let p = u64::from(division.pulses());
        let swing =
            if division.swings() && pulse % (2 * p) == p { self.swing_delay * division.sixteenths() } else { 0.0 };
        pulse as f64 / f64::from(PULSES_PER_16TH) + swing
    }

    /// The frame of a position in 16th notes.
    pub fn frame_at(&self, position: f64) -> u64 {
        let offset = ((position - self.origin) * self.samples_per_step).round();
        self.origin_frame + offset.max(0.0) as u64
    }

    /// The frame at which a step of `division` starting at `pulse` sounds.
    pub fn step_frame(&self, pulse: u64, division: Division) -> u64 {
        self.frame_at(self.step_position(pulse, division))
    }

    /// The frame of an unswung `pulse`.
    pub fn pulse_frame(&self, pulse: u64) -> u64 {
        self.frame_at(pulse as f64 / f64::from(PULSES_PER_16TH))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D16: Division = Division::Sixteenth;

    #[test]
    fn every_division_is_a_whole_number_of_pulses() {
        for d in Division::ALL {
            assert_eq!(PULSES_PER_BEAT % d.pulses(), 0, "{}", d.name());
            assert_eq!(Division::from_name(d.name()), Some(d));
        }
        assert_eq!(Division::default(), D16);
        assert_eq!(Division::from_name("1/4"), None);
    }

    #[test]
    fn exact_step_frames_at_120_bpm() {
        let mut t = Transport::new(120.0, 48_000.0);
        assert_eq!(Transport::samples_per_step(120.0, 48_000.0), 6000.0);
        t.start(1000);
        for k in 0..1000u64 {
            assert_eq!(t.step_frame(6 * k, D16), 1000 + k * 6000);
        }
    }

    #[test]
    fn each_division_is_its_fraction_of_a_beat() {
        // 120 BPM at 48 kHz: a quarter note is 24 000 frames.
        let mut t = Transport::new(120.0, 48_000.0);
        t.start(0);
        for (d, frames) in [
            (Division::Eighth, 12_000),
            (Division::EighthTriplet, 8000),
            (Division::Sixteenth, 6000),
            (Division::SixteenthTriplet, 4000),
            (Division::ThirtySecond, 3000),
        ] {
            let p = u64::from(d.pulses());
            for k in 0..100 {
                assert_eq!(t.step_frame(k * p, d), k * frames, "{} step {k}", d.name());
            }
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
            let f = t.step_frame(6 * k, D16);
            assert!((f as f64 - k as f64 * sps).abs() <= 0.5);
            assert!(k == 0 || f > last);
            last = f;
        }
    }

    #[test]
    fn a_tempo_change_while_a_swung_step_waits_keeps_the_way_already_gone() {
        // 120 BPM, 75 % swing: step 1 is due at 9000. At 7500 the playhead
        // is three quarters of the way there; at twice the tempo the last
        // quarter (1500 frames before) takes 750.
        let mut t = Transport::new(120.0, 48_000.0);
        t.set_swing(75.0);
        t.start(0);
        assert_eq!(t.step_frame(6, D16), 9000);
        t.set_tempo(240.0, 48_000.0, 7500, t.step_position(6, D16));
        assert_eq!(t.step_frame(6, D16), 8250);
        assert_eq!(t.step_frame(12, D16), 8250 + 3000 - 1500, "step 2 is straight again");
    }

    #[test]
    fn tempo_change_keeps_playhead_continuous() {
        let mut t = Transport::new(120.0, 48_000.0);
        t.start(0);
        // Halfway to step 1, double the tempo.
        t.set_tempo(240.0, 48_000.0, 3000, t.step_position(6, D16));
        assert!((t.position_at(3000) - 0.5).abs() < 1e-12);
        // The remaining half step now takes 1500 frames.
        assert_eq!(t.step_frame(6, D16), 4500);
        assert_eq!(t.step_frame(12, D16), 7500);
    }

    #[test]
    fn swing_delays_the_second_step_of_each_pair() {
        let mut t = Transport::new(120.0, 48_000.0);
        t.set_swing(75.0); // odd steps half a step late
        t.start(0);
        let frames: Vec<u64> = (0..6).map(|k| t.step_frame(6 * k, D16)).collect();
        assert_eq!(frames, vec![0, 9000, 12_000, 21_000, 24_000, 33_000]);
        // In eighths the second eighth of each beat; 32nds swing in pairs too.
        assert_eq!(t.step_frame(12, Division::Eighth), 18_000);
        assert_eq!(t.step_frame(24, Division::Eighth), 24_000);
        assert_eq!(t.step_frame(3, Division::ThirtySecond), 4500);
        // Triplets are never swung.
        assert_eq!(t.step_frame(8, Division::EighthTriplet), 8000);
        assert_eq!(t.step_frame(4, Division::SixteenthTriplet), 4000);
        // A step off its pair grid (after an odd-length pattern) is straight.
        assert_eq!(t.step_frame(18, Division::Eighth), 18_000);
        // Triplet feel: the off-beat sits at 2/3 of the 8th note.
        let mut t = Transport::new(120.0, 48_000.0);
        t.set_swing(200.0 / 3.0);
        t.start(0);
        assert_eq!(t.step_frame(6, D16), 8000);
        t.set_swing(10.0); // clamped to straight
        assert_eq!(t.step_frame(6, D16), 6000);
    }
}
