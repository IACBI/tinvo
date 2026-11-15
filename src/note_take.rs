//! Notes recorded while the pattern plays, turned into step edits.
//!
//! The engine reports every key pressed on the live track at its nearest
//! step, and every step the live track plays ([`Take`]). A [`NoteTake`]
//! collects those reports and, when recording stops, writes them as
//! [`Edit::SetStep`]s: one step per (track, pattern, step), whatever was
//! played on it becoming a chord.

use crate::engine::{Edit, MAX_CHORD, MAX_PATTERNS, MAX_STEPS, Step, Take};
use crate::project::Project;
use crate::settings::Recording;
use std::collections::{BTreeMap, HashMap};

/// A key pressed while recording.
#[derive(Clone, Copy, Debug)]
struct Played {
    track: usize,
    pattern: u8,
    step: u8,
    note: u8,
    velocity: u8,
    offset: i8,
    /// Which pass over its step the note belongs to (the first is 1).
    pass: u32,
    on: f64,
    off: Option<f64>,
}

/// The notes of one recording, from [`Take`] events.
#[derive(Debug, Default)]
pub struct NoteTake {
    played: Vec<Played>,
    /// Keys down, as indices into `played`.
    held: Vec<usize>,
    /// How many times each (track, pattern, step) has played.
    passes: HashMap<(usize, u8, u8), u32>,
    /// The latest position reported, for keys still down at the end.
    last: f64,
}

impl NoteTake {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, take: Take) {
        match take {
            Take::On { track, note, velocity, pattern, step, offset, early, position } => {
                let track = usize::from(track);
                let seen = self.passes.get(&(track, pattern, step)).copied().unwrap_or(0);
                // A key whose step has not played yet belongs to the pass
                // that is coming.
                let pass = seen + u32::from(early);
                // A key pressed again before its release ends its first press.
                self.release(note, position);
                self.held.push(self.played.len());
                self.played.push(Played {
                    track,
                    pattern,
                    step,
                    note,
                    velocity,
                    offset,
                    pass,
                    on: position,
                    off: None,
                });
                self.last = self.last.max(position);
            }
            Take::Off { note, position } => {
                self.release(note, position);
                self.last = self.last.max(position);
            }
            Take::Passed { track, pattern, step } => {
                *self.passes.entry((usize::from(track), pattern, step)).or_insert(0) += 1;
            }
        }
    }

    fn release(&mut self, note: u8, position: f64) {
        if let Some(i) = self.held.iter().position(|&i| self.played[i].note == note) {
            let p = self.held.swap_remove(i);
            self.played[p].off = Some(position);
        }
    }

    /// Follows the tracks through an edit that inserted or removed one:
    /// `follow` gives a track's new index, or `None` once it is gone.
    pub fn follow(&mut self, follow: impl Fn(usize) -> Option<usize>) {
        let mut kept = Vec::with_capacity(self.played.len());
        let mut index = vec![None; self.played.len()];
        for (i, p) in self.played.iter().enumerate() {
            if let Some(track) = follow(p.track) {
                index[i] = Some(kept.len());
                kept.push(Played { track, ..*p });
            }
        }
        self.played = kept;
        self.held = self.held.iter().filter_map(|&i| index[i]).collect();
        self.passes = self.passes.drain().filter_map(|((t, p, s), n)| Some(((follow(t)?, p, s), n))).collect();
    }

    /// Whether the take holds nothing to write.
    pub fn is_empty(&self) -> bool {
        self.played.is_empty() && self.passes.is_empty()
    }

    /// The step edits that write the take into `project`, a new step taking
    /// its other fields from its track's brush.
    ///
    /// Overdub adds the notes to each step as a chord, keeping the newest
    /// [`MAX_CHORD`] notes. Replace writes each step the playhead passed
    /// (or a key was played for) with only the notes of its last pass,
    /// clearing it if that pass played none. The step's timing, velocity
    /// and length come from the notes of its last pass played: its first
    /// note's offset (0 with `quantize`), its loudest note's velocity and
    /// its longest note's length in steps.
    pub fn edits(&self, project: &Project, brushes: &[Step], rec: Recording) -> Vec<Edit> {
        let mut steps: BTreeMap<(usize, u8, u8), Vec<&Played>> = BTreeMap::new();
        for p in &self.played {
            steps.entry((p.track, p.pattern, p.step)).or_default().push(p);
        }
        if rec.replace {
            for &key in self.passes.keys() {
                steps.entry(key).or_default();
            }
        }
        let mut edits = Vec::new();
        for ((track, pattern, step), notes) in steps {
            let (Some(t), Some(&brush)) = (project.tracks.get(track), brushes.get(track)) else { continue };
            if usize::from(pattern) >= MAX_PATTERNS || usize::from(step) >= MAX_STEPS {
                continue;
            }
            let seen = self.passes.get(&(track, pattern, step)).copied().unwrap_or(0);
            let lead = notes.iter().map(|p| p.pass).max().unwrap_or(0);
            let value = if rec.replace {
                let last = lead.max(seen);
                let kept: Vec<&Played> = notes.into_iter().filter(|p| p.pass == last).collect();
                self.step(None, &kept, &kept, brush, t.kind.is_drum(), project.divisions[usize::from(pattern)], rec)
            } else {
                let latest: Vec<&Played> = notes.iter().copied().filter(|p| p.pass == lead).collect();
                let old = t.steps[usize::from(pattern)][usize::from(step)];
                self.step(old, &notes, &latest, brush, t.kind.is_drum(), project.divisions[usize::from(pattern)], rec)
            };
            let (track, pattern, step) = (track as u8, pattern, step);
            edits.push(Edit::SetStep { track, pattern, step, value });
        }
        edits
    }

    /// The step `old` (or a new one from `brush`) becomes with `notes`
    /// added, timed by `lead`.
    #[allow(clippy::too_many_arguments)]
    fn step(
        &self,
        old: Option<Step>,
        notes: &[&Played],
        lead: &[&Played],
        brush: Step,
        drum: bool,
        division: crate::engine::Division,
        rec: Recording,
    ) -> Option<Step> {
        let first = lead.iter().min_by(|a, b| a.on.total_cmp(&b.on))?;
        let mut s = old.unwrap_or(brush);
        s.offset = if rec.quantize { 0 } else { first.offset };
        s.velocity = lead.iter().map(|p| p.velocity).max().unwrap_or(s.velocity);
        let longest = lead.iter().map(|p| p.off.unwrap_or(self.last) - p.on).fold(0.0, f64::max);
        s.length = (longest / division.sixteenths()).round().clamp(1.0, MAX_STEPS as f64) as u8;
        if drum {
            s.chord = [0; MAX_CHORD - 1];
        } else {
            // The step's notes oldest first, so the newest are kept.
            let mut all: Vec<u8> = match old {
                Some(o) => std::iter::once(o.note)
                    .chain(o.chord.iter().filter(|&&i| i > 0).map(|&i| o.note.saturating_add(i)))
                    .collect(),
                None => Vec::new(),
            };
            let mut by_time: Vec<&&Played> = notes.iter().collect();
            by_time.sort_by(|a, b| a.on.total_cmp(&b.on));
            for p in by_time {
                all.retain(|&n| n != p.note);
                all.push(p.note);
            }
            let all = &all[all.len().saturating_sub(MAX_CHORD)..];
            let root = *all.iter().min()?;
            s.note = root;
            s.chord = [0; MAX_CHORD - 1];
            let mut ivs: Vec<u8> = all.iter().map(|&n| n - root).filter(|&i| i > 0).collect();
            ivs.sort_unstable();
            for (slot, i) in s.chord.iter_mut().zip(ivs) {
                *slot = i;
            }
        }
        Some(s.sanitized())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Division;
    use crate::instrument::InstrumentKind;

    fn on(note: u8, step: u8, offset: i8, early: bool, position: f64) -> Take {
        Take::On { track: 0, note, velocity: 90, pattern: 0, step, offset, early, position }
    }

    fn off(note: u8, position: f64) -> Take {
        Take::Off { note, position }
    }

    fn passed(step: u8) -> Take {
        Take::Passed { track: 0, pattern: 0, step }
    }

    fn project(kind: InstrumentKind) -> Project {
        let mut p = Project::default();
        p.apply(Edit::AddTrack(kind)).unwrap();
        p
    }

    fn take(events: &[Take]) -> NoteTake {
        let mut t = NoteTake::new();
        for &e in events {
            t.push(e);
        }
        t
    }

    const OVERDUB: Recording = Recording { replace: false, quantize: false, count_in: 0 };
    const REPLACE: Recording = Recording { replace: true, quantize: false, count_in: 0 };

    fn set(step: u8, value: Option<Step>) -> Edit {
        Edit::SetStep { track: 0, pattern: 0, step, value }
    }

    fn chord(note: u8, velocity: u8, length: u8, offset: i8, ivs: [u8; 3]) -> Step {
        Step { offset, chord: ivs, ..Step::new(note, velocity, length) }
    }

    #[test]
    fn notes_become_steps_with_their_timing_and_length() {
        let p = project(InstrumentKind::Synth);
        let brush = [Step::new(60, 100, 1)];
        // A note on step 1, a quarter late and three steps long; a chord on
        // step 2 whose notes start a tick apart.
        let t = take(&[
            passed(0),
            passed(1),
            on(62, 1, 6, false, 1.25),
            on(67, 2, -1, true, 1.96),
            on(64, 2, 0, true, 2.0),
            passed(2),
            Take::On {
                track: 0,
                note: 60,
                velocity: 110,
                pattern: 0,
                step: 2,
                offset: 1,
                early: false,
                position: 2.04,
            },
            off(67, 3.0),
            off(64, 3.1),
            off(60, 4.0),
            off(62, 4.2),
        ]);
        assert_eq!(
            t.edits(&p, &brush, OVERDUB),
            [set(1, Some(chord(62, 90, 3, 6, [0; 3]))), set(2, Some(chord(60, 110, 2, -1, [4, 7, 0]))),]
        );
        let quantized = Recording { quantize: true, ..OVERDUB };
        assert_eq!(t.edits(&p, &brush, quantized)[0], set(1, Some(chord(62, 90, 3, 0, [0; 3]))));
    }

    #[test]
    fn overdub_adds_to_a_step_and_replace_keeps_the_last_pass() {
        let mut p = project(InstrumentKind::Synth);
        p.apply(set(0, Some(chord(48, 70, 4, 0, [7, 0, 0])))).unwrap();
        p.apply(set(1, Some(Step::new(50, 70, 1)))).unwrap();
        let brush = [Step::new(60, 100, 1)];
        // Two passes over steps 0 and 1: the first plays 60 on step 0, the
        // second 64 on step 0 (early, before it plays) and nothing on 1.
        let t = take(&[
            passed(0),
            on(60, 0, 2, false, 0.1),
            off(60, 0.9),
            passed(1),
            on(64, 0, -3, true, 1.9),
            off(64, 2.9),
            passed(0),
            passed(1),
        ]);
        assert_eq!(t.edits(&p, &brush, OVERDUB), [set(0, Some(chord(48, 90, 1, -3, [7, 12, 16])))]);
        assert_eq!(t.edits(&p, &brush, REPLACE), [set(0, Some(chord(64, 90, 1, -3, [0; 3]))), set(1, None)]);
    }

    #[test]
    fn a_chord_keeps_its_newest_notes_and_drums_keep_their_sound() {
        let p = project(InstrumentKind::Synth);
        let brush = [Step::new(60, 100, 1)];
        let notes = [72, 60, 64, 67, 71];
        let events: Vec<Take> = notes.iter().enumerate().map(|(i, &n)| on(n, 0, 0, true, i as f64 * 0.01)).collect();
        let t = take(&events);
        assert_eq!(t.edits(&p, &brush, OVERDUB)[0], set(0, Some(chord(60, 90, 1, 0, [4, 7, 11]))));
        let p = project(InstrumentKind::Kick);
        let brush = [Step::new(36, 100, 1)];
        let t = take(&[on(50, 3, 0, true, 3.0), on(52, 3, 0, true, 3.0)]);
        assert_eq!(t.edits(&p, &brush, OVERDUB), [set(3, Some(Step::new(36, 90, 1)))]);
    }

    #[test]
    fn a_take_follows_its_tracks() {
        let mut p = project(InstrumentKind::Synth);
        p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
        let brushes = [Step::new(60, 100, 1); 2];
        let mut t = take(&[
            Take::On { track: 1, note: 62, velocity: 90, pattern: 0, step: 0, offset: 0, early: true, position: 0.0 },
            Take::Passed { track: 0, pattern: 0, step: 0 },
        ]);
        // Track 0 removed: track 1's notes move up, track 0's passes go.
        t.follow(|tr| tr.checked_sub(1));
        p.apply(Edit::RemoveTrack(0)).unwrap();
        t.push(off(62, 2.0));
        assert_eq!(t.edits(&p, &brushes, REPLACE), [set(0, Some(Step::new(62, 90, 2)))]);
        assert!(!t.is_empty());
        // A step division sets the length's steps.
        p.apply(Edit::SetPatternDivision { pattern: 0, division: Division::Eighth }).unwrap();
        assert_eq!(t.edits(&p, &brushes, REPLACE), [set(0, Some(Step::new(62, 90, 1)))]);
    }
}
