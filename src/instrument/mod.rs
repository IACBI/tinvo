//! Instruments: sound generators driven by note events.
//!
//! Every instrument is a fixed-size value that never allocates, so the
//! engine can create, replace and move instruments on the audio thread. The
//! sampler holds its audio behind an `Arc` it neither allocates nor frees,
//! and a plugin its instance in a box it is handed and hands back.

mod drums;
mod fm;
mod sampler;
mod synth;
pub mod zone;

use crate::clap::PluginRef;
use crate::clap::host::{Instance, NoteId, Role};
use crate::clap::slot::PluginSlot;
use crate::clip::Player;
pub use drums::Drum;
pub use fm::{FM_VOICES, FmSynth};
pub use sampler::{HELD_NOTES, SAMPLER_VOICES, Sampler};
use std::sync::Arc;
pub use synth::{PolySynth, SYNTH_VOICES};
pub use zone::SampleMap;

/// The project format that introduced parameter `index` of `kind`.
pub fn param_since(kind: InstrumentKind, index: usize) -> u32 {
    match kind {
        InstrumentKind::Synth if index == synth::p::LFO_SYNC => 12,
        InstrumentKind::Synth if index >= synth::p::TABLE => 20,
        InstrumentKind::Synth if index >= synth::p::BEND_RANGE => 13,
        InstrumentKind::Synth if index >= synth::p::PW => 3,
        InstrumentKind::Synth if index >= synth::p::GLIDE => 2,
        InstrumentKind::Sampler if index >= sampler::BEND_RANGE => 13,
        InstrumentKind::Fm => 20,
        _ => 1,
    }
}

/// The highest value format `ver` allows parameter `index` of `kind`,
/// where that is below the parameter's own maximum: the choices a stepped
/// parameter gained later. `None` where the format has them all.
pub fn param_max(kind: InstrumentKind, index: usize, ver: u32) -> Option<f32> {
    match kind {
        InstrumentKind::Synth if ver < 20 => synth::max_before_20(index),
        _ => None,
    }
}

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
    /// The four-operator FM synth.
    Fm,
    /// An audio track: it plays clips placed on the song, not notes.
    Audio,
    /// A CLAP plugin, by its id. Not in [`InstrumentKind::ALL`], which is
    /// the built-in kinds.
    Plugin(PluginRef),
}

impl InstrumentKind {
    pub const ALL: [InstrumentKind; 9] = [
        InstrumentKind::Synth,
        InstrumentKind::Kick,
        InstrumentKind::Snare,
        InstrumentKind::HatClosed,
        InstrumentKind::HatOpen,
        InstrumentKind::Clap,
        InstrumentKind::Sampler,
        InstrumentKind::Fm,
        InstrumentKind::Audio,
    ];

    /// Stable identifier used in project files; `plugin` for every plugin,
    /// whose own id goes with it.
    pub fn id(self) -> &'static str {
        match self {
            InstrumentKind::Plugin(_) => "plugin",
            InstrumentKind::Synth => "synth",
            InstrumentKind::Sampler => "sampler",
            InstrumentKind::Kick => "kick",
            InstrumentKind::Snare => "snare",
            InstrumentKind::HatClosed => "hat",
            InstrumentKind::HatOpen => "openhat",
            InstrumentKind::Clap => "clap",
            InstrumentKind::Fm => "fm",
            InstrumentKind::Audio => "audio",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }

    /// The project format that introduced it.
    pub fn since(self) -> u32 {
        match self {
            InstrumentKind::Audio => 21,
            InstrumentKind::Fm => 20,
            InstrumentKind::Plugin(_) => 16,
            InstrumentKind::Sampler => 4,
            _ => 1,
        }
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
            InstrumentKind::Fm => "FM",
            InstrumentKind::Audio => "Audio",
            InstrumentKind::Plugin(_) => "Plugin",
        }
    }

    /// Parameter descriptions, in parameter-index order. A plugin's own
    /// parameters are not among them.
    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            InstrumentKind::Plugin(_) | InstrumentKind::Audio => &[],
            InstrumentKind::Synth => &synth::PARAMS,
            InstrumentKind::Sampler => &sampler::PARAMS,
            InstrumentKind::Fm => &fm::PARAMS,
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
        matches!(
            self,
            InstrumentKind::Kick
                | InstrumentKind::Snare
                | InstrumentKind::HatClosed
                | InstrumentKind::HatOpen
                | InstrumentKind::Clap
        )
    }

    /// Whether it plays notes: steps, live notes and MIDI. An audio track
    /// plays its clips instead.
    pub fn plays_notes(self) -> bool {
        self != InstrumentKind::Audio
    }

    /// The plugin it is, for a plugin.
    pub fn plugin(self) -> Option<PluginRef> {
        match self {
            InstrumentKind::Plugin(r) => Some(r),
            _ => None,
        }
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

/// Most parameters any instrument has.
pub const MAX_PARAMS: usize = 40;

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

    /// Position of `value` in the range, from 0 to 1: linear, or
    /// logarithmic for log-scaled parameters (so ramps sweep evenly by ear).
    pub fn to_unit(&self, value: f32) -> f32 {
        let v = self.clamp(value);
        match self.scale {
            ParamScale::Log { .. } => (v / self.min).ln() / (self.max / self.min).ln(),
            _ => (v - self.min) / (self.max - self.min),
        }
    }

    /// The inverse of [`ParamSpec::to_unit`], clamped (and rounded for
    /// stepped parameters).
    pub fn from_unit(&self, unit: f32) -> f32 {
        let u = if unit.is_finite() { unit.clamp(0.0, 1.0) } else { 0.0 };
        let v = match self.scale {
            ParamScale::Log { .. } => self.min * (self.max / self.min).powf(u),
            _ => self.min + (self.max - self.min) * u,
        };
        self.clamp(v)
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
    Fm(FmSynth),
    Sampler(Sampler),
    Drum(Drum),
    /// Silent until an instance is attached ([`Instrument::attach`]).
    Plugin(PluginSlot),
    /// An audio track's clips.
    Audio(Player),
}

impl Instrument {
    pub fn new(kind: InstrumentKind, sample_rate: f32) -> Self {
        match kind {
            InstrumentKind::Plugin(r) => {
                Instrument::Plugin(PluginSlot::new(r, Role::Instrument, f64::from(sample_rate)))
            }
            InstrumentKind::Synth => Instrument::Synth(PolySynth::new(sample_rate)),
            InstrumentKind::Fm => Instrument::Fm(FmSynth::new(sample_rate)),
            InstrumentKind::Sampler => Instrument::Sampler(Sampler::new(sample_rate)),
            InstrumentKind::Audio => Instrument::Audio(Player::new(sample_rate)),
            k => Instrument::Drum(Drum::new(k, sample_rate)),
        }
    }

    pub fn kind(&self) -> InstrumentKind {
        match self {
            Instrument::Synth(_) => InstrumentKind::Synth,
            Instrument::Fm(_) => InstrumentKind::Fm,
            Instrument::Sampler(_) => InstrumentKind::Sampler,
            Instrument::Drum(d) => d.kind(),
            Instrument::Plugin(p) => InstrumentKind::Plugin(p.plugin()),
            Instrument::Audio(_) => InstrumentKind::Audio,
        }
    }

    /// Sets a parameter (clamped to its spec). Out-of-range indices are ignored.
    pub fn set_param(&mut self, index: usize, value: f32) {
        match self {
            Instrument::Synth(s) => s.set_param(index, value),
            Instrument::Fm(s) => s.set_param(index, value),
            Instrument::Sampler(s) => s.set_param(index, value),
            Instrument::Drum(d) => d.set_param(index, value),
            Instrument::Plugin(_) | Instrument::Audio(_) => {}
        }
    }

    pub fn param(&self, index: usize) -> Option<f32> {
        match self {
            Instrument::Synth(s) => s.param(index),
            Instrument::Fm(s) => s.param(index),
            Instrument::Sampler(s) => s.param(index),
            Instrument::Drum(d) => d.param(index),
            Instrument::Plugin(_) | Instrument::Audio(_) => None,
        }
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) -> VoiceId {
        match self {
            Instrument::Synth(s) => s.note_on(note, velocity),
            Instrument::Fm(s) => s.note_on(note, velocity),
            Instrument::Sampler(s) => s.note_on(note, velocity),
            Instrument::Drum(d) => {
                d.trigger(note, velocity);
                VoiceId::NONE
            }
            // Note ids are 0 to i32::MAX, so their voices never are NONE.
            Instrument::Plugin(p) => p
                .instance()
                .and_then(|i| i.note_on(note, f32::from(velocity.min(127)) / 127.0))
                .map_or(VoiceId::NONE, |n| VoiceId(n.get() as u32 + 1)),
            Instrument::Audio(_) => VoiceId::NONE,
        }
    }

    pub fn note_off(&mut self, id: VoiceId) {
        match self {
            Instrument::Synth(s) => s.note_off(id),
            Instrument::Fm(s) => s.note_off(id),
            Instrument::Sampler(s) => s.note_off(id),
            Instrument::Drum(_) | Instrument::Audio(_) => {}
            Instrument::Plugin(p) => {
                let note = id.0.checked_sub(1).and_then(|n| i32::try_from(n).ok()).and_then(NoteId::from_id);
                if let (Some(i), Some(n)) = (p.instance(), note) {
                    i.note_off(n);
                }
            }
        }
    }

    /// The song's tempo, for a synth's synced LFO.
    pub fn set_tempo(&mut self, bpm: f32) {
        if let Instrument::Synth(s) = self {
            s.set_tempo(bpm);
        }
    }

    #[cfg(test)]
    pub fn tempo(&self) -> Option<f32> {
        match self {
            Instrument::Synth(s) => Some(s.tempo()),
            _ => None,
        }
    }

    /// The pitch bend wheel, −1 to 1: synths and samplers bend their notes
    /// by up to their bend range.
    pub fn set_bend(&mut self, amount: f32) {
        match self {
            Instrument::Synth(s) => s.set_bend(amount),
            Instrument::Fm(s) => s.set_bend(amount),
            Instrument::Sampler(s) => s.set_bend(amount),
            Instrument::Drum(_) | Instrument::Plugin(_) | Instrument::Audio(_) => {}
        }
    }

    /// The mod wheel, 0 to 1: a synth moves the parameter it is set to.
    pub fn set_wheel(&mut self, amount: f32) {
        if let Instrument::Synth(s) = self {
            s.set_wheel(amount);
        }
    }

    /// Playback starts.
    pub fn start(&mut self) {
        match self {
            Instrument::Synth(s) => s.start(),
            Instrument::Sampler(s) => s.start(),
            Instrument::Fm(_) | Instrument::Drum(_) | Instrument::Plugin(_) | Instrument::Audio(_) => {}
        }
    }

    /// Releases every held note (drums ring out naturally).
    pub fn release_all(&mut self) {
        match self {
            Instrument::Synth(s) => s.release_all(),
            Instrument::Fm(s) => s.release_all(),
            Instrument::Sampler(s) => s.release_all(),
            Instrument::Drum(_) | Instrument::Audio(_) => {}
            Instrument::Plugin(p) => {
                if let Some(i) = p.instance() {
                    i.release_all();
                }
            }
        }
    }

    /// Silences every voice at once, released ones too (a plugin is sent a
    /// choke of every note, or All Sound Off). An audio track's clips play
    /// on: they are not notes ([`Player::silence`] stops them).
    pub fn stop_all(&mut self) {
        match self {
            Instrument::Synth(s) => s.stop_all(),
            Instrument::Fm(s) => s.stop_all(),
            Instrument::Sampler(s) => s.stop_all(),
            Instrument::Drum(d) => d.stop(),
            Instrument::Plugin(p) => {
                if let Some(i) = p.instance() {
                    i.choke_all();
                }
            }
            Instrument::Audio(_) => {}
        }
    }

    /// Gives a sampler its samples, or a synth its file's wavetable, and
    /// returns what they replace. Other instruments hand them straight back.
    /// The result must be disposed of off the audio thread.
    #[must_use]
    pub fn set_samples(&mut self, map: Option<Arc<SampleMap>>) -> Option<Arc<SampleMap>> {
        match self {
            Instrument::Sampler(s) => s.set_samples(map),
            Instrument::Synth(s) => s.set_samples(map),
            _ => map,
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
        matches!(self, Instrument::Synth(_) | Instrument::Fm(_) | Instrument::Sampler(_) | Instrument::Plugin(_))
    }

    /// Whether it plays in stereo ([`Instrument::render_stereo`]) rather
    /// than mono ([`Instrument::render`]).
    pub fn is_stereo(&self) -> bool {
        matches!(self, Instrument::Plugin(_) | Instrument::Audio(_))
    }

    /// Adds the instrument's mono output into `out`. A stereo instrument
    /// adds nothing.
    pub fn render(&mut self, out: &mut [f32]) {
        match self {
            Instrument::Synth(s) => s.render(out),
            Instrument::Fm(s) => s.render(out),
            Instrument::Sampler(s) => s.render(out),
            Instrument::Drum(d) => d.render(out),
            Instrument::Plugin(_) | Instrument::Audio(_) => {}
        }
    }

    /// Writes a stereo instrument's output over `out`, interleaved; a mono
    /// one, or a plugin with no instance or one that failed, leaves silence.
    pub fn render_stereo(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        match self {
            Instrument::Plugin(p) => {
                if let Some(i) = p.instance() {
                    i.process(out);
                }
            }
            Instrument::Audio(p) => p.render(out),
            _ => {}
        }
    }

    /// The work a live note costs it, in zones looked through: a sampler
    /// looks through all of its own, at least one either way.
    pub fn note_work(&self) -> usize {
        match self {
            Instrument::Sampler(s) => 1 + s.zone_count(),
            _ => 1,
        }
    }

    /// Whether it is a plugin whose last call said it has a tail to play.
    pub fn has_tail(&self) -> bool {
        matches!(self, Instrument::Plugin(p) if p.has_tail())
    }

    /// The frames a plugin instrument's output comes late by.
    pub fn latency(&self) -> usize {
        match self {
            Instrument::Plugin(p) => p.latency(),
            _ => 0,
        }
    }

    /// An audio track's player.
    pub fn player(&mut self) -> Option<&mut Player> {
        match self {
            Instrument::Audio(p) => Some(p),
            _ => None,
        }
    }

    /// [`Instrument::player`], to look at.
    pub fn player_ref(&self) -> Option<&Player> {
        match self {
            Instrument::Audio(p) => Some(p),
            _ => None,
        }
    }

    /// A plugin instrument's slot.
    pub fn plugin_slot(&mut self) -> Option<&mut PluginSlot> {
        match self {
            Instrument::Plugin(p) => Some(p),
            _ => None,
        }
    }

    /// Puts `instance` in a plugin instrument it fits: returns the one it
    /// replaces, stopped; hands `instance` back from any other instrument.
    pub fn attach(&mut self, instance: Box<Instance>) -> Result<Option<Box<Instance>>, Box<Instance>> {
        match self {
            Instrument::Plugin(p) => p.attach(instance),
            _ => Err(instance),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::Sample;

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
        // An audio track has no instrument parameters.
        assert!(InstrumentKind::Audio.params().is_empty());
        for k in InstrumentKind::ALL.into_iter().filter(|k| k.plays_notes()) {
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
            assert!(specs.len() <= MAX_PARAMS, "{k:?}");
            for s in specs {
                for u in [0.0, 0.25, 0.5, 1.0] {
                    let v = s.from_unit(u);
                    if !matches!(s.scale, ParamScale::Stepped(_)) {
                        assert!((s.to_unit(v) - u).abs() < 1e-4, "{k:?} {} at {u}", s.id);
                    }
                }
                assert_eq!((s.from_unit(0.0), s.from_unit(1.0)), (s.min, s.max), "{k:?} {}", s.id);
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
        for k in InstrumentKind::ALL.into_iter().filter(|k| k.plays_notes()) {
            let mut inst = Instrument::new(k, SR);
            // A sampler is silent until it has a sample.
            let tone: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin()).collect();
            let sample = Arc::new(Sample::from_mono(&tone, 48_000).unwrap());
            let returned = inst.set_samples(Some(Arc::new(SampleMap::single(sample))));
            let keeps = matches!(k, InstrumentKind::Sampler | InstrumentKind::Synth);
            assert_eq!(returned.is_some(), !keeps, "{k:?}");
            inst.note_on(k.default_note(), 100);
            let mut buf = vec![0.0f32; 4800];
            inst.render(&mut buf);
            let peak = buf.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak > 0.05 && peak < 1.5, "{k:?} peak {peak}");
        }
    }
}
