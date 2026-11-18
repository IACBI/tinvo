//! Group buses: the tracks routed to a group are summed into its stereo
//! input, which runs through a stereo channel strip, the group's fader and
//! balance, and its sends, before it joins the master.
//!
//! A group's settings mirror a track's: volume and pan first (lane targets
//! 0 and 1), then the strip's parameters and the three bus sends, which are
//! taken after the group's fader as a track's are. There is no key send:
//! the key bus is fed from instruments, before any strip. At its defaults a
//! group passes its input through unchanged (gain 1 on both channels).
//!
//! A group runs while a track feeds it and for [`HOLD_TIME`] after the last
//! frame one did, so its strip rings out, and for as long again whenever
//! its insert chain still holds sound at the end (a long echo), then stops
//! costing anything. The hold is counted in frames, so a group stops on the
//! same frame whatever the buffer size.

use crate::dsp::smooth::Smoother;
use crate::engine::{BLOCK, fader_gain};
use crate::insert::{Chain, Insert};
use crate::instrument::ParamSpec;
use crate::mix::{Mix, SEND_PARAMS, level_gain, send};
use crate::strip::{self, StereoStrip, sp};

/// The most groups a project holds.
pub const MAX_GROUPS: usize = 8;

/// How long a group runs on after the last frame a track fed it: long
/// enough for the strip's slowest ring (the mid band at 100 Hz, Q 8 and
/// +18 dB, whose poles decay about 121 dB a second) to fall some 240 dB.
/// Then its strip lets go of what it held.
pub const HOLD_TIME: f32 = 2.0;

/// A group's parameters after its volume and pan: the strip's, then its
/// sends to the delay, reverb and chorus buses.
pub const GROUP_PARAMS: usize = sp::COUNT + send::BUSES;

/// Group parameter `index` (see [`GROUP_PARAMS`]).
pub fn group_param(index: usize) -> Option<&'static ParamSpec> {
    if index < sp::COUNT { strip::PARAMS.get(index) } else { SEND_PARAMS[..send::BUSES].get(index - sp::COUNT) }
}

/// Left and right gains of a balance control on a stereo signal: both 1 at
/// the centre, and towards either end the other side fades out on a
/// quarter cosine, so the image moves without the near side getting louder.
pub fn balance_gains(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    let fade = |x: f32| (x * std::f32::consts::FRAC_PI_2).cos();
    (if p > 0.0 { fade(p) } else { 1.0 }, if p < 0.0 { fade(-p) } else { 1.0 })
}

/// One group bus on the audio thread.
#[derive(Clone, Debug)]
pub struct Group {
    strip: StereoStrip,
    /// Its insert chain, after the strip. The engine changes it, and frees
    /// what it held, since freeing needs the retirement queue.
    pub chain: Chain,
    /// Parameter values set by edits; automation overrides them while a
    /// point is in force.
    base: [f32; GROUP_PARAMS],
    pub gain_db: f32,
    pub pan: f32,
    pub mute: bool,
    /// Automated volume and pan, overriding `gain_db` and `pan`.
    pub auto_gain: Option<f32>,
    pub auto_pan: Option<f32>,
    gain_l: Smoother,
    gain_r: Smoother,
    sends: [Smoother; send::BUSES],
    /// The members' sum for the current block.
    pub in_l: [f32; BLOCK],
    pub in_r: [f32; BLOCK],
    /// The input interleaved, for the strip.
    buf: [f32; 2 * BLOCK],
    /// Loudest output sample since last published.
    pub peak: f32,
    /// Deepest compressor gain reduction since last published (dB, >= 0).
    pub reduction: f32,
    /// Frames it still runs for with no track feeding it.
    hold_left: u32,
    smoothing: f32,
    sample_rate: f32,
}

impl Group {
    pub fn new(smoothing: f32, sample_rate: f32) -> Self {
        Self {
            strip: StereoStrip::new(sample_rate),
            chain: Chain::default(),
            base: std::array::from_fn(|i| group_param(i).map_or(0.0, |s| s.default)),
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            auto_gain: None,
            auto_pan: None,
            gain_l: Smoother::new(1.0, smoothing, sample_rate),
            gain_r: Smoother::new(1.0, smoothing, sample_rate),
            sends: [Smoother::new(0.0, smoothing, sample_rate); send::BUSES],
            in_l: [0.0; BLOCK],
            in_r: [0.0; BLOCK],
            buf: [0.0; 2 * BLOCK],
            peak: 0.0,
            reduction: 0.0,
            hold_left: 0,
            smoothing,
            sample_rate,
        }
    }

    /// Turns the group into a fresh one in place: a whole new `Group` would
    /// be built on the (audio thread's) stack first. Its chain must have
    /// been emptied already.
    pub fn reset(&mut self) {
        debug_assert!(self.chain.is_empty(), "effects freed on the audio thread");
        let (smoothing, sample_rate) = (self.smoothing, self.sample_rate);
        self.strip = StereoStrip::new(sample_rate);
        self.base = std::array::from_fn(|i| group_param(i).map_or(0.0, |s| s.default));
        self.gain_db = 0.0;
        self.pan = 0.0;
        self.mute = false;
        self.auto_gain = None;
        self.auto_pan = None;
        self.gain_l = Smoother::new(1.0, smoothing, sample_rate);
        self.gain_r = Smoother::new(1.0, smoothing, sample_rate);
        self.sends = [Smoother::new(0.0, smoothing, sample_rate); send::BUSES];
        self.peak = 0.0;
        self.reduction = 0.0;
        self.hold_left = 0;
    }

    /// The frames a group runs for after a track last fed it.
    fn hold(&self) -> u32 {
        (HOLD_TIME * self.sample_rate) as u32
    }

    /// Whether it still has output to give: fed, or ringing out.
    pub fn is_live(&self) -> bool {
        self.hold_left > 0
    }

    /// Keeps a group running for its whole hold, as one that was just fed
    /// (one taken out of the project, still ringing).
    pub fn hold_on(&mut self) {
        self.hold_left = self.hold();
    }

    /// Glides land at once while the group is silent: no one hears them,
    /// and a group fed again starts from its settings.
    fn settle_if_idle(&mut self) {
        if !self.is_live() {
            self.snap();
        }
    }

    /// Sets parameter `i` from an edit; `automated` keeps the value a lane
    /// holds in force.
    pub fn set_base(&mut self, i: usize, value: f32, automated: bool) {
        let Some(spec) = group_param(i) else { return };
        self.base[i] = spec.clamp(value);
        if !automated {
            self.set_param(i, self.base[i]);
        }
        self.settle_if_idle();
    }

    fn set_param(&mut self, i: usize, value: f32) {
        if i < sp::COUNT {
            self.strip.set_param(i, value);
        } else if let Some(spec) = group_param(i) {
            self.sends[i - sp::COUNT].set_target(level_gain(spec.clamp(value)));
        }
    }

    /// Writes an automation value (`None`: the base value) to lane target
    /// `target`: 0 volume, 1 pan, then the parameters. Returns whether the
    /// gains need updating.
    pub fn write_target(&mut self, target: u8, value: Option<f32>) -> bool {
        match target {
            0 => self.auto_gain = value,
            1 => self.auto_pan = value,
            t => {
                let i = usize::from(t) - 2;
                if let Some(&base) = self.base.get(i) {
                    self.set_param(i, value.unwrap_or(base));
                }
                self.settle_if_idle();
                return false;
            }
        }
        true
    }

    /// Sets the fader and balance gains from volume, pan and mute.
    pub fn update_gains(&mut self) {
        let g = if self.mute { 0.0 } else { fader_gain(self.auto_gain.unwrap_or(self.gain_db)) };
        let (l, r) = balance_gains(self.auto_pan.unwrap_or(self.pan));
        self.gain_l.set_target(g * l);
        self.gain_r.set_target(g * r);
        self.settle_if_idle();
    }

    /// Jumps glides to their targets, as for a project that is opened.
    pub fn snap(&mut self) {
        self.strip.snap();
        for s in [&mut self.gain_l, &mut self.gain_r].into_iter().chain(&mut self.sends) {
            let t = s.target();
            s.snap(t);
        }
    }

    /// Runs frames `a..b` of the input through the strip, fader and
    /// balance into `mix_l` and `mix_r` (`b - a` frames long) and the
    /// buses, where tracks fed the first `fed` of those frames. With none,
    /// it runs only for what is left of its hold. `key` is the key bus for
    /// the same frames; `at` is the segment's first sample in the
    /// interleaved bus inputs; effects its chain lets go of go to `retire`.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        fed: usize,
        a: usize,
        b: usize,
        key: &[f32],
        mix_l: &mut [f32],
        mix_r: &mut [f32],
        at: usize,
        mix: &mut Mix,
        retire: &mut impl FnMut(Box<Insert>),
    ) {
        if fed > 0 {
            // The hold counts from the last frame fed.
            self.hold_left = self.hold().saturating_sub((b - a - fed) as u32);
            self.run(a, b - a, key, mix_l, mix_r, at, mix, retire);
            return;
        }
        let mut done = 0;
        while done < b - a {
            let n = (b - a - done).min(self.hold_left as usize);
            self.hold_left -= n as u32;
            let from = a + done;
            self.run(from, n, &key[done..], &mut mix_l[done..], &mut mix_r[done..], at + 2 * done, mix, retire);
            done += n;
            // An echo still in the chain keeps it running, from the frame
            // the hold ran out on.
            if self.hold_left > 0 || self.chain.is_idle() {
                break;
            }
            self.hold_left = self.hold();
        }
        if self.hold_left == 0 {
            self.strip.clear();
            self.snap();
            self.chain.rest(retire);
        }
    }

    /// Runs `n` frames from frame `a` of the input, as [`Group::process`]
    /// says, with `key`, `mix_l` and `mix_r` starting at those frames.
    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        a: usize,
        n: usize,
        key: &[f32],
        mix_l: &mut [f32],
        mix_r: &mut [f32],
        at: usize,
        mix: &mut Mix,
        retire: &mut impl FnMut(Box<Insert>),
    ) {
        if n == 0 {
            return;
        }
        let b = a + n;
        let key = &key[..n];
        let buf = &mut self.buf[..2 * n];
        for (f, (&l, &r)) in buf.chunks_exact_mut(2).zip(self.in_l[a..b].iter().zip(&self.in_r[a..b])) {
            f[0] = l;
            f[1] = r;
        }
        self.strip.process(buf, key);
        if !self.chain.is_empty() {
            self.chain.process(buf, key, retire);
        }
        let sending: [bool; send::BUSES] = std::array::from_fn(|k| {
            let s = &self.sends[k];
            s.current() != 0.0 || s.target() != 0.0
        });
        let mut inputs = sending.contains(&true).then(|| mix.inputs(sending));
        let mut peak = self.peak;
        for (i, f) in buf.chunks_exact(2).enumerate() {
            let l = f[0] * self.gain_l.tick();
            let r = f[1] * self.gain_r.tick();
            mix_l[i] += l;
            mix_r[i] += r;
            peak = peak.max(l.abs()).max(r.abs());
            if let Some(inputs) = &mut inputs {
                let j = at + 2 * i;
                for (bus, input) in inputs.iter_mut().enumerate() {
                    if sending[bus] {
                        let g = self.sends[bus].tick();
                        input[j] += l * g;
                        input[j + 1] += r * g;
                    }
                }
            }
        }
        if peak.is_finite() {
            self.peak = peak;
        }
        let r = -self.strip.gain_reduction_db();
        if r > self.reduction {
            self.reduction = r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_balance_is_unity_at_the_centre_and_cuts_the_far_side() {
        assert_eq!(balance_gains(0.0), (1.0, 1.0));
        assert_eq!(balance_gains(1.0).1, 1.0);
        assert!(balance_gains(1.0).0.abs() < 1e-7);
        assert_eq!(balance_gains(-1.0).0, 1.0);
        assert!(balance_gains(-1.0).1.abs() < 1e-7);
        let (l, r) = balance_gains(0.5);
        assert!((l - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6 && r == 1.0);
        assert_eq!(balance_gains(-3.0), balance_gains(-1.0));
    }

    #[test]
    fn parameters_are_the_strip_then_the_bus_sends() {
        assert_eq!(group_param(0).map(|s| s.id), Some("hp_freq"));
        assert_eq!(group_param(sp::COUNT).map(|s| s.id), Some("send_delay"));
        assert_eq!(group_param(GROUP_PARAMS - 1).map(|s| s.id), Some("send_chorus"));
        assert!(group_param(GROUP_PARAMS).is_none());
    }
}
