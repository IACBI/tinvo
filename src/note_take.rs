//! Notes recorded while the pattern plays, turned into step edits.
//!
//! The engine reports every key pressed on the live track at its nearest
//! step, its length when it ends, and every step the live track plays
//! ([`Take`]). A [`NoteTake`] collects those reports and, when recording
//! stops, writes them as [`Edit::SetStep`]s: one step per (track, pattern,
//! step), whatever was played on it becoming a chord.

use crate::engine::{Edit, MAX_CHORD, MAX_PATTERNS, MAX_STEPS, Step, Take};
use crate::project::Project;
use crate::settings::Recording;
use std::collections::{BTreeMap, HashMap, VecDeque};

/// A key pressed while recording.
#[derive(Clone, Copy, Debug)]
struct Played {
    track: usize,
    channel: u8,
    pattern: u8,
    step: u8,
    note: u8,
    velocity: u8,
    offset: i8,
    /// Which pass over its step the note belongs to (the first is 1).
    pass: u32,
    on: f64,
    /// In steps, once the key has ended.
    length: Option<u8>,
}

/// The most keys one take records: past it, keys pressed are left out
/// (and counted), so a source that never stops cannot use up memory. The
/// passes played then are not counted either: replacing would clear the
/// steps with the notes a full take could not keep.
pub const MAX_TAKE_NOTES: usize = 100_000;

/// The notes of one recording, from [`Take`] events.
#[derive(Debug, Default)]
pub struct NoteTake {
    played: Vec<Played>,
    /// Keys down, as indices into `played`.
    held: Vec<usize>,
    /// How many times each (track, pattern, step) has played.
    passes: HashMap<(usize, u8, u8), u32>,
    /// Track insertions and removals made since recording started whose
    /// [`Take::Layout`] has not come yet: reports before it still name the
    /// tracks as they were.
    pending: VecDeque<Edit>,
    /// Keys pressed once the take held [`MAX_TAKE_NOTES`].
    left_out: usize,
    /// It held [`MAX_TAKE_NOTES`] at some point: it stays full when a track
    /// removed takes notes away, so a later pass cannot replace them.
    full: bool,
}

impl NoteTake {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, take: Take) {
        match take {
            Take::On { .. } if self.full => self.left_out += 1,
            Take::On { track, channel, note, velocity, pattern, step, offset, early, position } => {
                let track = usize::from(track);
                let seen = self.passes.get(&(track, pattern, step)).copied().unwrap_or(0);
                // A key whose step has not played yet belongs to the pass
                // that is coming.
                let pass = seen + u32::from(early);
                self.release(channel, note, 1);
                self.held.push(self.played.len());
                let (on, length) = (position, None);
                self.played.push(Played { track, channel, pattern, step, note, velocity, offset, pass, on, length });
                self.full = self.played.len() >= MAX_TAKE_NOTES;
            }
            Take::Off { channel, note, length } => self.release(channel, note, length),
            Take::Passed { .. } if self.full => {}
            Take::Passed { track, pattern, step } => {
                *self.passes.entry((usize::from(track), pattern, step)).or_insert(0) += 1;
            }
            Take::Layout => {
                if let Some(e) = self.pending.pop_front() {
                    self.follow(&e);
                }
            }
        }
    }

    fn release(&mut self, channel: u8, note: u8, length: u8) {
        let key = |p: &Played| p.channel == channel && p.note == note;
        if let Some(i) = self.held.iter().position(|&i| key(&self.played[i])) {
            let p = self.held.swap_remove(i);
            self.played[p].length = Some(length);
        }
    }

    /// Notes an edit applied to the project while recording: one that
    /// inserts or removes a track moves the take's tracks once the engine
    /// reports it has applied it too.
    pub fn edited(&mut self, e: &Edit) {
        if matches!(e, Edit::InsertTrack { .. } | Edit::RemoveTrack(_)) {
            self.pending.push_back(*e);
        }
    }

    /// Moves the take's tracks through the edits the engine applied after
    /// it stopped recording; call once every report has come.
    pub fn settle(&mut self) {
        while let Some(e) = self.pending.pop_front() {
            self.follow(&e);
        }
    }

    fn follow(&mut self, e: &Edit) {
        let mut kept = Vec::with_capacity(self.played.len());
        let mut index = vec![None; self.played.len()];
        for (i, p) in self.played.iter().enumerate() {
            if let Some(track) = follow_track(Some(p.track), e) {
                index[i] = Some(kept.len());
                kept.push(Played { track, ..*p });
            }
        }
        self.played = kept;
        self.held = self.held.iter().filter_map(|&i| index[i]).collect();
        self.passes =
            self.passes.drain().filter_map(|((t, p, s), n)| Some(((follow_track(Some(t), e)?, p, s), n))).collect();
    }

    /// How many keys were pressed once the take was full.
    pub fn left_out(&self) -> usize {
        self.left_out
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
    /// its longest note's length.
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
            let drum = t.kind.is_drum();
            let value = if rec.replace {
                let last = lead.max(seen);
                let kept: Vec<&Played> = notes.into_iter().filter(|p| p.pass == last).collect();
                step_with(None, &kept, &kept, brush, drum, rec)
            } else {
                let latest: Vec<&Played> = notes.iter().copied().filter(|p| p.pass == lead).collect();
                step_with(t.steps[usize::from(pattern)][usize::from(step)], &notes, &latest, brush, drum, rec)
            };
            edits.push(Edit::SetStep { track: track as u8, pattern, step, value });
        }
        edits
    }
}

/// The step `old` (or a new one from `brush`) becomes with `notes` added,
/// timed by `lead`.
fn step_with(
    old: Option<Step>,
    notes: &[&Played],
    lead: &[&Played],
    brush: Step,
    drum: bool,
    rec: Recording,
) -> Option<Step> {
    let first = lead.iter().min_by(|a, b| a.on.total_cmp(&b.on))?;
    let mut s = old.unwrap_or(brush);
    s.offset = if rec.quantize { 0 } else { first.offset };
    s.velocity = lead.iter().map(|p| p.velocity).max().unwrap_or(s.velocity);
    s.length = lead.iter().map(|p| p.length.unwrap_or(1)).max().unwrap_or(1);
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

/// Where track `track` is after `e` (`None` once it is removed).
pub fn follow_track(track: Option<usize>, e: &Edit) -> Option<usize> {
    let t = track?;
    match *e {
        Edit::InsertTrack { index, .. } if usize::from(index) <= t => Some(t + 1),
        Edit::RemoveTrack(r) if usize::from(r) == t => None,
        Edit::RemoveTrack(r) if usize::from(r) < t => Some(t - 1),
        _ => Some(t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::InstrumentKind;

    fn on(note: u8, step: u8, offset: i8, early: bool, position: f64) -> Take {
        Take::On { track: 0, channel: 0, note, velocity: 90, pattern: 0, step, offset, early, position }
    }

    fn off(note: u8, length: u8) -> Take {
        Take::Off { channel: 0, note, length }
    }

    #[test]
    fn a_take_records_at_most_its_limit_of_keys() {
        let mut t = NoteTake::new();
        for i in 0..MAX_TAKE_NOTES + 5 {
            let note = (i % 128) as u8;
            t.push(on(note, 0, 0, false, i as f64));
            t.push(off(note, 1));
        }
        assert_eq!((t.played.len(), t.left_out()), (MAX_TAKE_NOTES, 5));
        assert!(t.held.is_empty(), "keys recorded are still released");
        // A pass played once it is full does not replace what it kept, nor
        // once a track removed has taken most of its notes away.
        let mut t = take(&[passed(0), on(60, 0, 0, false, 0.0), off(60, 1)]);
        for i in 1..MAX_TAKE_NOTES {
            let position = i as f64;
            t.push(Take::On {
                track: 1,
                channel: 1,
                note: 64,
                velocity: 90,
                pattern: 0,
                step: 1,
                offset: 0,
                early: false,
                position,
            });
            t.push(Take::Off { channel: 1, note: 64, length: 1 });
        }
        let p = project(InstrumentKind::Synth);
        let brush = [Step::new(60, 100, 1)];
        let kept = [set(0, Some(Step { velocity: 90, ..Step::new(60, 100, 1) }))];
        t.push(passed(0));
        t.edited(&Edit::RemoveTrack(1));
        t.push(Take::Layout);
        assert_eq!(t.played.len(), 1);
        t.push(passed(0));
        t.push(on(62, 0, 0, false, 9.0e5));
        assert_eq!(t.edits(&p, &brush, REPLACE), kept);
        assert_eq!(t.left_out(), 1);
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
                channel: 0,
                note: 60,
                velocity: 110,
                pattern: 0,
                step: 2,
                offset: 1,
                early: false,
                position: 2.04,
            },
            off(67, 1),
            off(64, 1),
            off(60, 2),
            off(62, 3),
        ]);
        assert_eq!(
            t.edits(&p, &brush, OVERDUB),
            [set(1, Some(chord(62, 90, 3, 6, [0; 3]))), set(2, Some(chord(60, 110, 2, -1, [4, 7, 0]))),]
        );
        let quantized = Recording { quantize: true, ..OVERDUB };
        assert_eq!(t.edits(&p, &brush, quantized)[0], set(1, Some(chord(62, 90, 3, 0, [0; 3]))));
    }

    #[test]
    fn the_same_pitch_on_two_channels_is_two_keys() {
        let p = project(InstrumentKind::Synth);
        let brush = [Step::new(60, 100, 1)];
        let on_channel = |channel, step| match on(60, step, 0, true, f64::from(step)) {
            Take::On { track, note, velocity, pattern, step, offset, early, position, .. } => {
                Take::On { track, channel, note, velocity, pattern, step, offset, early, position }
            }
            t => t,
        };
        let off_channel = |channel, length| Take::Off { channel, note: 60, length };
        let t = take(&[on_channel(0, 1), on_channel(1, 2), off_channel(0, 3), off_channel(1, 1)]);
        assert_eq!(
            t.edits(&p, &brush, OVERDUB),
            [set(1, Some(Step::new(60, 90, 3))), set(2, Some(Step::new(60, 90, 1)))]
        );
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
            off(60, 1),
            passed(1),
            on(64, 0, -3, true, 1.9),
            off(64, 1),
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
    fn a_take_follows_its_tracks_when_the_engine_does() {
        let mut p = project(InstrumentKind::Synth);
        p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
        let brushes = [Step::new(60, 100, 1); 2];
        let on_track = |track, note, step| Take::On {
            track,
            channel: 0,
            note,
            velocity: 90,
            pattern: 0,
            step,
            offset: 0,
            early: true,
            position: f64::from(step),
        };
        let mut t = take(&[on_track(1, 62, 0), Take::Passed { track: 0, pattern: 0, step: 0 }]);
        // Track 0 is removed. A report the engine made before it applied the
        // removal still names track 1; the ones after its marker, track 0.
        let remove = Edit::RemoveTrack(0);
        p.apply(remove).unwrap();
        t.edited(&remove);
        t.push(on_track(1, 64, 1));
        t.push(Take::Layout);
        t.push(on_track(0, 65, 2));
        for (note, length) in [(62, 2), (64, 1), (65, 1)] {
            t.push(off(note, length));
        }
        let want = [
            set(0, Some(Step::new(62, 90, 2))),
            set(1, Some(Step::new(64, 90, 1))),
            set(2, Some(Step::new(65, 90, 1))),
        ];
        assert_eq!(t.edits(&p, &brushes, REPLACE), want);
        assert!(!t.is_empty());
        // A track inserted after recording stopped has no marker: settling
        // moves the take once every report is in.
        let insert = Edit::InsertTrack { index: 0, kind: InstrumentKind::Kick };
        p.apply(insert).unwrap();
        t.edited(&insert);
        t.settle();
        let moved: Vec<Edit> = want
            .iter()
            .map(|e| match *e {
                Edit::SetStep { pattern, step, value, .. } => Edit::SetStep { track: 1, pattern, step, value },
                e => e,
            })
            .collect();
        assert_eq!(t.edits(&p, &brushes, REPLACE), moved);
    }
}
