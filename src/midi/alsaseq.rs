//! Linux MIDI input and output through the ALSA sequencer.
//!
//! The sequencer sees everything rawmidi does (the kernel's `snd-seq-midi`
//! puts every hardware port on it) and also software clients: virtual
//! keyboards, DAWs, `aseqdump`'s peers. For input TINVO creates one writable
//! port named `TINVO:input`, subscribes it to the chosen port and decodes
//! note events on a thread that polls the sequencer. For output it creates a
//! readable port `TINVO:output`, connects it to the chosen port and sends
//! each message as an event delivered at once.
//!
//! Ports are identified by `seq:` followed by the client's and port's names,
//! which survive restarts of software clients, unlike their numbers.

use super::{Message, Port, decode};
use crate::audio::alsa::alsa_error;
use std::ffi::{CStr, c_char, c_int, c_short, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub const PREFIX: &str = "seq:";
const DEVICE: &str = "/dev/snd/seq";

const SND_SEQ_OPEN_OUTPUT: c_int = 1;
const SND_SEQ_OPEN_INPUT: c_int = 2;
const SND_SEQ_NONBLOCK: c_int = 0x0001;
const CAP_READ: c_uint = 1 << 0;
const CAP_WRITE: c_uint = 1 << 1;
const CAP_SUBS_READ: c_uint = 1 << 5;
const CAP_SUBS_WRITE: c_uint = 1 << 6;
const CAP_NO_EXPORT: c_uint = 1 << 7;
const TYPE_MIDI_GENERIC: c_uint = 1 << 1;
const TYPE_APPLICATION: c_uint = 1 << 20;
const CLIENT_SYSTEM: c_int = 0;
const EVENT_NOTEON: u8 = 6;
const EVENT_NOTEOFF: u8 = 7;
const EVENT_CONTROLLER: u8 = 10;
const EVENT_PITCHBEND: u8 = 13;
const EVENT_SONGPOS: u8 = 20;
const EVENT_START: u8 = 30;
const EVENT_CONTINUE: u8 = 31;
const EVENT_STOP: u8 = 32;
const EVENT_CLOCK: u8 = 36;
/// A subscription of the port's ended: its source went (closed or
/// unplugged), and the keys held on it will never be released.
const EVENT_PORT_UNSUBSCRIBED: u8 = 67;
const QUEUE_DIRECT: u8 = 253;
const ADDRESS_SUBSCRIBERS: u8 = 254;
const ADDRESS_UNKNOWN: u8 = 253;
const POLLIN: c_short = 1;
const EAGAIN: c_int = 11;
const ENOSPC: c_int = 28;
const MAX_FDS: usize = 4;
/// How often the thread looks at the stop flag while no events arrive.
const POLL_MS: c_int = 50;

/// `snd_seq_event_t` from `<alsa/seq_event.h>`: 28 bytes, 4-aligned, with
/// the event data union at byte 16. For note events the data starts with
/// channel, note and velocity.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Event {
    pub kind: u8,
    pub flags: u8,
    pub tag: u8,
    pub queue: u8,
    pub time: [u32; 2],
    pub source: [u8; 2],
    pub dest: [u8; 2],
    pub data: [u8; 12],
}

const _: () = assert!(size_of::<Event>() == 28 && align_of::<Event>() == 4);

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

#[link(name = "asound")]
unsafe extern "C" {
    fn snd_seq_open(handle: *mut *mut c_void, name: *const c_char, streams: c_int, mode: c_int) -> c_int;
    fn snd_seq_close(handle: *mut c_void) -> c_int;
    fn snd_seq_client_id(handle: *mut c_void) -> c_int;
    fn snd_seq_set_client_name(handle: *mut c_void, name: *const c_char) -> c_int;
    fn snd_seq_create_simple_port(handle: *mut c_void, name: *const c_char, caps: c_uint, kind: c_uint) -> c_int;
    fn snd_seq_connect_from(handle: *mut c_void, my_port: c_int, client: c_int, port: c_int) -> c_int;
    fn snd_seq_connect_to(handle: *mut c_void, my_port: c_int, client: c_int, port: c_int) -> c_int;
    fn snd_seq_event_output_direct(handle: *mut c_void, ev: *mut Event) -> c_int;
    fn snd_seq_poll_descriptors(handle: *mut c_void, pfds: *mut PollFd, space: c_uint, events: c_short) -> c_int;
    fn snd_seq_event_input(handle: *mut c_void, ev: *mut *mut Event) -> c_int;
    fn snd_seq_client_info_malloc(info: *mut *mut c_void) -> c_int;
    fn snd_seq_client_info_free(info: *mut c_void);
    fn snd_seq_client_info_set_client(info: *mut c_void, client: c_int);
    fn snd_seq_client_info_get_client(info: *const c_void) -> c_int;
    fn snd_seq_client_info_get_name(info: *mut c_void) -> *const c_char;
    fn snd_seq_query_next_client(handle: *mut c_void, info: *mut c_void) -> c_int;
    fn snd_seq_port_info_malloc(info: *mut *mut c_void) -> c_int;
    fn snd_seq_port_info_free(info: *mut c_void);
    fn snd_seq_port_info_set_client(info: *mut c_void, client: c_int);
    fn snd_seq_port_info_set_port(info: *mut c_void, port: c_int);
    fn snd_seq_port_info_get_port(info: *const c_void) -> c_int;
    fn snd_seq_port_info_get_name(info: *const c_void) -> *const c_char;
    fn snd_seq_port_info_get_capability(info: *const c_void) -> c_uint;
    fn snd_seq_query_next_port(handle: *mut c_void, info: *mut c_void) -> c_int;
}

unsafe extern "C" {
    fn poll(fds: *mut PollFd, n: c_ulong, timeout: c_int) -> c_int;
}

/// An open sequencer client, closed on drop.
struct Seq(*mut c_void);

// SAFETY: the handle is used by one thread at a time (moved into the reader).
unsafe impl Send for Seq {}

impl Drop for Seq {
    fn drop(&mut self) {
        // SAFETY: an open handle, closed once.
        unsafe { snd_seq_close(self.0) };
    }
}

impl Seq {
    fn open(streams: c_int) -> Result<Self, String> {
        // libasound would print its own complaint to stderr, over the editor.
        if !std::path::Path::new(DEVICE).exists() {
            return Err(format!("no ALSA sequencer ({DEVICE} is missing; the snd-seq module is not loaded)"));
        }
        let mut h = ptr::null_mut();
        // SAFETY: a valid out-pointer and NUL-terminated name.
        let r = unsafe { snd_seq_open(&mut h, c"default".as_ptr(), streams, SND_SEQ_NONBLOCK) };
        if r < 0 {
            return Err(alsa_error("cannot open the ALSA sequencer", r));
        }
        Ok(Self(h))
    }
}

/// A port of another client, with its address.
struct Source {
    port: Port,
    client: c_int,
    number: c_int,
}

fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: a NUL-terminated string owned by the info object.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Every port of every other client that can be subscribed to with `caps`:
/// for reading or for writing.
fn ports(seq: &Seq, caps_wanted: c_uint) -> Result<Vec<Source>, String> {
    let (mut client, mut port) = (ptr::null_mut(), ptr::null_mut());
    // SAFETY: valid out-pointers; both are freed below.
    unsafe {
        if snd_seq_client_info_malloc(&mut client) < 0 {
            return Err("out of memory".into());
        }
        if snd_seq_port_info_malloc(&mut port) < 0 {
            snd_seq_client_info_free(client);
            return Err("out of memory".into());
        }
    }
    let mut out: Vec<Source> = Vec::new();
    // SAFETY: an open sequencer and live info objects, queried in the
    // order the ALSA documentation gives (client -1, then port -1).
    unsafe {
        let me = snd_seq_client_id(seq.0);
        snd_seq_client_info_set_client(client, -1);
        while snd_seq_query_next_client(seq.0, client) >= 0 {
            let c = snd_seq_client_info_get_client(client);
            if c == CLIENT_SYSTEM || c == me {
                continue;
            }
            let client_name = text(snd_seq_client_info_get_name(client));
            snd_seq_port_info_set_client(port, c);
            snd_seq_port_info_set_port(port, -1);
            while snd_seq_query_next_port(seq.0, port) >= 0 {
                let caps = snd_seq_port_info_get_capability(port);
                if caps & caps_wanted != caps_wanted || caps & CAP_NO_EXPORT != 0 {
                    continue;
                }
                let n = snd_seq_port_info_get_port(port);
                let port_name = text(snd_seq_port_info_get_name(port));
                let mut id = format!("{PREFIX}{client_name}:{port_name}");
                let same = out.iter().filter(|s| s.port.id == id || s.port.id.starts_with(&format!("{id} #"))).count();
                if same > 0 {
                    id = format!("{id} #{}", same + 1);
                }
                let name = format!("{port_name} ({client_name}, {c}:{n})");
                out.push(Source { port: Port { id, name }, client: c, number: n });
            }
        }
        snd_seq_port_info_free(port);
        snd_seq_client_info_free(client);
    }
    Ok(out)
}

/// The sequencer's readable ports; an error if there is no sequencer (the
/// `snd-seq` module is not loaded).
pub fn inputs() -> Result<Vec<Port>, String> {
    let seq = Seq::open(SND_SEQ_OPEN_INPUT)?;
    Ok(ports(&seq, CAP_READ | CAP_SUBS_READ)?.into_iter().map(|s| s.port).collect())
}

/// The sequencer's writable ports.
pub fn outputs() -> Result<Vec<Port>, String> {
    let seq = Seq::open(SND_SEQ_OPEN_OUTPUT)?;
    Ok(ports(&seq, CAP_WRITE | CAP_SUBS_WRITE)?.into_iter().map(|s| s.port).collect())
}

/// Fills `snd_seq_ev_ctrl_t`, as `message` reads it.
fn control(ev: &mut Event, channel: u8, param: u32, value: i32) {
    ev.data[0] = channel;
    ev.data[4..8].copy_from_slice(&param.to_ne_bytes());
    ev.data[8..12].copy_from_slice(&value.to_ne_bytes());
}

/// The event for a message's bytes: a note, controller or bend, or the
/// song position, start, continue, stop and clock TINVO sends.
fn event(bytes: &[u8]) -> Option<Event> {
    let (status, a, b) = (*bytes.first()?, bytes.get(1).copied(), bytes.get(2).copied());
    let channel = status & 0x0F;
    let fourteen = || Some(i32::from(b?) << 7 | i32::from(a?));
    let mut ev = Event::default();
    ev.kind = match status {
        0x80..=0x9F => {
            ev.data[..3].copy_from_slice(&[channel, a?, b?]);
            if status < 0x90 { EVENT_NOTEOFF } else { EVENT_NOTEON }
        }
        0xB0..=0xBF => {
            control(&mut ev, channel, u32::from(a?), i32::from(b?));
            EVENT_CONTROLLER
        }
        0xE0..=0xEF => {
            control(&mut ev, channel, 0, fourteen()? - 8192);
            EVENT_PITCHBEND
        }
        0xF2 => {
            control(&mut ev, 0, 0, fourteen()?);
            EVENT_SONGPOS
        }
        0xF8 => EVENT_CLOCK,
        0xFA => EVENT_START,
        0xFB => EVENT_CONTINUE,
        0xFC => EVENT_STOP,
        _ => return None,
    };
    Some(ev)
}

/// An open sequencer output: TINVO's port, connected to the chosen one.
pub struct OutHandle {
    seq: Seq,
    port: c_int,
}

impl OutHandle {
    /// An event goes whole or not at all: nothing is left to send.
    pub fn flush(&mut self) -> bool {
        true
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut ev = event(bytes).ok_or("not a message the sequencer output sends")?;
        (ev.queue, ev.source[1], ev.dest) = (QUEUE_DIRECT, self.port as u8, [ADDRESS_SUBSCRIBERS, ADDRESS_UNKNOWN]);
        // SAFETY: an open sequencer and a complete event addressed to our
        // port's subscribers.
        let r = unsafe { snd_seq_event_output_direct(self.seq.0, &mut ev) };
        if r < 0 { Err(alsa_error("cannot send to the MIDI output", r)) } else { Ok(()) }
    }
}

pub fn open_output(port: &Port) -> Result<OutHandle, String> {
    let seq = Seq::open(SND_SEQ_OPEN_OUTPUT)?;
    let dest = ports(&seq, CAP_WRITE | CAP_SUBS_WRITE)?
        .into_iter()
        .find(|s| s.port.id == port.id)
        .ok_or_else(|| format!("MIDI output '{}' is not connected", port.id))?;
    // SAFETY: an open sequencer and NUL-terminated names.
    let mine = unsafe {
        snd_seq_set_client_name(seq.0, c"TINVO".as_ptr());
        snd_seq_create_simple_port(
            seq.0,
            c"output".as_ptr(),
            CAP_READ | CAP_SUBS_READ,
            TYPE_MIDI_GENERIC | TYPE_APPLICATION,
        )
    };
    if mine < 0 {
        return Err(alsa_error("cannot create the sequencer port", mine));
    }
    // SAFETY: our port and the destination's address.
    let r = unsafe { snd_seq_connect_to(seq.0, mine, dest.client, dest.number) };
    if r < 0 {
        return Err(alsa_error(&format!("cannot connect to '{}'", port.name), r));
    }
    Ok(OutHandle { seq, port: mine })
}

/// The note message an event carries, if any.
fn message(ev: &Event) -> Option<Message> {
    let [channel, note, velocity, ..] = ev.data;
    let channel = channel & 0x0F;
    // `snd_seq_ev_ctrl_t`: the channel, three reserved bytes, then the
    // controller and its value as 32-bit integers.
    let word = |at: usize| i32::from_ne_bytes(ev.data[at..at + 4].try_into().expect("four bytes"));
    match ev.kind {
        EVENT_NOTEON => decode(0x90 | channel, note, velocity),
        EVENT_NOTEOFF => decode(0x80 | channel, note, velocity),
        EVENT_CONTROLLER => {
            let controller = u8::try_from(word(4)).ok().filter(|&c| c < 128)?;
            Some(Message::Control { channel, controller, value: word(8).clamp(0, 127) as u8 })
        }
        EVENT_PITCHBEND => Some(Message::Bend { channel, value: word(8).clamp(-8192, 8191) as i16 }),
        EVENT_PORT_UNSUBSCRIBED => Some(Message::Lost),
        _ => None,
    }
}

pub struct Handle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn open<H>(port: &Port, mut handler: H) -> Result<Handle, String>
where
    H: FnMut(Message) + Send + 'static,
{
    let seq = Seq::open(SND_SEQ_OPEN_INPUT)?;
    let source = ports(&seq, CAP_READ | CAP_SUBS_READ)?
        .into_iter()
        .find(|s| s.port.id == port.id)
        .ok_or_else(|| format!("MIDI input '{}' is not connected", port.id))?;
    // SAFETY: an open sequencer and NUL-terminated names.
    let mine = unsafe {
        snd_seq_set_client_name(seq.0, c"TINVO".as_ptr());
        snd_seq_create_simple_port(
            seq.0,
            c"input".as_ptr(),
            CAP_WRITE | CAP_SUBS_WRITE,
            TYPE_MIDI_GENERIC | TYPE_APPLICATION,
        )
    };
    if mine < 0 {
        return Err(alsa_error("cannot create the sequencer port", mine));
    }
    // SAFETY: our port and the source's address.
    let r = unsafe { snd_seq_connect_from(seq.0, mine, source.client, source.number) };
    if r < 0 {
        return Err(alsa_error(&format!("cannot subscribe to '{}'", port.name), r));
    }
    let mut fds = [PollFd { fd: -1, events: 0, revents: 0 }; MAX_FDS];
    // SAFETY: `fds` has room for MAX_FDS descriptors.
    let n = unsafe { snd_seq_poll_descriptors(seq.0, fds.as_mut_ptr(), MAX_FDS as c_uint, POLLIN) };
    if n <= 0 {
        return Err(alsa_error("cannot poll the sequencer", n));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("tinvo-midi".into())
        .spawn(move || {
            let seq = seq;
            while !flag.load(Ordering::Acquire) {
                let mut ready = fds;
                // SAFETY: `n` initialized descriptors.
                if unsafe { poll(ready.as_mut_ptr(), n as c_ulong, POLL_MS) } <= 0 {
                    continue;
                }
                // A source that never pauses must not keep the thread from
                // stopping.
                while !flag.load(Ordering::Acquire) {
                    let mut ev: *mut Event = ptr::null_mut();
                    // SAFETY: an open, non-blocking sequencer; the event
                    // stays valid until the next call.
                    let r = unsafe { snd_seq_event_input(seq.0, &mut ev) };
                    match r {
                        // The kernel FIFO overran and was cleared; events
                        // were lost, and what follows is valid again.
                        _ if r == -ENOSPC => {
                            handler(Message::Lost);
                            continue;
                        }
                        // Nothing more until the next poll.
                        _ if r == -EAGAIN => break,
                        // Anything else would be reported again by every
                        // poll, spinning this thread; give up instead.
                        _ if r < 0 => {
                            handler(Message::Lost);
                            return;
                        }
                        _ if ev.is_null() => break,
                        _ => {}
                    }
                    // SAFETY: a valid event from the sequencer.
                    if let Some(m) = message(unsafe { &*ev }) {
                        handler(m);
                    }
                }
            }
        })
        .map_err(|e| format!("cannot start the MIDI thread: {e}"))?;
    Ok(Handle { stop, thread: Some(thread) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn note(kind: u8, channel: u8, note: u8, velocity: u8) -> Event {
        Event { kind, data: [channel, note, velocity, 0, 0, 0, 0, 0, 0, 0, 0, 0], ..Event::default() }
    }

    #[test]
    fn note_events_decode_like_midi_bytes() {
        assert_eq!(
            message(&note(EVENT_NOTEON, 2, 60, 100)),
            Some(Message::NoteOn { channel: 2, note: 60, velocity: 100 })
        );
        assert_eq!(message(&note(EVENT_NOTEON, 0, 60, 0)), Some(Message::NoteOff { channel: 0, note: 60 }));
        assert_eq!(message(&note(EVENT_NOTEOFF, 15, 61, 64)), Some(Message::NoteOff { channel: 15, note: 61 }));
        let control = |kind, channel: u8, param: i32, value: i32| {
            let mut data = [0; 12];
            data[0] = channel;
            data[4..8].copy_from_slice(&param.to_ne_bytes());
            data[8..].copy_from_slice(&value.to_ne_bytes());
            Event { kind, data, ..Event::default() }
        };
        assert_eq!(
            message(&control(EVENT_CONTROLLER, 4, 64, 127)),
            Some(Message::Control { channel: 4, controller: 64, value: 127 })
        );
        assert_eq!(message(&control(EVENT_CONTROLLER, 4, 300, 1)), None, "not a MIDI controller");
        assert_eq!(message(&control(EVENT_PITCHBEND, 1, 0, -8192)), Some(Message::Bend { channel: 1, value: -8192 }));
        assert_eq!(message(&control(EVENT_PITCHBEND, 1, 0, 9000)), Some(Message::Bend { channel: 1, value: 8191 }));
        // Port-exit (64) events carry no note.
        assert_eq!(message(&note(64, 0, 0, 0)), None);
    }

    #[test]
    fn messages_become_the_events_they_decode_from() {
        for bytes in
            [[0x92, 60, 100], [0x8F, 61, 64], [0xB4, 64, 127], [0xE1, 0, 0], [0xE1, 0x7F, 0x7F], [0xE1, 0, 0x40]]
        {
            let decoded = crate::midi::decode(bytes[0], bytes[1], bytes[2]);
            assert_eq!(message(&event(&bytes).unwrap()), decoded, "{bytes:?}");
        }
        let kind = |bytes: &[u8]| event(bytes).map(|e| e.kind);
        assert_eq!([0xF8, 0xFA, 0xFB, 0xFC].map(|b| kind(&[b])), [36, 30, 31, 32].map(Some));
        // The song position in 16ths, as the control event's value.
        let pos = event(&[0xF2, 0x05, 0x02]).unwrap();
        assert_eq!((pos.kind, i32::from_ne_bytes(pos.data[8..12].try_into().unwrap())), (EVENT_SONGPOS, 2 << 7 | 5));
        assert_eq!(kind(&[0xF0]), None);
        assert_eq!(kind(&[0x90, 60]), None, "a message cut short");
    }

    /// Sends a note and the clock to a sequencer client of the test's own
    /// through TINVO's output; skipped where there is no sequencer.
    #[test]
    fn messages_reach_another_sequencer_client() {
        let receiver = match Seq::open(SND_SEQ_OPEN_INPUT) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{e}; skipping");
                return;
            }
        };
        // SAFETY: an open sequencer and NUL-terminated names.
        let port = unsafe {
            snd_seq_set_client_name(receiver.0, c"tinvo-test-synth".as_ptr());
            snd_seq_create_simple_port(receiver.0, c"synth".as_ptr(), CAP_WRITE | CAP_SUBS_WRITE, TYPE_MIDI_GENERIC)
        };
        assert!(port >= 0);
        let dest = outputs().unwrap().into_iter().find(|p| p.id == "seq:tinvo-test-synth:synth").expect("port listed");
        let mut out = open_output(&dest).unwrap();
        for bytes in [&[0x93, 64, 90][..], &[0xF8], &[0xF2, 4, 0], &[0xFB], &[0x83, 64, 0]] {
            out.send(bytes).unwrap();
        }
        assert!(out.send(&[0xF0]).is_err());
        let mut got = Vec::new();
        for _ in 0..200 {
            let mut ev: *mut Event = ptr::null_mut();
            // SAFETY: an open, non-blocking sequencer.
            if unsafe { snd_seq_event_input(receiver.0, &mut ev) } >= 0 && !ev.is_null() {
                // SAFETY: a valid event from the sequencer.
                let ev = unsafe { *ev };
                // Subscription notices come from the system client.
                if ev.source[0] != CLIENT_SYSTEM as u8 {
                    got.push((ev.kind, ev.data));
                }
                if got.len() == 5 {
                    break;
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let kinds: Vec<u8> = got.iter().map(|g| g.0).collect();
        assert_eq!(kinds, [EVENT_NOTEON, EVENT_CLOCK, EVENT_SONGPOS, EVENT_CONTINUE, EVENT_NOTEOFF]);
        assert_eq!(got[0].1[..3], [3, 64, 90]);
    }

    /// Sends notes from a sequencer client of the test's own to TINVO's
    /// input; skipped where there is no sequencer.
    #[test]
    fn notes_arrive_from_another_sequencer_client() {
        let sender = match Seq::open(SND_SEQ_OPEN_OUTPUT) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{e}; skipping");
                return;
            }
        };
        // SAFETY: an open sequencer and NUL-terminated names.
        let out = unsafe {
            snd_seq_set_client_name(sender.0, c"tinvo-test-keys".as_ptr());
            snd_seq_create_simple_port(sender.0, c"keys".as_ptr(), CAP_READ | CAP_SUBS_READ, TYPE_MIDI_GENERIC)
        };
        assert!(out >= 0);
        let port = inputs().unwrap().into_iter().find(|p| p.id == "seq:tinvo-test-keys:keys").expect("port listed");
        assert!(port.name.starts_with("keys (tinvo-test-keys, "), "{}", port.name);
        let (tx, rx) = mpsc::channel();
        let _input = open(&port, move |m| {
            let _ = tx.send(m);
        })
        .unwrap();
        for mut ev in [note(EVENT_NOTEON, 1, 64, 90), note(EVENT_NOTEOFF, 1, 64, 0)] {
            (ev.queue, ev.source[1], ev.dest) = (QUEUE_DIRECT, out as u8, [ADDRESS_SUBSCRIBERS, ADDRESS_UNKNOWN]);
            // SAFETY: a complete event addressed to our port's subscribers.
            assert!(unsafe { snd_seq_event_output_direct(sender.0, &mut ev) } >= 0);
        }
        let wait = Duration::from_secs(2);
        assert_eq!(rx.recv_timeout(wait), Ok(Message::NoteOn { channel: 1, note: 64, velocity: 90 }));
        assert_eq!(rx.recv_timeout(wait), Ok(Message::NoteOff { channel: 1, note: 64 }));
        // The source going lets every held key go.
        drop(sender);
        assert_eq!(rx.recv_timeout(wait), Ok(Message::Lost));
    }
}
