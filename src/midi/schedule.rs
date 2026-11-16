//! Sending MIDI on time. The engine stamps each message for the output with
//! the frame it is due on, and at the start of every audio callback sends an
//! anchor: which frames the callback renders and when it ran. The output
//! thread maps frames to wall-clock time from the anchors and sends each
//! message when its frame is heard.
//!
//! A callback runs about one buffer before its frames play, and can run late
//! but never early, so the earliest anchor (as frame time against wall time)
//! is the closest to the device's clock; the estimate may creep later slowly,
//! which follows the drift between the two clocks.

use super::Output;
use crate::rt::spsc::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What the audio thread sends the MIDI output thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutEvent {
    /// `len` bytes of `bytes`, due on engine frame `frame`.
    Message { frame: u64, bytes: [u8; 3], len: u8 },
    /// An audio callback for `frames` frames from `frame` started at `at`,
    /// at `rate` frames a second.
    Anchor { frame: u64, frames: u32, rate: f32, at: Instant },
    /// Messages before this one, due on `frame`, may have been lost to a
    /// full queue: every note still sounding is let go then, and the
    /// receiver is stopped unless the clock is `running`.
    Reset { frame: u64, running: bool },
}

impl OutEvent {
    /// A message of one to three bytes due on `frame`.
    pub fn message(frame: u64, msg: &[u8]) -> Self {
        let mut bytes = [0; 3];
        let len = msg.len().min(3);
        bytes[..len].copy_from_slice(&msg[..len]);
        OutEvent::Message { frame, bytes, len: len as u8 }
    }
}

/// How late a message may be sent: one due longer ago is dropped (a note
/// that would start late, a clock), except what must not be lost: the
/// release of a note sent, and start, stop, continue and song position.
pub const LATE: Duration = Duration::from_millis(50);
/// How long a message that must not be lost is retried when the output
/// refuses it; a release given up on is sent again when the output closes.
const GIVE_UP: Duration = Duration::from_secs(1);
/// How long without an anchor, beyond four of the longest callbacks seen,
/// means the engine is gone or stalled: every note still sounding is let go
/// then.
pub const STALL: Duration = Duration::from_millis(250);
/// How long closing an output waits for it to take the releases it owes.
const CLOSING: Duration = Duration::from_millis(200);
/// How fast the estimate of when frames are heard may move later, in
/// seconds per second: far more than two clocks drift apart, far less than
/// a late callback moves it.
const CREEP: f64 = 0.001;

#[derive(Clone, Copy, Debug)]
struct Timing {
    /// When frame 0 is heard, in seconds after `t0`.
    zero: f64,
    /// Seconds after `t0` of the last anchor.
    last: f64,
    rate: f64,
    /// The longest callback seen, in seconds.
    period: f64,
}

/// Note counts per channel and key.
type Keys = [[u8; 128]; 16];

/// Orders the messages from the engine by frame and says when each is due.
#[derive(Debug)]
pub struct Scheduler {
    /// (frame, bytes, length), in the order the engine queued them, and
    /// sent in that order: one due before a message queued earlier waits
    /// for it. When the limiter's latency falls, frames after it wait for
    /// those before (a millisecond and a half at most), so a release never
    /// goes before its note, Start before an earlier Stop, or a reset before
    /// what it lets go. Length 0 is a reset, with whether the clock runs in the first
    /// byte.
    pending: Vec<(u64, [u8; 3], u8)>,
    t0: Instant,
    timing: Option<Timing>,
    /// Added to every message's time.
    delay: Duration,
    /// How many times each key on each channel was started and not yet
    /// released: two tracks can play one key, and each release counts.
    sounding: Keys,
    /// Starts dropped (late or refused) whose releases are still to come:
    /// each skips the next release of its key, so another note on it goes
    /// on sounding until its own.
    skipped: Keys,
    /// Releases (and a Stop) the output refused while everything was let
    /// go, tried again until [`GIVE_UP`] after `owed_since`.
    owed: Keys,
    owe_stop: bool,
    owed_since: Option<Instant>,
    /// Start or Continue was sent and Stop has not been since. Clocks go
    /// only then, so an output opened while playing waits for the next Play.
    running: bool,
}

impl Scheduler {
    pub fn new(delay: Duration) -> Self {
        Self {
            pending: Vec::new(),
            t0: Instant::now(),
            timing: None,
            delay,
            sounding: [[0; 128]; 16],
            skipped: [[0; 128]; 16],
            owed: [[0; 128]; 16],
            owe_stop: false,
            owed_since: None,
            running: false,
        }
    }

    pub fn push(&mut self, ev: OutEvent) {
        let (frame, bytes, len) = match ev {
            OutEvent::Message { frame, bytes, len } => (frame, bytes, len.max(1)),
            OutEvent::Reset { frame, running } => (frame, [u8::from(running), 0, 0], 0),
            OutEvent::Anchor { frame, frames, rate, at } => return self.anchor(frame, frames, rate, at),
        };
        self.pending.push((frame, bytes, len));
    }

    /// `at` in seconds after `t0`, negative before it.
    fn secs(&self, at: Instant) -> f64 {
        match at.checked_duration_since(self.t0) {
            Some(d) => d.as_secs_f64(),
            None => -(self.t0 - at).as_secs_f64(),
        }
    }

    /// When `frame` is heard (after the delay), or `None` before the first
    /// anchor.
    fn heard(&self, frame: u64) -> Option<Instant> {
        let t = self.timing?;
        let secs = t.zero + frame as f64 / t.rate;
        let at = if secs >= 0.0 {
            self.t0 + Duration::from_secs_f64(secs)
        } else {
            self.t0.checked_sub(Duration::from_secs_f64(-secs)).unwrap_or(self.t0)
        };
        Some(at + self.delay)
    }

    fn anchor(&mut self, frame: u64, frames: u32, rate: f32, at: Instant) {
        let rate = f64::from(rate.max(1.0));
        let secs = self.secs(at);
        // The callback's frames play after the buffer playing now.
        let period = f64::from(frames) / rate;
        let zero = secs + period - frame as f64 / rate;
        self.timing = Some(match self.timing {
            Some(t) if t.rate == rate => Timing {
                zero: zero.min(t.zero + CREEP * (secs - t.last).max(0.0)),
                last: secs,
                rate,
                period: t.period.max(period),
            },
            _ => Timing { zero, last: secs, rate, period },
        });
    }

    /// When a message for `frame` is due, or `None` before the first
    /// anchor: never before the scheduler started.
    pub fn due(&self, frame: u64) -> Option<Instant> {
        self.heard(frame).map(|at| at.max(self.t0))
    }

    /// When the next message is due.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.first().and_then(|p| self.due(p.0))
    }

    /// Hands `send` every message due by `now`, in order; `send` says
    /// whether the output took it. Note-ons and clocks due more than
    /// [`LATE`] ago, or refused, are dropped. The rest must not be lost: one
    /// refused is tried again on the next call, for up to [`GIVE_UP`].
    pub fn send_due(&mut self, now: Instant, mut send: impl FnMut(&[u8]) -> bool) {
        let mut taken = 0;
        while let Some(&(frame, bytes, len)) = self.pending.get(taken) {
            let Some(due) = self.due(frame) else { break };
            if due > now {
                break;
            }
            let late = now.saturating_duration_since(due);
            if len == 0 {
                // A reset the output refused part of stays first in line.
                if !self.let_go(bytes[0] != 0, &mut send) && late <= GIVE_UP {
                    break;
                }
                taken += 1;
                continue;
            }
            let msg = &bytes[..usize::from(len)];
            let status = msg[0];
            let (channel, key) = (usize::from(status & 0x0F), usize::from(msg.get(1).map_or(0, |&k| k & 0x7F)));
            let kept = match status & 0xF0 {
                0x90 if msg.get(2).is_some_and(|&v| v > 0) => {
                    let n = if late <= LATE && send(msg) {
                        &mut self.sounding[channel][key]
                    } else {
                        &mut self.skipped[channel][key]
                    };
                    *n = n.saturating_add(1);
                    true
                }
                // A release of a note never sent is not sent either.
                0x80 | 0x90 if self.skipped[channel][key] > 0 => {
                    self.skipped[channel][key] -= 1;
                    true
                }
                0x80 | 0x90 if self.sounding[channel][key] == 0 => true,
                0x80 | 0x90 => {
                    let ok = send(msg);
                    if ok {
                        self.sounding[channel][key] -= 1;
                    }
                    ok
                }
                _ if status == 0xF8 => {
                    if self.running && late <= LATE {
                        send(msg);
                    }
                    true
                }
                _ => {
                    let ok = send(msg);
                    match status {
                        0xFA | 0xFB if ok => self.running = true,
                        0xFC if ok => self.running = false,
                        _ => {}
                    }
                    ok
                }
            };
            if !kept && late <= GIVE_UP {
                break;
            }
            taken += 1;
        }
        self.pending.drain(..taken);
    }

    /// Sends a release for every note still sounding and, while the clock
    /// runs and should not, Stop. Says whether the output took them all;
    /// what it refused is still counted.
    fn let_go(&mut self, running: bool, send: &mut impl FnMut(&[u8]) -> bool) -> bool {
        let mut done = release(&mut self.sounding, send);
        if self.running && !running {
            if send(&[0xFC]) {
                self.running = false;
            } else {
                done = false;
            }
        }
        done
    }

    /// Whether no anchor has come for [`STALL`] beyond four callbacks: the
    /// engine is gone, or stalled, and what it sent may never be released.
    pub fn stalled(&self, now: Instant) -> bool {
        self.timing.is_some_and(|t| self.secs(now) > t.last + 4.0 * t.period + STALL.as_secs_f64())
    }

    /// Releases every note sent and not yet released, stops the receiver,
    /// and forgets the messages still to come: the engine they came from is
    /// gone, or the output closes. What the output refuses is owed, and
    /// [`Scheduler::settle`] tries it again.
    pub fn release_all(&mut self, now: Instant, mut send: impl FnMut(&[u8]) -> bool) {
        for (owed, sounding) in self.owed.iter_mut().flatten().zip(self.sounding.iter().flatten()) {
            *owed = owed.saturating_add(*sounding);
        }
        self.sounding = [[0; 128]; 16];
        self.skipped = [[0; 128]; 16];
        self.owe_stop |= std::mem::take(&mut self.running);
        self.owed_since.get_or_insert(now);
        self.pending.clear();
        self.timing = None;
        self.settle(now, &mut send);
    }

    /// Sends what [`Scheduler::release_all`] owes, giving up [`GIVE_UP`]
    /// after it first owed something. Says whether nothing is owed now.
    pub fn settle(&mut self, now: Instant, mut send: impl FnMut(&[u8]) -> bool) -> bool {
        let Some(since) = self.owed_since else { return true };
        let mut done = release(&mut self.owed, &mut send);
        if self.owe_stop {
            if send(&[0xFC]) {
                self.owe_stop = false;
            } else {
                done = false;
            }
        }
        if done || now.saturating_duration_since(since) > GIVE_UP {
            self.owed = [[0; 128]; 16];
            self.owe_stop = false;
            self.owed_since = None;
            return true;
        }
        false
    }

    /// Takes the queue of a new engine, or the same engine's from another
    /// output, at `now`: what it holds that was due more than [`LATE`] ago
    /// is stale. So is what came with no anchor (after the last one another
    /// thread took), which cannot be placed in time: at most one callback's
    /// messages.
    pub fn take_backlog(&mut self, now: Instant, queue: &mut Consumer<OutEvent>) {
        while let Some(ev) = queue.pop() {
            self.push(ev);
        }
        let before = now.checked_sub(LATE).unwrap_or(now);
        let stale: Vec<bool> = self.pending.iter().map(|p| self.heard(p.0).is_none_or(|at| at < before)).collect();
        let mut i = 0;
        self.pending.retain(|_| {
            i += 1;
            !stale[i - 1]
        });
    }

    /// One look at what is due at `now`: everything let go if the engine
    /// has stalled, what that still owes, then the engine's messages, which
    /// wait until nothing is owed so an old release or Stop cannot follow
    /// the new engine's notes and Start.
    pub fn run(&mut self, now: Instant, mut send: impl FnMut(&[u8]) -> bool) {
        if self.stalled(now) {
            self.release_all(now, &mut send);
        }
        if self.settle(now, &mut send) {
            self.send_due(now, &mut send);
        }
    }
}

/// Sends a release for each count in `keys`, counting down those the output
/// takes. Says whether it took them all.
fn release(keys: &mut Keys, send: &mut impl FnMut(&[u8]) -> bool) -> bool {
    let mut done = true;
    for (channel, notes) in keys.iter_mut().enumerate() {
        for (key, n) in notes.iter_mut().enumerate() {
            while *n > 0 && send(&[0x80 | channel as u8, key as u8, 0]) {
                *n -= 1;
            }
            done &= *n == 0;
        }
    }
    done
}

/// Where the engine's queue of MIDI messages waits for the output thread:
/// a new engine puts its queue here, and the thread takes it.
pub type OutSlot = Arc<Mutex<Option<Consumer<OutEvent>>>>;

/// How long the output thread sleeps at most between looks at its queue.
const POLL: Duration = Duration::from_millis(1);

/// The thread that sends the engine's MIDI messages to an output, on time.
/// Dropping it releases every note it sent and gives the queue back.
pub struct OutputThread {
    name: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl OutputThread {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for OutputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts sending what the engine queues in `slot` to `output`, `delay`
/// later than its frames are heard.
pub fn start(mut output: Output, slot: OutSlot, delay: Duration) -> Result<OutputThread, String> {
    let name = output.name().to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("tinvo-midi-out".into())
        .spawn(move || {
            let mut sched = Scheduler::new(delay);
            let mut queue: Option<Consumer<OutEvent>> = None;
            let mut send = |m: &[u8]| output.send(m).is_ok();
            while !flag.load(Ordering::Acquire) {
                if let Some(mut fresh) = slot.try_lock().ok().and_then(|mut s| s.take()) {
                    // A new engine: what the old one queued will not be
                    // released.
                    let now = Instant::now();
                    sched.release_all(now, &mut send);
                    sched.take_backlog(now, &mut fresh);
                    queue = Some(fresh);
                }
                if let Some(q) = queue.as_mut() {
                    while let Some(ev) = q.pop() {
                        sched.push(ev);
                    }
                }
                let now = Instant::now();
                sched.run(now, &mut send);
                let wait = sched.next_due().map_or(POLL, |d| d.saturating_duration_since(now).min(POLL));
                std::thread::sleep(wait.max(Duration::from_micros(100)));
            }
            let closing = Instant::now();
            sched.release_all(closing, &mut send);
            while !sched.settle(Instant::now(), &mut send) && closing.elapsed() < CLOSING {
                std::thread::sleep(POLL);
            }
            // The next output thread takes the queue from here.
            if let (Some(q), Ok(mut s)) = (queue, slot.lock()) {
                if s.is_none() {
                    *s = Some(q);
                }
            }
        })
        .map_err(|e| format!("cannot start the MIDI output thread: {e}"))?;
    Ok(OutputThread { name, stop, thread: Some(thread) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(t0: Instant, frame: u64, ms: u64) -> OutEvent {
        OutEvent::Anchor { frame, frames: 480, rate: 48_000.0, at: t0 + Duration::from_millis(ms) }
    }

    fn sent(s: &mut Scheduler, now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        s.send_due(now, |m| {
            out.push(m.to_vec());
            true
        });
        out
    }

    fn released(s: &mut Scheduler) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        s.release_all(s.t0, |m| {
            out.push(m.to_vec());
            true
        });
        out
    }

    #[test]
    fn frames_are_heard_a_buffer_after_their_callback() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        assert_eq!(s.due(0), None, "no anchor yet");
        // Frames 4800.. rendered at 100 ms: frame 4800 plays at 110 ms.
        s.push(anchor(t0, 4800, 100));
        let ms = |i: Instant| (i - t0).as_secs_f64() * 1000.0;
        assert!((ms(s.due(4800).unwrap()) - 110.0).abs() < 1e-6);
        assert!((ms(s.due(9600).unwrap()) - 210.0).abs() < 1e-6);
        // A late callback does not move it; an early one does.
        s.push(anchor(t0, 9600, 205));
        assert!((ms(s.due(9600).unwrap()) - 210.0).abs() < 0.2, "{}", ms(s.due(9600).unwrap()));
        s.push(anchor(t0, 14_400, 297));
        assert!((ms(s.due(14_400).unwrap()) - 307.0).abs() < 1e-6);
        // A delay moves everything later.
        let mut d = Scheduler::new(Duration::from_millis(20));
        d.t0 = t0;
        d.push(anchor(t0, 4800, 100));
        assert!((ms(d.due(4800).unwrap()) - 130.0).abs() < 1e-6);
    }

    #[test]
    fn messages_go_in_order_when_due() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(OutEvent::message(0, &[0xFA]));
        s.push(OutEvent::message(480, &[0xF8]));
        s.push(OutEvent::message(960, &[0x90, 60, 100]));
        s.push(OutEvent::message(960, &[0xF8]));
        s.push(anchor(t0, 0, 0));
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(10)), [vec![0xFA]]);
        // Frame 0 plays at 10 ms, 480 at 20 ms, 960 at 30 ms.
        assert!(sent(&mut s, t0 + Duration::from_millis(19)).is_empty());
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(21)), [vec![0xF8]]);
        // On one frame, in the order the engine queued them.
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(31)), [vec![0x90, 60, 100], vec![0xF8]]);
        assert_eq!(s.next_due(), None);
    }

    #[test]
    fn late_notes_and_clocks_are_dropped_but_releases_and_transport_are_not() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        s.push(OutEvent::message(0, &[0xFA]));
        s.push(OutEvent::message(0, &[0x90, 60, 100]));
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(11)), [vec![0xFA], vec![0x90, 60, 100]]);
        for m in [[0x91, 62, 100], [0x80, 60, 0], [0x81, 62, 0]] {
            s.push(OutEvent::message(480, &m));
        }
        for m in [&[0xF8][..], &[0xFC], &[0xF2, 0, 1]] {
            s.push(OutEvent::message(480, m));
        }
        // 200 ms late: the note never sent is not started or released.
        let late = sent(&mut s, t0 + Duration::from_millis(220));
        assert_eq!(late, [vec![0x80, 60, 0], vec![0xFC], vec![0xF2, 0, 1]]);
    }

    #[test]
    fn releasing_all_ends_the_notes_still_sounding() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        for m in [[0x90, 60, 100], [0x9F, 127, 1], [0x90, 61, 100], [0x90, 61, 0]] {
            s.push(OutEvent::message(0, &m));
        }
        s.push(OutEvent::message(0, &[0xFB]));
        s.push(OutEvent::message(48_000, &[0x80, 60, 0]));
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(11)).len(), 5);
        assert_eq!(released(&mut s), [vec![0x80, 60, 0], vec![0x8F, 127, 0], vec![0xFC]]);
        assert_eq!((s.next_due(), s.due(0)), (None, None), "the rest is forgotten");
        assert!(released(&mut s).is_empty());
    }

    #[test]
    fn a_key_two_tracks_play_is_released_as_often_as_it_was_started() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        for m in [[0x90, 60, 100], [0x90, 60, 90], [0x80, 60, 0], [0x80, 60, 0], [0x80, 60, 0]] {
            s.push(OutEvent::message(0, &m));
        }
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(11)).len(), 4, "the third release has no note");
        s.push(OutEvent::message(0, &[0x90, 60, 100]));
        s.push(OutEvent::message(0, &[0x90, 60, 100]));
        sent(&mut s, t0 + Duration::from_millis(12));
        assert_eq!(released(&mut s), [vec![0x80, 60, 0], vec![0x80, 60, 0]]);
    }

    #[test]
    fn a_release_the_output_refuses_is_tried_again() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        s.push(OutEvent::message(0, &[0x90, 60, 100]));
        s.push(OutEvent::message(480, &[0x80, 60, 0]));
        s.push(OutEvent::message(480, &[0x90, 62, 100]));
        sent(&mut s, t0 + Duration::from_millis(11));
        // Full: the release waits, and what follows it with it.
        let mut tries = 0;
        s.send_due(t0 + Duration::from_millis(21), |_| {
            tries += 1;
            false
        });
        assert_eq!(tries, 1);
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(22)), [vec![0x80, 60, 0], vec![0x90, 62, 100]]);
        // Refused for longer than it is tried: given up, and sent on close.
        s.push(OutEvent::message(960, &[0x80, 62, 0]));
        s.send_due(t0 + Duration::from_millis(1100), |_| false);
        assert_eq!(s.next_due(), None);
        assert_eq!(released(&mut s), [vec![0x80, 62, 0]]);
    }

    #[test]
    fn a_reset_lets_every_note_go_and_stops_a_clock_that_should_not_run() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        for m in [&[0xFA][..], &[0x90, 60, 100], &[0xF8]] {
            s.push(OutEvent::message(0, m));
        }
        s.push(OutEvent::Reset { frame: 480, running: true });
        s.push(OutEvent::message(480, &[0xF8]));
        s.push(OutEvent::message(960, &[0x80, 60, 0]));
        s.push(OutEvent::Reset { frame: 960, running: false });
        s.push(OutEvent::message(960, &[0xF8]));
        let got = sent(&mut s, t0 + Duration::from_millis(31));
        assert_eq!(got, [vec![0xFA], vec![0x90, 60, 100], vec![0xF8], vec![0x80, 60, 0], vec![0xF8], vec![0xFC]]);
    }

    #[test]
    fn clocks_wait_for_start_or_continue() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        for m in [&[0xF8][..], &[0xF2, 4, 0], &[0xFB], &[0xF8], &[0xFC], &[0xF8]] {
            s.push(OutEvent::message(0, m));
        }
        let got = sent(&mut s, t0 + Duration::from_millis(11));
        assert_eq!(got, [vec![0xF2, 4, 0], vec![0xFB], vec![0xF8], vec![0xFC]]);
    }

    #[test]
    fn a_dropped_start_skips_its_release_and_not_another_notes() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        s.push(OutEvent::message(0, &[0x90, 60, 100]));
        sent(&mut s, t0 + Duration::from_millis(11));
        // A second C4, dropped as late, and its release.
        s.push(OutEvent::message(480, &[0x90, 60, 90]));
        s.push(OutEvent::message(480, &[0x80, 60, 0]));
        assert!(sent(&mut s, t0 + Duration::from_millis(100)).is_empty());
        s.push(OutEvent::message(4800, &[0x80, 60, 0]));
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(111)), [vec![0x80, 60, 0]], "the first ends on its own");
    }

    #[test]
    fn a_reset_or_a_release_of_everything_the_output_refuses_is_tried_again() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        for m in [&[0xFA][..], &[0x90, 60, 100], &[0x90, 61, 100]] {
            s.push(OutEvent::message(0, m));
        }
        sent(&mut s, t0 + Duration::from_millis(11));
        s.push(OutEvent::Reset { frame: 480, running: false });
        s.push(OutEvent::message(480, &[0x90, 70, 100]));
        // The output takes one release, then nothing: the reset waits.
        let mut took = 0;
        s.send_due(t0 + Duration::from_millis(21), |_| {
            took += 1;
            took == 1
        });
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(22)), [vec![0x80, 61, 0], vec![0xFC], vec![0x90, 70, 100]]);
        // Everything let go while the output refuses: owed, and settled.
        s.push(anchor(t0, 480, 10));
        s.push(OutEvent::message(480, &[0xFB]));
        sent(&mut s, t0 + Duration::from_millis(30));
        s.release_all(t0 + Duration::from_millis(30), |_| false);
        assert!(!s.settle(t0 + Duration::from_millis(31), |_| false));
        let mut out = Vec::new();
        assert!(s.settle(t0 + Duration::from_millis(32), |m| {
            out.push(m.to_vec());
            true
        }));
        assert_eq!(out, [vec![0x80, 70, 0], vec![0xFC]]);
        // Given up after a while.
        s.push(anchor(t0, 960, 40));
        s.push(OutEvent::message(960, &[0x90, 72, 100]));
        sent(&mut s, t0 + Duration::from_millis(60));
        s.release_all(t0 + Duration::from_millis(60), |_| false);
        assert!(s.settle(t0 + Duration::from_millis(1100), |_| false));
        assert!(s.settle(t0 + Duration::from_millis(1101), |_| panic!("nothing is owed")));
    }

    #[test]
    fn a_message_never_goes_before_one_queued_earlier() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        // Queued 77 frames late under the limiter, then the limiter went: a
        // release, a Stop and Start, a reset, each after what came before.
        s.push(OutEvent::message(0, &[0xFA]));
        s.push(OutEvent::message(77, &[0x90, 60, 100]));
        s.push(OutEvent::message(64, &[0x80, 60, 0]));
        s.push(OutEvent::message(77, &[0x90, 62, 100]));
        s.push(OutEvent::message(77, &[0xFC]));
        s.push(OutEvent::message(64, &[0xFA]));
        s.push(OutEvent::message(64, &[0xF8]));
        s.push(OutEvent::Reset { frame: 64, running: true });
        s.push(OutEvent::message(64, &[0x90, 64, 100]));
        let got = sent(&mut s, t0 + Duration::from_millis(20));
        let want = [
            vec![0xFA],
            vec![0x90, 60, 100],
            vec![0x80, 60, 0],
            vec![0x90, 62, 100],
            vec![0xFC],
            vec![0xFA],
            vec![0xF8],
            vec![0x80, 62, 0],
            vec![0x90, 64, 100],
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn what_cleanup_owes_goes_before_the_next_engine_sends() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 0, 0));
        s.push(OutEvent::message(0, &[0xFA]));
        s.push(OutEvent::message(0, &[0x90, 60, 100]));
        sent(&mut s, t0 + Duration::from_millis(11));
        s.release_all(t0 + Duration::from_millis(12), |_| false);
        s.push(anchor(t0, 0, 20));
        s.push(OutEvent::message(0, &[0xFA]));
        s.push(OutEvent::message(0, &[0x90, 60, 100]));
        let mut out = Vec::new();
        s.run(t0 + Duration::from_millis(31), |m| {
            out.push(m.to_vec());
            false
        });
        assert_eq!(out, [vec![0x80, 60, 0], vec![0xFC]], "only the old cleanup is tried");
        out.clear();
        s.run(t0 + Duration::from_millis(32), |m| {
            out.push(m.to_vec());
            true
        });
        assert_eq!(out, [vec![0x80, 60, 0], vec![0xFC], vec![0xFA], vec![0x90, 60, 100]]);
    }

    #[test]
    fn a_new_engine_counting_from_frame_0_is_not_held_back() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(anchor(t0, 480_000, 0));
        s.push(OutEvent::message(480_000, &[0x90, 60, 100]));
        sent(&mut s, t0 + Duration::from_millis(11));
        released(&mut s);
        s.push(anchor(t0, 0, 20));
        s.push(OutEvent::message(0, &[0x90, 60, 100]));
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(31)), [vec![0x90, 60, 100]]);
    }

    #[test]
    fn a_new_queue_keeps_only_what_is_fresh() {
        // With no anchor, nothing can be placed in time.
        let mut s = Scheduler::new(Duration::ZERO);
        let (mut tx, mut rx) = crate::rt::spsc::channel(16);
        tx.push(OutEvent::message(0, &[0xFA])).unwrap();
        s.take_backlog(s.t0, &mut rx);
        assert!(s.pending.is_empty());
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        // Queued long ago, then Play just before the queue is taken.
        tx.push(anchor(t0, 0, 0)).unwrap();
        tx.push(OutEvent::message(0, &[0xFA])).unwrap();
        tx.push(OutEvent::message(480, &[0xFC])).unwrap();
        tx.push(anchor(t0, 480_000, 10_000)).unwrap();
        tx.push(OutEvent::message(480_000, &[0xFA])).unwrap();
        s.take_backlog(t0 + Duration::from_millis(10_001), &mut rx);
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(10_011)), [vec![0xFA]]);
        // Stale too from callbacks before this scheduler started.
        let early = Instant::now();
        std::thread::sleep(Duration::from_millis(30));
        let mut s = Scheduler::new(Duration::ZERO);
        for ev in [
            OutEvent::Anchor { frame: 0, frames: 480, rate: 48_000.0, at: early },
            OutEvent::message(0, &[0xFA]),
            OutEvent::message(0, &[0xFC]),
        ] {
            tx.push(ev).unwrap();
        }
        s.take_backlog(early + Duration::from_millis(70), &mut rx);
        s.push(OutEvent::message(1_000_000, &[0xFA]));
        assert_eq!(s.pending.len(), 1, "stale, though from before the scheduler");
        // With a delay: heard 70 ms ago is due 20 ms ago, and kept.
        let mut s = Scheduler::new(Duration::from_millis(50));
        let t0 = s.t0;
        tx.push(OutEvent::Anchor { frame: 0, frames: 480, rate: 48_000.0, at: t0 }).unwrap();
        tx.push(OutEvent::message(0, &[0xFA])).unwrap();
        s.take_backlog(t0 + Duration::from_millis(80), &mut rx);
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(80)), [vec![0xFA]]);
    }

    #[test]
    fn an_engine_silent_for_a_while_has_stalled() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        assert!(!s.stalled(t0 + Duration::from_secs(5)), "no engine yet");
        s.push(anchor(t0, 0, 100));
        // Four 10 ms callbacks and 250 ms after the last.
        assert!(!s.stalled(t0 + Duration::from_millis(385)));
        assert!(s.stalled(t0 + Duration::from_millis(395)));
        s.push(anchor(t0, 480, 350));
        assert!(!s.stalled(t0 + Duration::from_millis(395)));
        // Callbacks of 16,384 frames come 341 ms apart and are not a stall.
        let mut big = Scheduler::new(Duration::ZERO);
        let t0 = big.t0;
        big.push(OutEvent::Anchor { frame: 0, frames: 16_384, rate: 48_000.0, at: t0 });
        assert!(!big.stalled(t0 + Duration::from_millis(1500)));
        assert!(big.stalled(t0 + Duration::from_millis(1700)));
    }
}
