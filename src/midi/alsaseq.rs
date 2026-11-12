//! Linux MIDI input through the ALSA sequencer.
//!
//! The sequencer sees everything rawmidi does (the kernel's `snd-seq-midi`
//! puts every hardware port on it) and also software clients: virtual
//! keyboards, DAWs, `aseqdump`'s peers. TINVO creates one writable port
//! named `TINVO:input`, subscribes it to the chosen port and decodes note
//! events on a thread that polls the sequencer.
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

/// A port another client offers for reading, with its address.
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

/// Every port of every other client that can be subscribed to for reading.
fn sources(seq: &Seq) -> Result<Vec<Source>, String> {
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
                if caps & (CAP_READ | CAP_SUBS_READ) != CAP_READ | CAP_SUBS_READ || caps & CAP_NO_EXPORT != 0 {
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
    Ok(sources(&seq)?.into_iter().map(|s| s.port).collect())
}

/// The note message an event carries, if any.
fn message(ev: &Event) -> Option<Message> {
    let [channel, note, velocity, ..] = ev.data;
    match ev.kind {
        EVENT_NOTEON => decode(0x90 | (channel & 0x0F), note, velocity),
        EVENT_NOTEOFF => decode(0x80 | (channel & 0x0F), note, velocity),
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
    let source = sources(&seq)?
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
                loop {
                    let mut ev: *mut Event = ptr::null_mut();
                    // SAFETY: an open, non-blocking sequencer; the event
                    // stays valid until the next call.
                    let r = unsafe { snd_seq_event_input(seq.0, &mut ev) };
                    match r {
                        // The kernel FIFO overran and was cleared; events
                        // were lost, and what follows is valid again.
                        _ if r == -ENOSPC => continue,
                        // Nothing more until the next poll.
                        _ if r == -EAGAIN => break,
                        // Anything else would be reported again by every
                        // poll, spinning this thread; give up instead.
                        _ if r < 0 => return,
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

    const SND_SEQ_OPEN_OUTPUT: c_int = 1;
    const QUEUE_DIRECT: u8 = 253;
    const ADDRESS_SUBSCRIBERS: u8 = 254;
    const ADDRESS_UNKNOWN: u8 = 253;

    #[link(name = "asound")]
    unsafe extern "C" {
        fn snd_seq_event_output_direct(handle: *mut c_void, ev: *mut Event) -> c_int;
    }

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
        // Controller (10) and port-exit (64) events carry no note.
        assert_eq!(message(&note(10, 0, 7, 100)), None);
        assert_eq!(message(&note(64, 0, 0, 0)), None);
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
    }
}
