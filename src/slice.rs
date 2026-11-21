//! Cutting a sample into slices, one per key: at its transients, or into
//! equal parts.
//!
//! Transients are found from the energy of the signal's first difference,
//! which weighs an attack's high frequencies over a sustained tone's low
//! ones, in hops of 5 ms. A hop whose energy rises far enough over the
//! loudest of the 50 ms before it (a whole cycle of a 20 Hz tone, so a low
//! note's waveform does not count as a rise) starts a slice, if it is loud
//! enough against the loudest hop. Its start is found to the frame: back
//! from the hop's largest change through the changes that belong to it,
//! then to a zero crossing just before, so a slice starts at its attack
//! without a click. Of the candidates, the loudest are taken first, each
//! only if 40 ms from every one taken, the first slice's start and the end.
//! The analysis keeps only the last 50 ms and a bounded list of candidates,
//! whatever the sample's length and rate.

use crate::instrument::zone::{Mode, Placement};

/// The most slices a sample is cut into.
pub const MAX_SLICES: usize = 64;
/// The key of the first slice; the rest follow it up the keyboard.
pub const FIRST_KEY: u8 = 36;
/// The sensitivity a bare `t`, in percent.
pub const DEFAULT_SENSITIVITY: u8 = 50;
/// How far a cut at a transient is from every other and from the ends.
const MIN_GAP_SECS: f64 = 0.04;
/// The most candidates kept while the analysis runs: far more than a
/// sample's slices, as a hop is one only if it rises over the 50 ms before.
const MAX_CANDIDATES: usize = 16 * MAX_SLICES;
/// The hop the energy is measured in.
const HOP_SECS: f64 = 0.005;
/// How far back a rise is measured from.
const BEFORE_SECS: f64 = 0.05;
/// How far a slice's start moves back to find a zero crossing, and how
/// long a quiet stretch ends an attack's changes, looking back.
const ZERO_SECS: f64 = 0.001;

/// How a sample is cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// Into this many parts of equal length.
    Parts(usize),
    /// At its transients, with a sensitivity from 0 (only the clearest)
    /// to 100 (the faintest).
    Transients(u8),
}

impl How {
    /// Reads what the slice prompt takes: a number of equal parts, or `t`
    /// and an optional sensitivity from 0 to 100 (`t`, `t 70`, `t70`).
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let usage = || format!("a number of equal parts (2 to {MAX_SLICES}), or t and a sensitivity from 0 to 100");
        if let Some(rest) = text.strip_prefix(['t', 'T']) {
            let rest = rest.trim();
            if rest.is_empty() {
                return Ok(How::Transients(DEFAULT_SENSITIVITY));
            }
            return match rest.parse::<u8>() {
                Ok(s) if s <= 100 => Ok(How::Transients(s)),
                _ => Err(usage()),
            };
        }
        match text.parse::<usize>() {
            Ok(n) if (2..=MAX_SLICES).contains(&n) => Ok(How::Parts(n)),
            _ => Err(usage()),
        }
    }

    /// What it reads back as.
    pub fn text(self) -> String {
        match self {
            How::Parts(n) => n.to_string(),
            How::Transients(s) => format!("t {s}"),
        }
    }
}

/// Where the slices of `audio[from..to]` start, in frames of `audio`:
/// `from` first, then on in order, each slice running to the next start
/// and the last to `to`. At most [`MAX_SLICES`]. A frame that is not a
/// number counts as silence.
pub fn starts(audio: &[f32], sample_rate: u32, from: usize, to: usize, how: How) -> Result<Vec<usize>, String> {
    if from >= to || to > audio.len() {
        return Err("nothing to slice".into());
    }
    if u32::try_from(to).is_err() {
        return Err("longer than a zone can play".into());
    }
    let len = to - from;
    match how {
        How::Parts(n) => {
            let n = n.clamp(1, MAX_SLICES);
            if len < n {
                return Err(format!("{len} frames, too few for {n} parts"));
            }
            Ok((0..n).map(|i| from + (len as u128 * i as u128 / n as u128) as usize).collect())
        }
        How::Transients(sensitivity) => Ok(transients(audio, sample_rate, from, to, sensitivity.min(100))),
    }
}

/// Frames of a sample, a frame that is not a number read as silence.
struct Frames<'a> {
    audio: &'a [f32],
    from: usize,
}

impl Frames<'_> {
    fn at(&self, i: usize) -> f64 {
        let x = f64::from(self.audio[i]);
        if x.is_finite() { x } else { 0.0 }
    }

    /// The change into frame `i`; none at the first.
    fn diff(&self, i: usize) -> f64 {
        if i > self.from { self.at(i) - self.at(i - 1) } else { 0.0 }
    }
}

/// How far apart, in frames, cuts at transients are at `sample_rate`, and
/// how far from the ends of what is cut.
pub fn min_gap(sample_rate: u32) -> usize {
    ((f64::from(sample_rate.max(1)) * MIN_GAP_SECS).round() as usize).max(1)
}

fn transients(audio: &[f32], sample_rate: u32, from: usize, to: usize, sensitivity: u8) -> Vec<usize> {
    let sr = f64::from(sample_rate.max(1));
    let hop = ((sr * HOP_SECS).round() as usize).max(1);
    let gap = min_gap(sample_rate);
    let zero = (sr * ZERO_SECS).round() as usize;
    let back = ((sr * BEFORE_SECS / hop as f64).round() as usize).max(1);
    let f = Frames { audio, from };
    let energy = |h: usize| -> f64 { (h..(h + hop).min(to)).map(|i| f.diff(i).powi(2)).sum() };
    let hops = (to - from).div_ceil(hop);
    let loudest = (0..hops).map(|n| energy(from + n * hop)).fold(0.0, f64::max);
    if loudest <= 0.0 {
        return vec![from];
    }
    // A rise from 18 dB at 0 down to 3 dB at 100, to a level (in amplitude)
    // against the loudest hop from -12 dB down to -40 dB.
    let s = f64::from(sensitivity) / 100.0;
    let rise = 10f64.powf((18.0 - 15.0 * s) / 10.0);
    let floor = loudest * 10f64.powf((-12.0 - 28.0 * s) / 20.0).powi(2);
    let tiny = loudest * 1e-12;
    let mut recent = vec![0.0f64; back];
    let mut found: Vec<(usize, f64)> = Vec::new();
    for n in 0..hops {
        let e = energy(from + n * hop);
        let before = recent.iter().copied().fold(0.0, f64::max);
        recent[n % back] = e;
        if n == 0 || e < floor || e + tiny < rise * (before + tiny) {
            continue;
        }
        let at = attack(&f, from + n * hop, hop, back * hop, to, zero);
        if at >= from + gap && at + gap <= to {
            found.push((at, e));
            if found.len() >= 2 * MAX_CANDIDATES {
                loudest_first(&mut found);
                found.truncate(MAX_CANDIDATES);
            }
        }
    }
    // The loudest first, each only if far enough from every one taken.
    loudest_first(&mut found);
    let mut cuts: Vec<usize> = Vec::with_capacity(MAX_SLICES);
    for (at, _) in found {
        if cuts.len() + 1 >= MAX_SLICES {
            break;
        }
        if cuts.iter().all(|&c| c.abs_diff(at) >= gap) {
            cuts.push(at);
        }
    }
    cuts.sort_unstable();
    std::iter::once(from).chain(cuts).collect()
}

/// Loudest first; of equals, the earlier.
fn loudest_first(found: &mut [(usize, f64)]) {
    found.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
}

/// The frame a transient found in the hop at `hop_start` starts at. From
/// the hop's largest change, back (up to `reach` frames before the hop)
/// through the changes of at least a tenth of it, until `zero` frames in
/// a row fall short; then the frame before that, moved back up to `zero`
/// frames to where the signal crosses zero, on the side nearer it.
fn attack(f: &Frames, hop_start: usize, hop: usize, reach: usize, to: usize, zero: usize) -> usize {
    let lo = hop_start.saturating_sub(reach).max(f.from + 1);
    let hi = (hop_start + hop).min(to);
    let (peak_at, peak) = (hop_start.max(lo)..hi)
        .map(|i| (i, f.diff(i).abs()))
        .fold((hop_start, 0.0), |best, x| if x.1 > best.1 { x } else { best });
    let (mut onset, mut quiet) = (peak_at, 0);
    for i in (lo..peak_at).rev() {
        if f.diff(i).abs() >= peak * 0.1 {
            onset = i;
            quiet = 0;
        } else {
            quiet += 1;
            if quiet >= zero.max(1) {
                break;
            }
        }
    }
    let at = onset.saturating_sub(1).max(f.from);
    // Both frames of a crossing within `zero` of `at`.
    let stop = at.saturating_sub(zero).max(f.from);
    for i in (stop + 1..=at).rev() {
        let (x, before) = (f.at(i), f.at(i - 1));
        if x == 0.0 {
            return i;
        }
        if before == 0.0 {
            return i - 1;
        }
        if (x > 0.0) != (before > 0.0) {
            return if before.abs() < x.abs() { i - 1 } else { i };
        }
    }
    at
}

/// The zones of slices starting at `starts` and ending at `to`: one key
/// each from [`FIRST_KEY`], played at the pitch it was recorded at, in
/// the track's mode. `to` is at most `u32::MAX`, as [`starts`] checks.
pub fn placements(starts: &[usize], to: usize) -> Vec<Placement> {
    let frame = |x: usize| u32::try_from(x).unwrap_or(u32::MAX);
    let ends = starts.iter().skip(1).copied().chain(std::iter::once(to));
    starts
        .iter()
        .zip(ends)
        .enumerate()
        .take(MAX_SLICES)
        .map(|(k, (&start, end))| {
            let key = FIRST_KEY + k as u8;
            Placement {
                keys: (key, key),
                root: Some(key),
                start: frame(start),
                end: Some(frame(end)),
                mode: Mode::Track,
                ..Placement::WHOLE
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::noise::Rng;

    const SR: u32 = 48_000;

    /// `secs` of silence with hits at `at` (seconds, gain): each a
    /// decaying noise burst over a decaying low sine, as a drum.
    fn hits(secs: f64, at: &[(f64, f32)]) -> Vec<f32> {
        let mut audio = vec![0.0f32; (secs * f64::from(SR)) as usize];
        let mut rng = Rng::new(7);
        for &(t, gain) in at {
            let start = (t * f64::from(SR)) as usize;
            for (k, x) in audio[start..].iter_mut().enumerate().take(SR as usize / 4) {
                let s = k as f32 / SR as f32;
                let body = (std::f32::consts::TAU * 80.0 * s).sin() * (-s * 20.0).exp();
                let noise = rng.bipolar() * (-s * 60.0).exp();
                *x += gain * (0.6 * body + 0.4 * noise);
            }
        }
        audio
    }

    fn frames(t: f64) -> usize {
        (t * f64::from(SR)) as usize
    }

    fn near(found: &[usize], want: &[usize], slack: usize) -> bool {
        found.len() == want.len() && found.iter().zip(want).all(|(f, w)| f.abs_diff(*w) <= slack)
    }

    #[test]
    fn hits_start_slices_at_their_attacks() {
        let at = [(0.25, 1.0), (0.8, 0.5), (1.3, 1.0), (2.05, 0.1), (2.9, 0.7)];
        let audio = hits(3.5, &at);
        let found = starts(&audio, SR, 0, audio.len(), How::Transients(50)).unwrap();
        let want: Vec<usize> = std::iter::once(0).chain(at.iter().map(|&(t, _)| frames(t))).collect();
        // Within a millisecond of each attack, and never after it.
        assert!(near(&found, &want, 48), "{found:?} for {want:?}");
        assert!(found.iter().zip(&want).all(|(f, w)| f <= w), "{found:?}");
        // The same each time, and within a part of the file.
        assert_eq!(starts(&audio, SR, 0, audio.len(), How::Transients(50)).unwrap(), found);
        let part = starts(&audio, SR, frames(1.0), frames(2.5), How::Transients(50)).unwrap();
        assert!(near(&part, &[frames(1.0), frames(1.3), frames(2.05)], 48), "{part:?}");
    }

    #[test]
    fn a_hit_at_the_start_or_too_near_the_last_adds_none() {
        let audio = hits(1.0, &[(0.0, 1.0), (0.5, 1.0), (0.51, 1.0)]);
        let found = starts(&audio, SR, 0, audio.len(), How::Transients(100)).unwrap();
        assert!(near(&found, &[0, frames(0.5)], 48), "{found:?}");
    }

    #[test]
    fn a_tone_that_only_decays_or_holds_has_no_transient() {
        let n = SR as usize * 2;
        let decay: Vec<f32> = (0..n).map(|i| (i as f32 * 0.02).sin() * (-(i as f32) / SR as f32 * 2.0).exp()).collect();
        assert_eq!(starts(&decay, SR, 0, n, How::Transients(100)).unwrap(), [0]);
        let mut rng = Rng::new(3);
        let held: Vec<f32> = (0..n).map(|_| 0.5 * rng.bipolar()).collect();
        assert_eq!(starts(&held, SR, 0, n, How::Transients(100)).unwrap(), [0]);
        assert_eq!(starts(&vec![0.0; n], SR, 0, n, How::Transients(100)).unwrap(), [0]);
    }

    #[test]
    fn sensitivity_decides_on_a_faint_hit_over_a_tail() {
        // A quiet hit while a loud one still rings: 20 dB under it.
        let audio = hits(1.0, &[(0.0, 1.0), (0.1, 0.1)]);
        let at = |s: u8| starts(&audio, SR, 0, audio.len(), How::Transients(s)).unwrap().len();
        assert_eq!((at(0), at(100)), (1, 2));
    }

    #[test]
    fn a_drum_loop_slices_on_every_hit() {
        // Two bars at 170 BPM: a hit on every eighth, some ghost notes.
        let eighth = 60.0 / 170.0 / 2.0;
        let at: Vec<(f64, f32)> = (0..16).map(|k| (0.05 + k as f64 * eighth, [1.0, 0.3, 0.7, 0.3][k % 4])).collect();
        let audio = hits(0.05 + 16.0 * eighth + 0.3, &at);
        let found = starts(&audio, SR, 0, audio.len(), How::Transients(DEFAULT_SENSITIVITY)).unwrap();
        let want: Vec<usize> = std::iter::once(0).chain(at.iter().map(|&(t, _)| frames(t))).collect();
        assert!(near(&found, &want, 48), "{found:?}");
    }

    #[test]
    fn many_hits_keep_the_clearest() {
        // 100 hits 80 ms apart (sixteenths at 188 BPM), every tenth loud,
        // over a held noise.
        let mut rng = Rng::new(9);
        let mut audio: Vec<f32> = (0..SR as usize * 9).map(|_| 0.01 * rng.bipolar()).collect();
        for k in 0..100 {
            let start = frames(0.1 + k as f64 * 0.08);
            let gain = if k % 10 == 0 { 1.0 } else { 0.2 };
            for x in &mut audio[start..start + 100] {
                *x += gain * rng.bipolar();
            }
        }
        let found = starts(&audio, SR, 0, audio.len(), How::Transients(100)).unwrap();
        assert_eq!(found.len(), MAX_SLICES);
        assert!(found.windows(2).all(|w| w[0] < w[1]));
        for k in (0..100).step_by(10) {
            let want = frames(0.1 + k as f64 * 0.08);
            assert!(found.iter().any(|f| f.abs_diff(want) <= 48), "the loud hit at {want} kept");
        }
    }

    #[test]
    fn close_hits_keep_the_loudest_and_what_fits_beside_it() {
        // Three hits 35 ms apart, each louder: the last, and the first,
        // 70 ms before it; not the middle one, too near both.
        let audio = hits(1.0, &[(0.2, 0.3), (0.235, 0.6), (0.27, 1.0)]);
        let found = starts(&audio, SR, 0, audio.len(), How::Transients(100)).unwrap();
        assert!(near(&found, &[0, frames(0.2), frames(0.27)], 48), "{found:?}");
        let gap = min_gap(SR);
        assert!(found.windows(2).all(|w| w[1] - w[0] >= gap) && audio.len() - found[found.len() - 1] >= gap);
        // Two 35 ms apart, the later louder: the later, though it came second.
        let audio = hits(1.0, &[(0.6, 0.3), (0.635, 1.0)]);
        let found = starts(&audio, SR, 0, audio.len(), How::Transients(100)).unwrap();
        assert!(near(&found, &[0, frames(0.635)], 48), "{found:?}");
    }

    #[test]
    fn a_low_tone_held_or_begun_is_one_transient_at_most() {
        // A 20 Hz and a 45 Hz sine at every phase against the hops: the
        // waveform's own swings are not attacks, at any sensitivity.
        for (freq, phase) in [(20.0, 0.0), (20.0, 1.3), (45.0, 0.7), (45.0, 2.9)] {
            let tone = |i: usize| 0.8 * (std::f32::consts::TAU * freq * i as f32 / SR as f32 + phase).sin();
            let held: Vec<f32> = (0..SR as usize).map(tone).collect();
            assert_eq!(starts(&held, SR, 0, held.len(), How::Transients(100)).unwrap(), [0], "{freq} Hz");
            // Begun from silence, it starts one slice where it begins.
            let begun: Vec<f32> = (0..SR as usize).map(|i| if i < 12_000 { 0.0 } else { tone(i) }).collect();
            let found = starts(&begun, SR, 0, begun.len(), How::Transients(100)).unwrap();
            assert!(near(&found, &[0, 12_000], 48), "{freq} Hz: {found:?}");
        }
    }

    #[test]
    fn an_attack_starts_where_it_does_not_where_something_came_before() {
        // A faint step 8.5 ms before a loud one: one slice, at the loud one,
        // which is not lost to the faint one taking its place.
        let mut audio = vec![0.0f32; 9_600];
        audio[2_410..].fill(0.001);
        let mut rng = Rng::new(5);
        for x in &mut audio[2_820..4_000] {
            *x += rng.bipolar();
        }
        assert_eq!(starts(&audio, SR, 0, audio.len(), How::Transients(100)).unwrap(), [0, 2_819]);
        // A small change 5 ms before an attack, in the hop before it, is not
        // where it starts.
        let mut audio = vec![0.0f32; 9_600];
        audio[2_200] = 0.07;
        for x in &mut audio[2_441..3_400] {
            *x = 0.25 * rng.bipolar();
        }
        let found = starts(&audio, SR, 0, audio.len(), How::Transients(100)).unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!((2_400..=2_441).contains(&found[1]), "{found:?}");
        // Of a crossing just before the attack, the side nearer zero.
        let mut audio = vec![-0.005f32; 9_600];
        audio[3_990..].fill(0.1);
        for x in &mut audio[4_000..5_000] {
            *x = rng.bipolar();
        }
        assert_eq!(starts(&audio, SR, 0, audio.len(), How::Transients(100)).unwrap(), [0, 3_989]);
        // An exact zero as far back as it looks, before negative frames.
        let mut audio = vec![-0.005f32; 9_600];
        audio[3_951] = 0.0;
        for x in &mut audio[4_000..5_000] {
            *x = -0.8 + 0.1 * rng.bipolar();
        }
        assert_eq!(starts(&audio, SR, 0, audio.len(), How::Transients(100)).unwrap(), [0, 3_951]);
    }

    #[test]
    fn odd_input_stays_in_bounds_and_time() {
        // A hit too near the end would leave a slice shorter than the
        // shortest: none.
        let mut audio = vec![0.0f32; 4_800];
        audio[4_799] = 1.0;
        assert_eq!(starts(&audio, SR, 0, audio.len(), How::Transients(100)).unwrap(), [0]);
        // Frames that are not numbers are silence.
        // Frames that are not numbers are silence: here around one hit.
        let mut odd = hits(1.0, &[(0.5, 1.0)]);
        for (k, x) in odd.iter_mut().enumerate().filter(|(k, _)| k % 4_800 == 100) {
            *x = if k % 9_600 == 100 { f32::NAN } else { f32::INFINITY };
        }
        let found = starts(&odd, SR, 0, odd.len(), How::Transients(100)).unwrap();
        assert!(near(&found, &[0, frames(0.5)], 48), "{found:?}");
        // One frame a second, millions of them, three quiet then three loud:
        // the analysis keeps little and ends in time.
        let odd: Vec<f32> = (0..3_000_000).map(|i| if i % 6 < 3 { 0.0 } else { 1.0 }).collect();
        let t = std::time::Instant::now();
        let found = starts(&odd, 1, 0, odd.len(), How::Transients(100)).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
        assert!(found.len() <= MAX_SLICES && found.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn equal_parts_split_exactly() {
        let audio = vec![0.1; 1000];
        assert_eq!(starts(&audio, SR, 0, 1000, How::Parts(3)).unwrap(), [0, 333, 666]);
        assert_eq!(starts(&audio, SR, 100, 500, How::Parts(4)).unwrap(), [100, 200, 300, 400]);
        assert!(starts(&audio, SR, 0, 3, How::Parts(4)).unwrap_err().contains("too few"));
        assert!(starts(&audio, SR, 500, 500, How::Parts(2)).is_err());
        assert!(starts(&audio, SR, 0, 1001, How::Parts(2)).is_err(), "past the audio");
        let z = placements(&[100, 200, 300], 450);
        assert_eq!(z.len(), 3);
        assert_eq!((z[2].keys, z[2].root, z[2].start, z[2].end), ((38, 38), Some(38), 300, Some(450)));
        assert!(z.iter().all(|p| p.check().is_ok() && p.mode == Mode::Track));
    }

    #[test]
    fn the_prompt_reads_parts_or_transients() {
        assert_eq!(How::parse("16"), Ok(How::Parts(16)));
        assert_eq!(How::parse(" t "), Ok(How::Transients(DEFAULT_SENSITIVITY)));
        assert_eq!(How::parse("t 70"), Ok(How::Transients(70)));
        assert_eq!(How::parse("T0"), Ok(How::Transients(0)));
        for bad in ["", "1", "65", "x", "t 101", "t -1", "t x", "t 50.5"] {
            assert!(How::parse(bad).is_err(), "{bad}");
        }
        for how in [How::Parts(8), How::Transients(70)] {
            assert_eq!(How::parse(&how.text()), Ok(how));
        }
    }
}
