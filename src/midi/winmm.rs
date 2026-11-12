//! Windows MIDI input through WinMM (`midiIn*`).
//!
//! WinMM numbers devices by position, which shifts when one is unplugged, so
//! a port's id is its name (with ` #2`, ` #3` for repeated names, in device
//! order). WinMM hands over complete short messages; system exclusive data
//! would need buffers TINVO does not register, so it never arrives.

use super::{Message, Port, decode};
use std::ffi::c_void;
use std::ptr;

type Hmidiin = *mut c_void;

const CALLBACK_FUNCTION: u32 = 0x0003_0000;
const MIM_DATA: u32 = 0x3C3;
const MAXPNAMELEN: usize = 32;

/// `MIDIINCAPSW`; every field falls on its natural alignment, so the
/// byte packing `mmsystem.h` asks for changes nothing.
#[repr(C)]
struct MidiInCapsW {
    mid: u16,
    pid: u16,
    driver_version: u32,
    pname: [u16; MAXPNAMELEN],
    support: u32,
}

type MidiInProc = unsafe extern "system" fn(Hmidiin, u32, usize, usize, usize);

#[link(name = "winmm")]
unsafe extern "system" {
    fn midiInGetNumDevs() -> u32;
    fn midiInGetDevCapsW(device: usize, caps: *mut MidiInCapsW, size: u32) -> u32;
    fn midiInOpen(handle: *mut Hmidiin, device: u32, callback: usize, instance: usize, flags: u32) -> u32;
    fn midiInStart(handle: Hmidiin) -> u32;
    fn midiInStop(handle: Hmidiin) -> u32;
    fn midiInReset(handle: Hmidiin) -> u32;
    fn midiInClose(handle: Hmidiin) -> u32;
    fn midiInGetErrorTextW(error: u32, text: *mut u16, len: u32) -> u32;
}

fn error_text(what: &str, err: u32) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: a valid buffer and its length in characters.
    let ok = unsafe { midiInGetErrorTextW(err, buf.as_mut_ptr(), buf.len() as u32) } == 0;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if ok && end > 0 {
        format!("{what}: {}", String::from_utf16_lossy(&buf[..end]))
    } else {
        format!("{what} (MMRESULT {err})")
    }
}

/// Every input device's index and port.
fn ports() -> Vec<(u32, Port)> {
    // SAFETY: no arguments.
    let n = unsafe { midiInGetNumDevs() };
    let mut out: Vec<(u32, Port)> = Vec::new();
    for i in 0..n {
        let mut caps = MidiInCapsW { mid: 0, pid: 0, driver_version: 0, pname: [0; MAXPNAMELEN], support: 0 };
        // SAFETY: a valid out-pointer and its size.
        let r = unsafe { midiInGetDevCapsW(i as usize, &mut caps, size_of::<MidiInCapsW>() as u32) };
        if r != 0 {
            continue;
        }
        let end = caps.pname.iter().position(|&c| c == 0).unwrap_or(MAXPNAMELEN);
        let name = String::from_utf16_lossy(&caps.pname[..end]);
        let name = if name.is_empty() { format!("MIDI input {}", i + 1) } else { name };
        let repeats = out.iter().filter(|(_, p)| p.name == name).count();
        let id = if repeats == 0 { name.clone() } else { format!("{name} #{}", repeats + 1) };
        out.push((i, Port { id, name }));
    }
    out
}

pub fn inputs() -> Result<Vec<Port>, String> {
    Ok(ports().into_iter().map(|(_, p)| p).collect())
}

struct State<H> {
    handler: H,
}

unsafe extern "system" fn callback<H: FnMut(Message)>(_h: Hmidiin, msg: u32, instance: usize, p1: usize, _p2: usize) {
    if msg != MIM_DATA {
        return;
    }
    // SAFETY: `instance` is the `State<H>` boxed in `open`, freed only after
    // `midiInClose` returns, and WinMM calls one device's callback serially.
    let state = unsafe { &mut *(instance as *mut State<H>) };
    let [status, d1, d2, _] = (p1 as u32).to_le_bytes();
    if let Some(m) = decode(status, d1, d2) {
        (state.handler)(m);
    }
}

pub struct Handle {
    handle: Hmidiin,
    state: *mut c_void,
    free: unsafe fn(*mut c_void),
}

// SAFETY: the handle and state pointer are only used again in `drop`.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: an open handle; once midiInClose returns WinMM makes no
        // further callbacks, so the state can be freed.
        unsafe {
            midiInStop(self.handle);
            midiInReset(self.handle);
            midiInClose(self.handle);
            (self.free)(self.state);
        }
    }
}

unsafe fn free_state<H>(p: *mut c_void) {
    // SAFETY: `p` came from Box::into_raw of a State<H>, freed once.
    drop(unsafe { Box::from_raw(p.cast::<State<H>>()) });
}

pub fn open<H>(port: &Port, handler: H) -> Result<Handle, String>
where
    H: FnMut(Message) + Send + 'static,
{
    let index = ports()
        .into_iter()
        .find(|(_, p)| p.id == port.id)
        .map(|(i, _)| i)
        .ok_or_else(|| format!("MIDI input '{}' is not connected", port.id))?;
    let state = Box::into_raw(Box::new(State { handler }));
    let mut handle: Hmidiin = ptr::null_mut();
    let proc: MidiInProc = callback::<H>;
    // SAFETY: valid out-pointer; the callback and state outlive the handle.
    let r = unsafe { midiInOpen(&mut handle, index, proc as usize, state as usize, CALLBACK_FUNCTION) };
    if r != 0 {
        // SAFETY: the device did not open, so nothing else holds `state`.
        drop(unsafe { Box::from_raw(state) });
        return Err(error_text(&format!("cannot open MIDI input '{}'", port.name), r));
    }
    let h = Handle { handle, state: state.cast(), free: free_state::<H> };
    // SAFETY: an open handle.
    let r = unsafe { midiInStart(h.handle) };
    if r != 0 {
        return Err(error_text(&format!("cannot start MIDI input '{}'", port.name), r));
    }
    Ok(h)
}
