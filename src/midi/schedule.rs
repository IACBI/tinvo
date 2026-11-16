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
}

/// Orders the messages from the engine by frame and says when each is due.
#[derive(Debug)]
pub struct Scheduler {
    /// (frame, arrival, bytes, length), sorted.
    pending: Vec<(u64, u64, [u8; 3], u8)>,
    arrivals: u64,
    t0: Instant,
    timing: Option<Timing>,
    /// Added to every message's time.
    delay: Duration,
    /// Notes sent and not yet released, a bit per key on each channel.
    sounding: [u128; 16],
}

impl Scheduler {
    pub fn new(delay: Duration) -> Self {
        Self { pending: Vec::new(), arrivals: 0, t0: Instant::now(), timing: None, delay, sounding: [0; 16] }
    }

    pub fn push(&mut self, ev: OutEvent) {
        match ev {
            OutEvent::Message { frame, bytes, len } => {
                let key = (frame, self.arrivals);
                self.arrivals += 1;
                let at = self.pending.partition_point(|p| (p.0, p.1) <= key);
                self.pending.insert(at, (frame, key.1, bytes, len));
            }
            OutEvent::Anchor { frame, frames, rate, at } => {
                let rate = f64::from(rate.max(1.0));
                let secs = at.saturating_duration_since(self.t0).as_secs_f64();
                // The callback's frames play after the buffer playing now.
                let zero = secs + f64::from(frames) / rate - frame as f64 / rate;
                self.timing = Some(match self.timing {
                    Some(t) if t.rate == rate => {
                        Timing { zero: zero.min(t.zero + CREEP * (secs - t.last).max(0.0)), last: secs, rate }
                    }
                    _ => Timing { zero, last: secs, rate },
                });
            }
        }
    }

    /// When `frame` is heard, or `None` before the first anchor.
    pub fn due(&self, frame: u64) -> Option<Instant> {
        let t = self.timing?;
        let secs = (t.zero + frame as f64 / t.rate).max(0.0);
        Some(self.t0 + Duration::from_secs_f64(secs) + self.delay)
    }

    /// When the next message is due.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.first().and_then(|p| self.due(p.0))
    }

    /// Hands `send` every message due by `now`, in order. Those due more
    /// than [`LATE`] ago are dropped unless they must not be lost.
    pub fn send_due(&mut self, now: Instant, mut send: impl FnMut(&[u8])) {
        let mut taken = 0;
        for &(frame, _, bytes, len) in &self.pending {
            let Some(due) = self.due(frame) else { break };
            if due > now {
                break;
            }
            taken += 1;
            let msg = &bytes[..usize::from(len)];
            let late = now.saturating_duration_since(due) > LATE;
            let status = msg.first().copied().unwrap_or(0);
            let channel = usize::from(status & 0x0F);
            let key = msg.get(1).map_or(0, |&k| 1u128 << (k & 0x7F));
            match status & 0xF0 {
                0x90 if msg.get(2).is_some_and(|&v| v > 0) => {
                    if late {
                        continue;
                    }
                    self.sounding[channel] |= key;
                }
                0x80 | 0x90 => {
                    if self.sounding[channel] & key == 0 {
                        continue;
                    }
                    self.sounding[channel] &= !key;
                }
                _ if late && status == 0xF8 => continue,
                _ => {}
            }
            send(msg);
        }
        self.pending.drain(..taken);
    }

    /// Releases every note sent and not yet released, and forgets the
    /// messages still to come: the engine they came from is gone, or the
    /// output closes.
    pub fn release_all(&mut self, mut send: impl FnMut(&[u8])) {
        for (channel, keys) in self.sounding.iter_mut().enumerate() {
            for key in (0..128u8).filter(|&k| *keys & 1 << k != 0) {
                send(&[0x80 | channel as u8, key, 0]);
            }
            *keys = 0;
        }
        self.pending.clear();
        self.timing = None;
    }
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
            let mut send = |m: &[u8]| {
                // A failed send is not retried: the next messages may go
                // through, and a lost release is made up for on close.
                let _ = output.send(m);
            };
            while !flag.load(Ordering::Acquire) {
                if let Some(mut fresh) = slot.try_lock().ok().and_then(|mut s| s.take()) {
                    // A new engine: what the old one queued will not be
                    // released, and what the new one queued before this
                    // thread looked is stale.
                    sched.release_all(&mut send);
                    while fresh.pop().is_some() {}
                    queue = Some(fresh);
                }
                if let Some(q) = queue.as_mut() {
                    while let Some(ev) = q.pop() {
                        sched.push(ev);
                    }
                }
                let now = Instant::now();
                sched.send_due(now, &mut send);
                let wait = sched.next_due().map_or(POLL, |d| d.saturating_duration_since(now).min(POLL));
                std::thread::sleep(wait.max(Duration::from_micros(100)));
            }
            sched.release_all(&mut send);
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
        s.send_due(now, |m| out.push(m.to_vec()));
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
    fn messages_go_in_frame_order_when_due() {
        let mut s = Scheduler::new(Duration::ZERO);
        let t0 = s.t0;
        s.push(OutEvent::message(960, &[0x90, 60, 100]));
        s.push(OutEvent::message(480, &[0xF8]));
        s.push(OutEvent::message(960, &[0xF8]));
        s.push(anchor(t0, 0, 0));
        // Frame 0 plays at 10 ms, 480 at 20 ms, 960 at 30 ms.
        assert!(((s.next_due().unwrap() - t0).as_secs_f64() - 0.020).abs() < 1e-6);
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
        s.push(OutEvent::message(0, &[0x90, 60, 100]));
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(11)), [vec![0x90, 60, 100]]);
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
        s.push(OutEvent::message(48_000, &[0x80, 60, 0]));
        assert_eq!(sent(&mut s, t0 + Duration::from_millis(11)).len(), 4);
        let mut out = Vec::new();
        s.release_all(|m| out.push(m.to_vec()));
        assert_eq!(out, [vec![0x80, 60, 0], vec![0x8F, 127, 0]]);
        assert_eq!((s.next_due(), s.due(0)), (None, None), "the rest is forgotten");
    }
}
