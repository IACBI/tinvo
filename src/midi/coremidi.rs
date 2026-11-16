//! macOS MIDI input and output through CoreMIDI.
//!
//! Ports are CoreMIDI's sources and destinations, identified by their unique
//! id (stable across reconnects). CoreMIDI calls the read procedure on its
//! own high-priority thread with packet lists, which run through [`Parser`]
//! since a packet may hold several messages (or part of a system exclusive
//! one). Output sends each message in a packet list of its own, timestamped
//! now.

use super::{Message, Parser, Port};
use crate::audio::coreaudio::{CFStringRef, new_cf_string, release, take_cf_string};
use std::ffi::c_void;
use std::ptr;

type MidiObject = u32;
type ReadProc = unsafe extern "C" fn(*const c_void, *mut c_void, *mut c_void);

#[link(name = "CoreMIDI", kind = "framework")]
unsafe extern "C" {
    fn MIDIGetNumberOfSources() -> usize;
    fn MIDIGetSource(index: usize) -> MidiObject;
    fn MIDIGetNumberOfDestinations() -> usize;
    fn MIDIGetDestination(index: usize) -> MidiObject;
    fn MIDIObjectGetStringProperty(obj: MidiObject, property: CFStringRef, out: *mut CFStringRef) -> i32;
    fn MIDIObjectGetIntegerProperty(obj: MidiObject, property: CFStringRef, out: *mut i32) -> i32;
    fn MIDIClientCreate(name: CFStringRef, notify: *const c_void, refcon: *mut c_void, out: *mut MidiObject) -> i32;
    fn MIDIClientDispose(client: MidiObject) -> i32;
    fn MIDIInputPortCreate(
        client: MidiObject,
        name: CFStringRef,
        read: ReadProc,
        refcon: *mut c_void,
        out: *mut MidiObject,
    ) -> i32;
    fn MIDIPortConnectSource(port: MidiObject, source: MidiObject, refcon: *mut c_void) -> i32;
    fn MIDIOutputPortCreate(client: MidiObject, name: CFStringRef, out: *mut MidiObject) -> i32;
    fn MIDIPacketListInit(list: *mut c_void) -> *mut c_void;
    fn MIDIPacketListAdd(
        list: *mut c_void,
        list_size: usize,
        packet: *mut c_void,
        time: u64,
        n: usize,
        data: *const u8,
    ) -> *mut c_void;
    fn MIDISend(port: MidiObject, dest: MidiObject, list: *const c_void) -> i32;
    fn MIDIPortDispose(port: MidiObject) -> i32;
    static kMIDIPropertyDisplayName: CFStringRef;
    static kMIDIPropertyUniqueID: CFStringRef;
}

/// Every source with its unique id and name.
fn sources() -> Vec<(MidiObject, Port)> {
    // SAFETY: plain queries with indices below the count.
    endpoints(unsafe { MIDIGetNumberOfSources() }, |i| unsafe { MIDIGetSource(i) }, "source")
}

/// Every destination with its unique id and name.
fn destinations() -> Vec<(MidiObject, Port)> {
    // SAFETY: plain queries with indices below the count.
    endpoints(unsafe { MIDIGetNumberOfDestinations() }, |i| unsafe { MIDIGetDestination(i) }, "destination")
}

fn endpoints(count: usize, get: impl Fn(usize) -> MidiObject, what: &str) -> Vec<(MidiObject, Port)> {
    // SAFETY: plain queries; properties are copied out and released.
    unsafe {
        (0..count)
            .filter_map(|i| {
                let src = get(i);
                if src == 0 {
                    return None;
                }
                let mut uid = 0i32;
                if MIDIObjectGetIntegerProperty(src, kMIDIPropertyUniqueID, &mut uid) != 0 {
                    return None;
                }
                let mut name: CFStringRef = ptr::null();
                let name = if MIDIObjectGetStringProperty(src, kMIDIPropertyDisplayName, &mut name) == 0 {
                    take_cf_string(name)
                } else {
                    None
                };
                let id = uid.to_string();
                Some((src, Port { name: name.unwrap_or_else(|| format!("MIDI {what} {id}")), id }))
            })
            .collect()
    }
}

pub fn inputs() -> Result<Vec<Port>, String> {
    Ok(sources().into_iter().map(|(_, p)| p).collect())
}

pub fn outputs() -> Result<Vec<Port>, String> {
    Ok(destinations().into_iter().map(|(_, p)| p).collect())
}

/// An open output: a client and its output port, sending to `dest`.
pub struct OutHandle {
    client: MidiObject,
    port: MidiObject,
    dest: MidiObject,
}

// SAFETY: CoreMIDI references are plain ids, usable from any thread.
unsafe impl Send for OutHandle {}

impl Drop for OutHandle {
    fn drop(&mut self) {
        // SAFETY: references this handle created, disposed once.
        unsafe {
            if self.port != 0 {
                MIDIPortDispose(self.port);
            }
            MIDIClientDispose(self.client);
        }
    }
}

impl OutHandle {
    /// A packet goes whole or not at all: nothing is left to send.
    pub fn flush(&mut self) -> bool {
        true
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        // Room for a packet list of one short message, aligned for its
        // 64-bit timestamp.
        let mut buf = [0u64; 8];
        let list = buf.as_mut_ptr().cast::<c_void>();
        // SAFETY: a buffer of the size passed; a timestamp of 0 sends now.
        let status = unsafe {
            let packet = MIDIPacketListInit(list);
            if MIDIPacketListAdd(list, size_of_val(&buf), packet, 0, bytes.len(), bytes.as_ptr()).is_null() {
                return Err("a MIDI message too long to send".into());
            }
            MIDISend(self.port, self.dest, list)
        };
        if status == 0 { Ok(()) } else { Err(format!("cannot send to the MIDI output (OSStatus {status})")) }
    }
}

pub fn open_output(port: &Port) -> Result<OutHandle, String> {
    let dest = destinations()
        .into_iter()
        .find(|(_, p)| p.id == port.id)
        .map(|(d, _)| d)
        .ok_or_else(|| format!("MIDI output '{}' is not connected", port.id))?;
    let mut client = 0;
    // SAFETY: valid name and out-pointer; no notification callback.
    let status =
        with_cf_string("TINVO", |name| unsafe { MIDIClientCreate(name, ptr::null(), ptr::null_mut(), &mut client) });
    if status != 0 {
        return Err(format!("cannot create a CoreMIDI client (OSStatus {status})"));
    }
    let mut h = OutHandle { client, port: 0, dest };
    let mut out = 0;
    // SAFETY: valid client, name and out-pointer.
    let status = with_cf_string("TINVO output", |name| unsafe { MIDIOutputPortCreate(client, name, &mut out) });
    if status != 0 {
        return Err(format!("cannot create a CoreMIDI output port (OSStatus {status})"));
    }
    h.port = out;
    Ok(h)
}

struct State<H> {
    handler: H,
    parser: Parser,
}

/// Bytes from a packet's start to its data (`MIDIPacket` is packed to 4
/// bytes: an 8-byte timestamp, then a 16-bit length).
const PACKET_DATA: usize = 10;

unsafe extern "C" fn read<H: FnMut(Message)>(list: *const c_void, refcon: *mut c_void, _conn: *mut c_void) {
    // SAFETY: `refcon` is the `State<H>` boxed in `open`, freed only after
    // the port is disposed; CoreMIDI calls a port's read procedure serially.
    let state = unsafe { &mut *refcon.cast::<State<H>>() };
    let base = list.cast::<u8>();
    // SAFETY: CoreMIDI hands a valid packet list: a 32-bit count, then the
    // packets (4-byte packing), each followed by the next per MIDIPacketNext.
    unsafe {
        let count = ptr::read_unaligned(base.cast::<u32>());
        let mut packet = base.add(4);
        for _ in 0..count {
            let len = usize::from(ptr::read_unaligned(packet.add(8).cast::<u16>()));
            let data = std::slice::from_raw_parts(packet.add(PACKET_DATA), len);
            for &b in data {
                if let Some(m) = state.parser.feed(b) {
                    (state.handler)(m);
                }
            }
            let next = packet.add(PACKET_DATA + len);
            // On ARM, MIDIPacketNext rounds up to a 4-byte boundary.
            packet = if cfg!(target_arch = "aarch64") { next.add(next.align_offset(4)) } else { next };
        }
    }
}

pub struct Handle {
    client: MidiObject,
    port: MidiObject,
    state: *mut c_void,
    free: unsafe fn(*mut c_void),
}

// SAFETY: the references and state pointer are only used again in `drop`.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: disposing the port stops its read procedure, after which
        // the state is no longer referenced.
        unsafe {
            if self.port != 0 {
                MIDIPortDispose(self.port);
            }
            MIDIClientDispose(self.client);
            (self.free)(self.state);
        }
    }
}

unsafe fn free_state<H>(p: *mut c_void) {
    // SAFETY: `p` came from Box::into_raw of a State<H>, freed once.
    drop(unsafe { Box::from_raw(p.cast::<State<H>>()) });
}

/// Makes a CFString, runs `f` with it and releases it.
fn with_cf_string<R>(s: &str, f: impl FnOnce(CFStringRef) -> R) -> R {
    let cf = new_cf_string(s);
    let r = f(cf);
    if !cf.is_null() {
        // SAFETY: we own the one reference new_cf_string returned.
        unsafe { release(cf) };
    }
    r
}

pub fn open<H>(port: &Port, handler: H) -> Result<Handle, String>
where
    H: FnMut(Message) + Send + 'static,
{
    let source = sources()
        .into_iter()
        .find(|(_, p)| p.id == port.id)
        .map(|(s, _)| s)
        .ok_or_else(|| format!("MIDI input '{}' is not connected", port.id))?;
    let mut client = 0;
    // SAFETY: valid name and out-pointer; no notification callback.
    let status =
        with_cf_string("TINVO", |name| unsafe { MIDIClientCreate(name, ptr::null(), ptr::null_mut(), &mut client) });
    if status != 0 {
        return Err(format!("cannot create a CoreMIDI client (OSStatus {status})"));
    }
    let state = Box::into_raw(Box::new(State { handler, parser: Parser::new() }));
    let mut h = Handle { client, port: 0, state: state.cast(), free: free_state::<H> };
    let mut midi_port = 0;
    // SAFETY: valid client, name and out-pointer; the state outlives the port.
    let status = with_cf_string("TINVO input", |name| unsafe {
        MIDIInputPortCreate(client, name, read::<H>, state.cast(), &mut midi_port)
    });
    if status != 0 {
        return Err(format!("cannot create a CoreMIDI input port (OSStatus {status})"));
    }
    h.port = midi_port;
    // SAFETY: valid port and source.
    let status = unsafe { MIDIPortConnectSource(midi_port, source, ptr::null_mut()) };
    if status != 0 {
        return Err(format!("cannot connect to MIDI input '{}' (OSStatus {status})", port.name));
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[link(name = "CoreMIDI", kind = "framework")]
    unsafe extern "C" {
        fn MIDIDestinationCreate(
            client: MidiObject,
            name: CFStringRef,
            read: ReadProc,
            refcon: *mut c_void,
            out: *mut MidiObject,
        ) -> i32;
        fn MIDIEndpointDispose(endpoint: MidiObject) -> i32;
    }

    type Handler = Box<dyn FnMut(Message) + Send>;

    /// Sends through TINVO's output to a virtual destination of the test's
    /// own, read back with the input's read procedure.
    #[test]
    fn messages_reach_a_virtual_destination() {
        let mut client = 0;
        // SAFETY: valid name and out-pointer.
        let status = with_cf_string("tinvo-test", |name| unsafe {
            MIDIClientCreate(name, ptr::null(), ptr::null_mut(), &mut client)
        });
        if status != 0 {
            eprintln!("no CoreMIDI client (OSStatus {status}); skipping");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let handler: Handler = Box::new(move |m| {
            let _ = tx.send(m);
        });
        let state = Box::into_raw(Box::new(State { handler, parser: Parser::new() }));
        let mut dest = 0;
        // SAFETY: valid client, name and out-pointer; the state outlives
        // the destination, disposed below before it is freed.
        let status = with_cf_string("tinvo-test-synth", |name| unsafe {
            MIDIDestinationCreate(client, name, read::<Handler>, state.cast(), &mut dest)
        });
        assert_eq!(status, 0);
        let port = outputs().unwrap().into_iter().find(|p| p.name.contains("tinvo-test-synth")).expect("listed");
        let mut out = open_output(&port).unwrap();
        out.send(&[0x93, 64, 90]).unwrap();
        out.send(&[0x83, 64, 0]).unwrap();
        let wait = Duration::from_secs(2);
        assert_eq!(rx.recv_timeout(wait), Ok(Message::NoteOn { channel: 3, note: 64, velocity: 90 }));
        assert_eq!(rx.recv_timeout(wait), Ok(Message::NoteOff { channel: 3, note: 64 }));
        drop(out);
        // SAFETY: disposing the destination stops its read procedure; the
        // state is freed once after that.
        unsafe {
            MIDIEndpointDispose(dest);
            MIDIClientDispose(client);
            drop(Box::from_raw(state));
        }
    }
}
