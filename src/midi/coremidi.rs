//! macOS MIDI input through CoreMIDI.
//!
//! Ports are CoreMIDI's sources, identified by their unique id (stable across
//! reconnects). CoreMIDI calls the read procedure on its own high-priority
//! thread with packet lists, which run through [`Parser`] since a packet may
//! hold several messages (or part of a system exclusive one).

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
    fn MIDIPortDispose(port: MidiObject) -> i32;
    static kMIDIPropertyDisplayName: CFStringRef;
    static kMIDIPropertyUniqueID: CFStringRef;
}

/// Every source with its unique id and name.
fn sources() -> Vec<(MidiObject, Port)> {
    // SAFETY: plain queries; properties are copied out and released.
    unsafe {
        (0..MIDIGetNumberOfSources())
            .filter_map(|i| {
                let src = MIDIGetSource(i);
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
                Some((src, Port { name: name.unwrap_or_else(|| format!("MIDI source {id}")), id }))
            })
            .collect()
    }
}

pub fn inputs() -> Result<Vec<Port>, String> {
    Ok(sources().into_iter().map(|(_, p)| p).collect())
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
