//! A sampler's samples laid over the keyboard: key zones, velocity layers,
//! round robin (in turn or at random), release samples, and each zone's
//! own way of looping.
//!
//! A [`SampleMap`] is built on the UI thread and reaches the engine in an
//! `Arc`; the sampler only reads it. A synth track's file travels the same
//! way, as a map holding the wavetable built from it.

use crate::dsp::db_to_gain;
use crate::dsp::wavetable::Wavetable;
use crate::sample::Sample;
use std::sync::Arc;

/// The most zones a sampler track holds.
pub const MAX_ZONES: usize = 1024;
/// The most zones a [`SampleMap`] holds: a track's zones and its sample.
pub const MAP_ZONES: usize = MAX_ZONES + 1;
/// The longest round-robin cycle.
pub const MAX_SEQ: u8 = 100;
/// How far a zone's own tuning reaches, in cents either way.
pub const MAX_TUNE: f32 = 4800.0;
/// A zone's own gain, in dB.
pub const MIN_GAIN_DB: f32 = -96.0;
pub const MAX_GAIN_DB: f32 = 24.0;

/// When a zone sounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// When a key goes down.
    Attack,
    /// When a key comes up, at the velocity it went down with.
    Release,
}

/// How a zone plays through its frames. Every mode but `Track` leaves the
/// track's mode and loop settings aside. Loop points are frames of the
/// file, from the loop's start up to its end (exclusive); `None` takes the
/// loop the file itself gives, if it gives one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// As the track's mode and loop settings say.
    Track,
    /// Once to its end, or until its key comes up.
    NoLoop,
    /// Once to its end, whatever its key does.
    OneShot,
    /// Round its loop for as long as it sounds, its release too.
    Continuous(Option<(u32, u32)>),
    /// Round its loop while its key is down, then on to its end.
    Sustain(Option<(u32, u32)>),
}

impl Mode {
    /// Its loop points, for the modes that loop.
    pub fn loop_points(self) -> Option<Option<(u32, u32)>> {
        match self {
            Mode::Continuous(p) | Mode::Sustain(p) => Some(p),
            _ => None,
        }
    }
}

/// Where a zone sits and how it plays, apart from its sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// The lowest and highest key it answers, inclusive.
    pub keys: (u8, u8),
    /// The lowest and highest velocity it answers, inclusive.
    pub vels: (u8, u8),
    /// The key that plays the sample at its own pitch; `None` follows the
    /// track's root note parameter.
    pub root: Option<u8>,
    /// Round robin: it plays the `.0`th (1-based) of every `.1` notes that
    /// reach it.
    pub seq: (u8, u8),
    /// Round robin at random: it plays a note whose random number, drawn
    /// in `[0, 1)` for each note, falls in `[.0, .1)` (`.1` of 1 takes
    /// every number from `.0` up).
    pub rand: (f32, f32),
    pub trigger: Trigger,
    /// Its own tuning, in cents.
    pub tune: f32,
    pub gain_db: f32,
    /// The frames of the sample it plays: from `start` up to `end`
    /// (exclusive), `None` being the sample's end.
    pub start: u32,
    pub end: Option<u32>,
    pub mode: Mode,
}

impl Placement {
    /// The whole sample over the whole keyboard, rooted at the track's root
    /// note: what a sampler track's single sample is.
    pub const WHOLE: Placement = Placement {
        keys: (0, 127),
        vels: (0, 127),
        root: None,
        seq: (1, 1),
        rand: (0.0, 1.0),
        trigger: Trigger::Attack,
        tune: 0.0,
        gain_db: 0.0,
        start: 0,
        end: None,
        mode: Mode::Track,
    };

    /// Why the placement cannot be played, if it cannot.
    pub fn check(&self) -> Result<(), String> {
        let ((lk, hk), (lv, hv), (pos, len)) = (self.keys, self.vels, self.seq);
        if lk > hk || hk > 127 {
            return Err("keys run from 0 to 127, low to high".into());
        }
        if lv > hv || hv > 127 {
            return Err("velocities run from 0 to 127, low to high".into());
        }
        if self.root.is_some_and(|r| r > 127) {
            return Err("a root key is 0 to 127".into());
        }
        if !(1..=MAX_SEQ).contains(&len) || !(1..=len).contains(&pos) {
            return Err(format!("a round-robin position is 1 to its cycle, of 1 to {MAX_SEQ}"));
        }
        if !(-MAX_TUNE..=MAX_TUNE).contains(&self.tune) {
            return Err(format!("a zone's tuning is {} to {MAX_TUNE} cents", -MAX_TUNE));
        }
        if !(MIN_GAIN_DB..=MAX_GAIN_DB).contains(&self.gain_db) {
            return Err(format!("a zone's gain is {MIN_GAIN_DB} to {MAX_GAIN_DB} dB"));
        }
        if self.end.is_some_and(|e| e <= self.start) {
            return Err("a zone ends after it starts".into());
        }
        let (lo, hi) = self.rand;
        if !(0.0 <= lo && lo < hi && hi <= 1.0) {
            return Err("a random range runs from 0 to 1, low below high".into());
        }
        if self.mode.loop_points().flatten().is_some_and(|(s, e)| e <= s) {
            return Err("a loop ends after it starts".into());
        }
        Ok(())
    }

    /// Whether a note whose random number is `r` reaches it.
    pub fn draws(&self, r: f32) -> bool {
        self.rand.0 <= r && (r < self.rand.1 || self.rand.1 >= 1.0)
    }

    /// Whether a note of `key` at `vel` reaches it.
    pub fn covers(&self, key: u8, vel: u8) -> bool {
        (self.keys.0..=self.keys.1).contains(&key) && (self.vels.0..=self.vels.1).contains(&vel)
    }
}

/// A sample and where it plays.
#[derive(Debug, PartialEq)]
pub struct Zone {
    pub place: Placement,
    pub sample: Arc<Sample>,
    /// `place.gain_db` as a factor.
    pub gain: f32,
}

/// The zones a sampler plays, in order.
#[derive(Debug, PartialEq)]
pub struct SampleMap {
    zones: Vec<Zone>,
    release: bool,
    /// The zones answering key `k`, in order, are `by_key[starts[k]..starts[k + 1]]`,
    /// so a note looks only at those.
    starts: [u32; 129],
    by_key: Vec<u16>,
    /// A synth track's wavetable, in place of zones.
    table: Option<Wavetable>,
}

impl SampleMap {
    /// One sample over the whole keyboard ([`Placement::WHOLE`]).
    pub fn single(sample: Arc<Sample>) -> Self {
        Self::new([(Placement::WHOLE, sample)])
    }

    /// The zones given, up to [`MAP_ZONES`]; placements that do not
    /// [`Placement::check`] are left out.
    pub fn new(zones: impl IntoIterator<Item = (Placement, Arc<Sample>)>) -> Self {
        let zones: Vec<Zone> = zones
            .into_iter()
            .filter(|(p, _)| p.check().is_ok())
            .take(MAP_ZONES)
            .map(|(place, sample)| Zone { place, sample, gain: db_to_gain(place.gain_db) })
            .collect();
        let release = zones.iter().any(|z| z.place.trigger == Trigger::Release);
        let (mut starts, mut by_key) = ([0u32; 129], Vec::new());
        for key in 0..=127u8 {
            let answering = zones.iter().enumerate().filter(|(_, z)| (z.place.keys.0..=z.place.keys.1).contains(&key));
            by_key.extend(answering.map(|(i, _)| i as u16));
            starts[usize::from(key) + 1] = by_key.len() as u32;
        }
        Self { zones, release, starts, by_key, table: None }
    }

    /// A synth track's wavetable, with no zones.
    pub fn wavetable(table: Wavetable) -> Self {
        Self { table: Some(table), ..Self::new([]) }
    }

    pub fn table(&self) -> Option<&Wavetable> {
        self.table.as_ref()
    }

    pub fn zones(&self) -> &[Zone] {
        &self.zones
    }

    /// The indices of the zones whose key range holds `key`, in order.
    pub fn on_key(&self, key: u8) -> &[u16] {
        let k = usize::from(key.min(127));
        &self.by_key[self.starts[k] as usize..self.starts[k + 1] as usize]
    }

    pub fn is_empty(&self) -> bool {
        self.zones.is_empty() && self.table.is_none()
    }

    /// Whether a zone sounds when a key comes up.
    pub fn has_release(&self) -> bool {
        self.release
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Arc<Sample> {
        Arc::new(Sample::from_mono(&[0.5; 100], 48_000).unwrap())
    }

    #[test]
    fn placements_are_checked() {
        let ok = Placement { keys: (36, 48), vels: (1, 64), root: Some(40), seq: (2, 3), ..Placement::WHOLE };
        assert_eq!(ok.check(), Ok(()));
        assert!(ok.covers(36, 1) && ok.covers(48, 64));
        assert!(!ok.covers(35, 10) && !ok.covers(49, 10) && !ok.covers(40, 0) && !ok.covers(40, 65));
        let third = Placement { rand: (1.0 / 3.0, 2.0 / 3.0), ..ok };
        assert!(third.draws(0.5) && !third.draws(0.2) && !third.draws(2.0 / 3.0));
        let last = Placement { rand: (2.0 / 3.0, 1.0), ..ok };
        assert!(last.draws(0.999_999) && last.draws(1.0) && !last.draws(0.5));
        assert_eq!(Placement { mode: Mode::Continuous(Some((0, 10))), ..ok }.check(), Ok(()));
        let bad = [
            Placement { keys: (50, 49), ..ok },
            Placement { keys: (0, 128), ..ok },
            Placement { vels: (65, 64), ..ok },
            Placement { root: Some(128), ..ok },
            Placement { seq: (0, 3), ..ok },
            Placement { seq: (4, 3), ..ok },
            Placement { seq: (1, MAX_SEQ + 1), ..ok },
            Placement { tune: MAX_TUNE + 1.0, ..ok },
            Placement { tune: f32::NAN, ..ok },
            Placement { gain_db: MAX_GAIN_DB + 1.0, ..ok },
            Placement { start: 10, end: Some(10), ..ok },
            Placement { rand: (0.5, 0.5), ..ok },
            Placement { rand: (-0.1, 0.5), ..ok },
            Placement { rand: (0.5, 1.5), ..ok },
            Placement { rand: (f32::NAN, 1.0), ..ok },
            Placement { mode: Mode::Sustain(Some((100, 100))), ..ok },
        ];
        for p in bad {
            assert!(p.check().is_err(), "{p:?}");
        }
    }

    #[test]
    fn a_map_keeps_the_zones_that_can_play_up_to_the_limit() {
        let bad = Placement { keys: (2, 1), ..Placement::WHOLE };
        let rel = Placement { trigger: Trigger::Release, ..Placement::WHOLE };
        let m = SampleMap::new([(bad, sample()), (Placement::WHOLE, sample())]);
        assert_eq!(m.zones().len(), 1);
        assert!(!m.has_release());
        assert!(SampleMap::new([(rel, sample())]).has_release());
        let s = sample();
        let many = SampleMap::new((0..MAX_ZONES + 5).map(|_| (Placement::WHOLE, s.clone())));
        assert_eq!(many.zones().len(), MAP_ZONES, "a track's zones and its sample");
        assert_eq!(many.on_key(127).len(), MAP_ZONES);
        // Each key knows the zones that answer it.
        let low = Placement { keys: (0, 59), ..Placement::WHOLE };
        let high = Placement { keys: (60, 127), ..Placement::WHOLE };
        let m = SampleMap::new([(low, s.clone()), (Placement::WHOLE, s.clone()), (high, s.clone())]);
        assert_eq!(
            (m.on_key(0), m.on_key(59), m.on_key(60), m.on_key(127)),
            (&[0, 1][..], &[0, 1][..], &[1, 2][..], &[1, 2][..])
        );
        assert_eq!(SampleMap::single(s).zones()[0].gain, 1.0);
        // A synth's table, with no zones, is something to play.
        let t = crate::dsp::wavetable::Wavetable::from_audio(&[0.0, 1.0, 0.0, -1.0]).unwrap();
        let m = SampleMap::wavetable(t);
        assert!(!m.is_empty() && m.zones().is_empty() && m.table().is_some());
        assert!(SampleMap::new([]).is_empty() && SampleMap::new([]).table().is_none());
    }
}
