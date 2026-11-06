//! Instruments: sound generators driven by note events.
//!
//! Every instrument is a fixed-size value that never allocates, so the
//! engine can create, replace and move instruments on the audio thread. The
//! sampler holds its audio behind an `Arc` it neither allocates nor frees.

mod drums;
mod sampler;
mod synth;

use crate::sample::Sample;
pub use drums::Drum;
pub use sampler::{SAMPLER_VOICES, Sampler};
use std::sync::Arc;
pub use synth::{PolySynth, SYNTH_VOICES};

/// The kinds of instrument a track can host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InstrumentKind {
    Synth,
    Sampler,
    Kick,
    Snare,
    HatClosed,
    HatOpen,
    Clap,
}

impl InstrumentKind {
    pub const ALL: [InstrumentKind; 7] = [
        InstrumentKind::Synth,
        InstrumentKind::Kick,
        InstrumentKind::Snare,
        InstrumentKind::HatClosed,
        InstrumentKind::HatOpen,
        InstrumentKind::Clap,
        InstrumentKind::Sampler,
    ];

    /// Stable identifier used in project files.
    pub fn id(self) -> &'static str {
        match self {
            InstrumentKind::Synth => "synth",
            InstrumentKind::Sampler => "sampler",
            InstrumentKind::Kick => "kick",
            InstrumentKind::Snare => "snare",
            InstrumentKind::HatClosed => "hat",
            InstrumentKind::HatOpen => "openhat",
            InstrumentKind::Clap => "clap",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }

    /// Human-readable name.
    pub fn label(self) -> &'static str {
        match self {
            InstrumentKind::Synth => "Synth",
            InstrumentKind::Sampler => "Sampler",
            InstrumentKind::Kick => "Kick",
            InstrumentKind::Snare => "Snare",
            InstrumentKind::HatClosed => "Hi-hat",
            InstrumentKind::HatOpen => "Open hat",
            InstrumentKind::Clap => "Clap",
        }
    }

    /// Parameter descriptions, in parameter-index order.
    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            InstrumentKind::Synth => &synth::PARAMS,
            InstrumentKind::Sampler => &sampler::PARAMS,
            k => drums::params(k),
        }
    }

    /// Note placed by default when the user adds a step.
    pub fn default_note(self) -> u8 {
        match self {
            InstrumentKind::Synth => 36,
            _ => 60,
        }
    }

    pub fn is_drum(self) -> bool {
        !matches!(self, InstrumentKind::Synth | InstrumentKind::Sampler)
    }
}

/// How a parameter's value moves when the user nudges it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamScale {
    /// Additive steps of `step`.
    Linear { step: f32 },
    /// Multiplicative steps of `ratio`.
    Log { ratio: f32 },
    /// Discrete choice; the value is an index into the names.
    Stepped(&'static [&'static str]),
}

/// Display unit of a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    None,
    Hz,
    Seconds,
    Semitones,
    Cents,
    Octaves,
    Decibels,
    Pan,
    /// A MIDI note number, shown as a note name.
    Note,
    /// A fraction in `[0, 1]`, shown as a percentage.
    Percent,
}

/// Scientific pitch notation: 60 = C4.
pub fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[usize::from(note % 12)], i32::from(note / 12) - 1)
}

/// Static description of one instrument or mixer parameter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSpec {
    /// Stable identifier used in project files.
    pub id: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub scale: ParamScale,
    pub unit: Unit,
}

/// Values at or below this level are displayed and treated as silence.
pub const SILENT_DB: f32 = -60.0;

impl ParamSpec {
    pub const fn linear(
        id: &'static str,
        label: &'static str,
        min: f32,
        max: f32,
        default: f32,
        step: f32,
        unit: Unit,
    ) -> Self {
        Self { id, label, min, max, default, scale: ParamScale::Linear { step }, unit }
    }

    pub const fn log(
        id: &'static str,
        label: &'static str,
        min: f32,
        max: f32,
        default: f32,
        ratio: f32,
        unit: Unit,
    ) -> Self {
        Self { id, label, min, max, default, scale: ParamScale::Log { ratio }, unit }
    }

    pub const fn stepped(id: &'static str, label: &'static str, names: &'static [&'static str], default: f32) -> Self {
        Self {
            id,
            label,
            min: 0.0,
            max: (names.len() - 1) as f32,
            default,
            scale: ParamScale::Stepped(names),
            unit: Unit::None,
        }
    }

    /// Brings any value (including NaN) into the parameter's valid domain.
    pub fn clamp(&self, value: f32) -> f32 {
        if !value.is_finite() {
            return self.default;
        }
        let v = value.clamp(self.min, self.max);
        match self.scale {
            ParamScale::Stepped(_) => v.round(),
            _ => v,
        }
    }

    /// Moves `value` by `steps` increments (negative moves down). Coarse
    /// steps are ten linear steps or six multiplicative ones.
    pub fn nudge(&self, value: f32, steps: i32, coarse: bool) -> f32 {
        let v = match self.scale {
            ParamScale::Linear { step } => {
                let n = steps as f32 * if coarse { 10.0 } else { 1.0 };
                let q = ((value - self.min) / step).round() + n;
                self.min + q * step
            }
            ParamScale::Log { ratio } => {
                let n = steps * if coarse { 6 } else { 1 };
                value * ratio.powi(n)
            }
            ParamScale::Stepped(_) => value + steps as f32,
        };
        self.clamp(v)
    }

    /// Formats a value for display.
    pub fn format(&self, value: f32) -> String {
        if let ParamScale::Stepped(names) = self.scale {
            return names[(self.clamp(value) as usize).min(names.len() - 1)].to_string();
        }
        match self.unit {
            Unit::None => format!("{value:.2}"),
            Unit::Hz if value >= 1000.0 => format!("{:.2} kHz", value / 1000.0),
            Unit::Hz if value < 10.0 => format!("{value:.2} Hz"),
            Unit::Hz => format!("{value:.0} Hz"),
            Unit::Seconds if value < 0.01 => format!("{:.1} ms", value * 1000.0),
            Unit::Seconds if value < 1.0 => format!("{:.0} ms", value * 1000.0),
            Unit::Seconds => format!("{value:.2} s"),
            Unit::Semitones if value.fract().abs() < 1.0e-4 => format!("{value:+.0} st"),
            Unit::Semitones => format!("{value:+.2} st"),
            Unit::Cents => format!("{value:+.0} ct"),
            Unit::Octaves => format!("{value:+.2} oct"),
            Unit::Decibels if value <= SILENT_DB => "-inf dB".to_string(),
            Unit::Decibels => format!("{value:+.1} dB"),
            Unit::Pan if value.abs() < 0.005 => "C".to_string(),
            Unit::Pan if value < 0.0 => format!("L{:.0}", -value * 100.0),
            Unit::Pan => format!("R{:.0}", value * 100.0),
            Unit::Note => note_name(value.round().clamp(0.0, 127.0) as u8),
            Unit::Percent => format!("{:.1} %", value * 100.0),
        }
    }
}

/// Handle to a sounding note, used to release exactly that note later even
/// if the same pitch has been retriggered in the meantime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VoiceId(pub u32);

impl VoiceId {
    pub const NONE: VoiceId = VoiceId(0);
}

/// A track's sound source.
#[allow(clippy::large_enum_variant)]
pub enum Instrument {
    Synth(PolySynth),
    Sampler(Sampler),
    Drum(Drum),
}

impl Instrument {
    pub fn new(kind: InstrumentKind, sample_rate: f32) -> Self {
        match kind {
            InstrumentKind::Synth => Instrument::Synth(PolySynth::new(sample_rate)),
            InstrumentKind::Sampler => Instrument::Sampler(Sampler::new(sample_rate)),
            k => Instrument::Drum(Drum::new(k, sample_rate)),
        }
    }

    pub fn kind(&self) -> InstrumentKind {
        match self {
            Instrument::Synth(_) => InstrumentKind::Synth,
            Instrument::Sampler(_) => InstrumentKind::Sampler,
            Instrument::Drum(d) => d.kind(),
        }
    }

    /// Sets a parameter (clamped to its spec). Out-of-range indices are ignored.
    pub fn set_param(&mut self, index: usize, value: f32) {
        match self {
            Instrument::Synth(s) => s.set_param(index, value),
            Instrument::Sampler(s) => s.set_param(index, value),
            Instrument::Drum(d) => d.set_param(index, value),
        }
    }

    pub fn param(&self, index: usize) -> Option<f32> {
        match self {
            Instrument::Synth(s) => s.param(index),
            Instrument::Sampler(s) => s.param(index),
            Instrument::Drum(d) => d.param(index),
        }
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) -> VoiceId {
        match self {
            Instrument::Synth(s) => s.note_on(note, velocity),
            Instrument::Sampler(s) => s.note_on(note, velocity),
            Instrument::Drum(d) => {
                d.trigger(note, velocity);
                VoiceId::NONE
            }
        }
    }

    pub fn note_off(&mut self, id: VoiceId) {
        match self {
            Instrument::Synth(s) => s.note_off(id),
            Instrument::Sampler(s) => s.note_off(id),
            Instrument::Drum(_) => {}
        }
    }

    /// Releases every held note (drums ring out naturally).
    pub fn release_all(&mut self) {
        match self {
            Instrument::Synth(s) => s.release_all(),
            Instrument::Sampler(s) => s.release_all(),
            Instrument::Drum(_) => {}
        }
    }

    /// Gives a sampler its sample and returns the one it replaces. Other
    /// instruments hand the sample straight back. The result must be
    /// disposed of off the audio thread.
    #[must_use]
    pub fn set_sample(&mut self, sample: Option<Arc<Sample>>) -> Option<Arc<Sample>> {
        match self {
            Instrument::Sampler(s) => s.set_sample(sample),
            _ => sample,
        }
    }

    /// Fades a ringing open hi-hat out within a few milliseconds (other
    /// instruments ignore it).
    pub fn choke(&mut self) {
        if let Instrument::Drum(d) = self {
            d.choke();
        }
    }

    /// Whether this instrument responds to note-off at all.
    pub fn uses_note_off(&self) -> bool {
        matches!(self, Instrument::Synth(_) | Instrument::Sampler(_))
    }

    /// Adds the instrument's mono output into `out`.
    pub fn render(&mut self, out: &mut [f32]) {
        match self {
            Instrument::Synth(s) => s.render(out),
            Instrument::Sampler(s) => s.render(out),
            Instrument::Drum(d) => d.render(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    #[test]
    fn kind_ids_round_trip() {
        for k in InstrumentKind::ALL {
            assert_eq!(InstrumentKind::from_id(k.id()), Some(k));
        }
        assert_eq!(InstrumentKind::from_id("nope"), None);
    }

    #[test]
    fn param_specs_are_consistent() {
        for k in InstrumentKind::ALL {
            let specs = k.params();
            assert!(!specs.is_empty());
            for (i, s) in specs.iter().enumerate() {
                assert!(s.min < s.max, "{k:?} {}", s.id);
                assert_eq!(s.clamp(s.default), s.default, "{k:?} {}", s.id);
                if let ParamScale::Log { .. } = s.scale {
                    assert!(s.min > 0.0);
                }
                assert!(specs[..i].iter().all(|o| o.id != s.id), "duplicate id {}", s.id);
                assert!(s.id.bytes().all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit()));
            }
            let inst = Instrument::new(k, SR);
            for (i, s) in specs.iter().enumerate() {
                assert_eq!(inst.param(i), Some(s.default), "{k:?} {}", s.id);
            }
            assert_eq!(inst.param(specs.len()), None);
        }
    }

    #[test]
    fn nudge_and_clamp() {
        let lin = ParamSpec::linear("x", "X", -1.0, 1.0, 0.0, 0.05, Unit::Pan);
        assert!((lin.nudge(0.0, 1, false) - 0.05).abs() < 1e-6);
        assert_eq!(lin.nudge(0.98, 1, true), 1.0);
        assert_eq!(lin.clamp(f32::NAN), 0.0);
        let log = ParamSpec::log("f", "F", 20.0, 20_000.0, 1000.0, 2.0, Unit::Hz);
        assert_eq!(log.nudge(1000.0, 1, false), 2000.0);
        assert_eq!(log.nudge(1000.0, -1, true), 20.0);
        let st = ParamSpec::stepped("w", "W", &["a", "b", "c"], 0.0);
        assert_eq!(st.nudge(0.0, 5, false), 2.0);
        assert_eq!(st.format(1.4), "b");
    }

    #[test]
    fn formatting() {
        let db = ParamSpec::linear("v", "V", -60.0, 12.0, 0.0, 0.5, Unit::Decibels);
        assert_eq!(db.format(-60.0), "-inf dB");
        assert_eq!(db.format(-3.0), "-3.0 dB");
        let pan = ParamSpec::linear("p", "P", -1.0, 1.0, 0.0, 0.05, Unit::Pan);
        assert_eq!(pan.format(0.0), "C");
        assert_eq!(pan.format(-0.25), "L25");
        let hz = ParamSpec::log("c", "C", 20.0, 20_000.0, 1000.0, 1.1, Unit::Hz);
        assert_eq!(hz.format(1500.0), "1.50 kHz");
        assert_eq!(hz.format(440.0), "440 Hz");
        let s = ParamSpec::log("t", "T", 0.001, 10.0, 0.1, 1.1, Unit::Seconds);
        assert_eq!(s.format(0.002), "2.0 ms");
        assert_eq!(s.format(0.25), "250 ms");
        assert_eq!(s.format(1.5), "1.50 s");
    }

    /// Every instrument, at every parameter extreme, must produce finite,
    /// bounded output and fall silent after release.
    #[test]
    fn extreme_params_stay_finite_and_finish() {
        for k in InstrumentKind::ALL {
            let specs = k.params();
            for corner in 0..3 {
                let mut inst = Instrument::new(k, SR);
                for (i, s) in specs.iter().enumerate() {
                    let v = match corner {
                        0 => s.min,
                        1 => s.max,
                        _ => {
                            if i % 2 == 0 {
                                s.min
                            } else {
                                s.max
                            }
                        }
                    };
                    inst.set_param(i, v);
                }
                let mut ids = Vec::new();
                for note in [0u8, 24, 60, 96, 127] {
                    ids.push(inst.note_on(note, 127));
                }
                let mut buf = vec![0.0f32; 4800];
                inst.render(&mut buf);
                for id in ids {
                    inst.note_off(id);
                }
                // Longest release/decay is 10 s; render up to 20 s.
                let mut silent = false;
                for _ in 0..200 {
                    buf.fill(0.0);
                    inst.render(&mut buf);
                    for &x in &buf {
                        assert!(x.is_finite() && x.abs() < 8.0, "{k:?} corner {corner}: {x}");
                    }
                    if buf.iter().all(|&x| x == 0.0) {
                        silent = true;
                        break;
                    }
                }
                assert!(silent, "{k:?} corner {corner} never fell silent");
            }
        }
    }

    #[test]
    fn instruments_make_sound_at_defaults() {
        for k in InstrumentKind::ALL {
            let mut inst = Instrument::new(k, SR);
            // A sampler is silent until it has a sample.
            let tone: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin()).collect();
            let sample = Arc::new(Sample::from_mono(&tone, 48_000).unwrap());
            let returned = inst.set_sample(Some(sample));
            assert_eq!(returned.is_some(), k != InstrumentKind::Sampler, "{k:?}");
            inst.note_on(k.default_note(), 100);
            let mut buf = vec![0.0f32; 4800];
            inst.render(&mut buf);
            let peak = buf.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak > 0.05 && peak < 1.5, "{k:?} peak {peak}");
        }
    }
}
