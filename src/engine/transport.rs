//! Sample-accurate musical clock.
//!
//! Positions are counted in pulses, 24 to the quarter note, from the moment
//! playback starts. Every step division is a whole number of pulses, so the
//! steps of any pattern fall exactly on the clock. Frames are computed from
//! an origin rather than accumulated, so timing never drifts however long
//! playback runs. Tempo changes move the origin to the current position,
//! keeping the playhead continuous.
//!
//! The tempo is constant from the origin, or ramps: it changes by a fixed
//! number of BPM per 16th note. A ramp's frames are its exact integral, so
//! a step lands where the ramp puts it whatever the buffer size.

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

/// A pattern's time signature. It places the beat and bar lines on screen
/// and leaves playback alone; a pattern is as long as its steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Meter {
    beats: u8,
    unit: u8,
}

impl Default for Meter {
    fn default() -> Self {
        Meter { beats: 4, unit: 4 }
    }
}

impl Meter {
    pub const MAX_BEATS: u8 = 16;
    /// The note values a beat can be, from the longest.
    pub const UNITS: [u8; 4] = [2, 4, 8, 16];

    /// `beats` (1 to [`Self::MAX_BEATS`]) of `unit` notes (one of
    /// [`Self::UNITS`]) to the bar.
    pub fn new(beats: u8, unit: u8) -> Option<Self> {
        ((1..=Self::MAX_BEATS).contains(&beats) && Self::UNITS.contains(&unit)).then_some(Meter { beats, unit })
    }

    pub fn beats(self) -> u8 {
        self.beats
    }

    pub fn unit(self) -> u8 {
        self.unit
    }

    /// A beat's length in clock pulses.
    pub fn beat_pulses(self) -> u32 {
        PULSES_PER_BEAT * 4 / u32::from(self.unit)
    }

    /// A bar's length in clock pulses.
    pub fn bar_pulses(self) -> u32 {
        self.beat_pulses() * u32::from(self.beats)
    }

    /// The bar and beat (both from 1) that start at `pulse` of a pattern, if
    /// one does.
    pub fn beat_at(self, pulse: u32) -> Option<(u32, u32)> {
        (pulse % self.beat_pulses() == 0)
            .then(|| (pulse / self.bar_pulses() + 1, pulse % self.bar_pulses() / self.beat_pulses() + 1))
    }

    /// The name used on screen and in project files, such as `6/8`.
    pub fn name(self) -> String {
        format!("{}/{}", self.beats, self.unit)
    }

    pub fn from_name(name: &str) -> Option<Self> {
        let (b, u) = name.split_once('/')?;
        Self::new(b.parse().ok()?, u.parse().ok()?)
    }
}

/// The tempo at the start of song entry `i` (of `len`) and how it changes
/// across it, in BPM per 16th note. It holds from the last point at or
/// before the entry, or is `base` before any. With `ramp` it moves in a
/// straight line over the pulses from one point to the next, so a ramp
/// spanning several entries keeps one slope.
pub fn entry_tempo(
    i: usize,
    len: usize,
    ramp: bool,
    base: f32,
    point: impl Fn(usize) -> Option<f32>,
    pulses: impl Fn(usize) -> u64,
) -> (f64, f64) {
    let Some((a, va)) = (0..=i.min(len.saturating_sub(1))).rev().find_map(|j| point(j).map(|v| (j, v))) else {
        return (f64::from(base), 0.0);
    };
    if ramp {
        if let Some((b, vb)) = (i + 1..len).find_map(|j| point(j).map(|v| (j, v))) {
            let total: u64 = (a..b).map(&pulses).sum();
            let done: u64 = (a..i).map(&pulses).sum();
            if total > 0 {
                let per_pulse = f64::from(vb - va) / total as f64;
                return (f64::from(va) + per_pulse * done as f64, per_pulse * f64::from(PULSES_PER_16TH));
            }
        }
    }
    (f64::from(va), 0.0)
}

/// Frames that `sixteenths` take starting at `bpm` and changing by `slope`
/// BPM per 16th note: the integral of the time per 16th, `15 sr / bpm`.
pub fn span_frames(bpm: f64, slope: f64, sixteenths: f64, sample_rate: f32) -> f64 {
    let k = f64::from(sample_rate) * 15.0;
    if slope == 0.0 { sixteenths * k / bpm } else { k / slope * (slope * sixteenths / bpm).ln_1p() }
}

#[derive(Clone, Debug)]
pub struct Transport {
    playing: bool,
    sample_rate: f32,
    /// Frames in a 16th note at `bpm`, the tempo at the origin.
    samples_per_step: f64,
    bpm: f64,
    /// BPM gained per 16th note after the origin.
    slope: f64,
    origin_frame: u64,
    /// Where between `origin_frame` and the next frame the origin falls; a
    /// ramp's end is rarely on a whole frame.
    origin_frac: f64,
    /// Position at `origin_frame`, in 16th notes.
    origin: f64,
    /// Delay of a swung step as a fraction of a step, in `[0, 0.5]`.
    swing_delay: f64,
}

impl Transport {
    pub fn new(bpm: f32, sample_rate: f32) -> Self {
        Self {
            playing: false,
            sample_rate,
            samples_per_step: Self::samples_per_step(bpm, sample_rate),
            bpm: f64::from(bpm),
            slope: 0.0,
            origin_frame: 0,
            origin_frac: 0.0,
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

    /// Length of an unswung 16th note at `position`, in frames.
    pub fn step_frames(&self, position: f64) -> f64 {
        if self.slope == 0.0 {
            self.samples_per_step
        } else {
            f64::from(self.sample_rate) * 15.0 / self.bpm_at(position)
        }
    }

    /// The tempo at `position`.
    pub fn bpm_at(&self, position: f64) -> f64 {
        self.bpm + self.slope * (position - self.origin).max(0.0)
    }

    /// The tempo at the origin and how it changes, as for [`span_frames`].
    pub fn tempo(&self) -> (f64, f64) {
        (self.bpm, self.slope)
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Starts from pulse 0 at `now`.
    pub fn start(&mut self, now: u64) {
        self.playing = true;
        self.origin_frame = now;
        self.origin_frac = 0.0;
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
        self.move_origin(now, pending);
        self.sample_rate = sample_rate;
        self.samples_per_step = Self::samples_per_step(bpm, sample_rate);
        self.bpm = f64::from(bpm);
        self.slope = 0.0;
    }

    /// Like [`Self::set_tempo`], for a tempo `bpm` that then changes by
    /// `slope` BPM per 16th note.
    pub fn set_ramp(&mut self, bpm: f64, slope: f64, now: u64, pending: f64) {
        self.move_origin(now, pending);
        self.set_rate(bpm, slope);
    }

    /// Moves the origin to `position`, exactly where the current tempo puts
    /// it, and sets the tempo from there.
    pub fn set_segment(&mut self, position: f64, bpm: f64, slope: f64) {
        if self.playing {
            let at = self.origin_frac + self.offset(position - self.origin).max(0.0);
            let whole = at.floor();
            self.origin_frame += whole as u64;
            self.origin_frac = at - whole;
            self.origin = position;
        }
        self.set_rate(bpm, slope);
    }

    fn move_origin(&mut self, now: u64, pending: f64) {
        if self.playing {
            self.origin = self.position_at(now).min(pending);
            self.origin_frame = now;
            self.origin_frac = 0.0;
        }
    }

    fn set_rate(&mut self, bpm: f64, slope: f64) {
        self.samples_per_step = f64::from(self.sample_rate) * 60.0 / (bpm * 4.0);
        self.bpm = bpm;
        self.slope = slope;
    }

    /// Frames from the origin to `sixteenths` after it.
    fn offset(&self, sixteenths: f64) -> f64 {
        if self.slope == 0.0 {
            sixteenths * self.samples_per_step
        } else {
            span_frames(self.bpm, self.slope, sixteenths, self.sample_rate)
        }
    }

    /// Position at `frame` (which must not precede the origin), in 16th
    /// notes.
    pub fn position_at(&self, frame: u64) -> f64 {
        let frames = frame.saturating_sub(self.origin_frame) as f64 - self.origin_frac;
        if self.slope == 0.0 {
            self.origin + frames / self.samples_per_step
        } else {
            let k = f64::from(self.sample_rate) * 15.0;
            self.origin + self.bpm * (frames * self.slope / k).exp_m1() / self.slope
        }
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
        let offset = (self.origin_frac + self.offset(position - self.origin)).round();
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
    fn a_ramp_lands_each_step_on_its_integral() {
        // 120 to 240 BPM over 16 16ths at 48 kHz: the time to 16th d is
        // the integral of 15 sr / (120 + 7.5 x) from 0 to d.
        let mut t = Transport::new(120.0, 48_000.0);
        t.start(0);
        t.set_segment(0.0, 120.0, 7.5);
        let k = 48_000.0 * 15.0;
        for d in 0..=16u64 {
            let exact = k / 7.5 * (1.0 + 7.5 * d as f64 / 120.0).ln();
            assert_eq!(t.step_frame(6 * d, D16), exact.round() as u64, "step {d}");
            let pos = t.position_at(exact.round() as u64);
            assert!((pos - d as f64).abs() < 1e-3, "{d} {pos}");
        }
        // Twice the tempo by the end: the last step is half as long as the
        // first.
        let first = t.step_frame(6, D16) - t.step_frame(0, D16);
        let last = t.step_frame(96, D16) - t.step_frame(90, D16);
        assert!((first as f64 / last as f64 - 2.0).abs() < 0.1, "{first} {last}");
        assert!((t.bpm_at(16.0) - 240.0).abs() < 1e-9);
        // Held after the ramp, from its exact end: no frame is lost.
        let end = k / 7.5 * 2f64.ln();
        t.set_segment(16.0, 240.0, 0.0);
        assert_eq!(t.step_frame(96, D16), end.round() as u64);
        assert_eq!(t.step_frame(96 + 6 * 100, D16), (end + 100.0 * 3000.0).round() as u64);
        // A tempo the slope barely moves is the straight line.
        assert!((span_frames(120.0, 1e-12, 16.0, 48_000.0) - 96_000.0).abs() < 1e-3);
        assert_eq!(span_frames(120.0, 0.0, 16.0, 48_000.0), 96_000.0);
    }

    #[test]
    fn entry_tempo_holds_and_ramps_between_points() {
        // Entries of 96 pulses; points at entries 1 (100) and 3 (140).
        let points = [None, Some(100.0), None, Some(140.0), None];
        let point = |j: usize| points[j];
        let pulses = |_| 96;
        let hold: Vec<_> = (0..5).map(|i| entry_tempo(i, 5, false, 120.0, point, pulses)).collect();
        assert_eq!(hold, [(120.0, 0.0), (100.0, 0.0), (100.0, 0.0), (140.0, 0.0), (140.0, 0.0)]);
        let ramp: Vec<_> = (0..5).map(|i| entry_tempo(i, 5, true, 120.0, point, pulses)).collect();
        // 40 BPM over 192 pulses (32 16ths): 1.25 BPM per 16th.
        assert_eq!(ramp, [(120.0, 0.0), (100.0, 1.25), (120.0, 1.25), (140.0, 0.0), (140.0, 0.0)]);
        // Unequal entries share the slope by their pulses.
        let lens = [96, 48, 144, 96, 96];
        let (t2, s2) = entry_tempo(2, 5, true, 120.0, point, |j| lens[j]);
        assert_eq!((t2, s2), (100.0 + 40.0 * 48.0 / 192.0, 40.0 / 192.0 * 6.0));
        assert_eq!(entry_tempo(0, 0, true, 90.0, |_| None, pulses), (90.0, 0.0));
    }

    #[test]
    fn meters_place_beats_and_bars() {
        let four = Meter::default();
        assert_eq!(four.name(), "4/4");
        assert_eq!((four.beat_pulses(), four.bar_pulses()), (24, 96));
        let six = Meter::from_name("6/8").unwrap();
        assert_eq!((six.beats(), six.unit(), six.beat_pulses(), six.bar_pulses()), (6, 8, 12, 72));
        // 16ths in 6/8: a beat every two steps, a bar every twelve.
        let marks: Vec<_> = (0..14).filter_map(|i| six.beat_at(i * 6).map(|b| (i, b))).collect();
        assert_eq!(
            marks,
            [(0, (1, 1)), (2, (1, 2)), (4, (1, 3)), (6, (1, 4)), (8, (1, 5)), (10, (1, 6)), (12, (2, 1))]
        );
        // Triplet eighths in 4/4: three to the beat.
        assert_eq!(four.beat_at(3 * 8), Some((1, 2)));
        assert_eq!(four.beat_at(8), None);
        for bad in ["0/4", "17/4", "4/3", "4/32", "4", "a/4", "4/4/4", ""] {
            assert_eq!(Meter::from_name(bad), None, "{bad}");
        }
        assert_eq!(Meter::from_name("16/16").map(Meter::name).as_deref(), Some("16/16"));
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
