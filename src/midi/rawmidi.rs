//! Linux MIDI input through ALSA rawmidi.
//!
//! The fallback where the ALSA sequencer is missing. Ports are the rawmidi
//! devices ALSA's hints list for input (hardware and USB MIDI); software
//! clients of the sequencer are not visible here.
//! A TINVO thread polls the device and runs the byte stream through
//! [`Parser`].

use super::{Message, Parser, Port};
use crate::audio::alsa::{alsa_error, hints};
use std::ffi::{CString, c_char, c_int, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

const SND_RAWMIDI_NONBLOCK: c_int = 0x0002;
const EAGAIN: isize = 11;
const MAX_FDS: usize = 4;
/// How often the thread looks at the stop flag while no bytes arrive.
const POLL_MS: c_int = 50;

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[link(name = "asound")]
unsafe extern "C" {
    fn snd_rawmidi_open(input: *mut *mut c_void, output: *mut *mut c_void, name: *const c_char, mode: c_int) -> c_int;
    fn snd_rawmidi_close(rawmidi: *mut c_void) -> c_int;
    fn snd_rawmidi_read(rawmidi: *mut c_void, buffer: *mut c_void, size: usize) -> isize;
    fn snd_rawmidi_poll_descriptors(rawmidi: *mut c_void, pfds: *mut PollFd, space: c_uint) -> c_int;
}

unsafe extern "C" {
    fn poll(fds: *mut PollFd, n: c_ulong, timeout: c_int) -> c_int;
}

pub fn inputs() -> Result<Vec<Port>, String> {
    Ok(hints(c"rawmidi")?
        .into_iter()
        .filter(|h| h.ioid.as_deref() != Some("Output"))
        .map(|h| {
            let label = h.desc.unwrap_or_else(|| h.name.clone());
            Port { name: format!("{label} ({})", h.name), id: h.name }
        })
        .collect())
}

struct Raw(*mut c_void);

// SAFETY: the handle is used by one thread at a time (moved into the reader).
unsafe impl Send for Raw {}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: an open handle, closed once.
        unsafe { snd_rawmidi_close(self.0) };
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
    let name = CString::new(port.id.as_str()).map_err(|_| format!("bad MIDI port name '{}'", port.id))?;
    let mut raw = ptr::null_mut();
    // SAFETY: valid out-pointer and NUL-terminated name; no output side.
    let r = unsafe { snd_rawmidi_open(&mut raw, ptr::null_mut(), name.as_ptr(), SND_RAWMIDI_NONBLOCK) };
    if r < 0 {
        return Err(alsa_error(&format!("cannot open MIDI input '{}'", port.id), r));
    }
    let raw = Raw(raw);
    let mut fds = [PollFd { fd: -1, events: 0, revents: 0 }; MAX_FDS];
    // SAFETY: `fds` has room for MAX_FDS descriptors.
    let n = unsafe { snd_rawmidi_poll_descriptors(raw.0, fds.as_mut_ptr(), MAX_FDS as c_uint) };
    if n <= 0 {
        return Err(alsa_error("cannot poll the MIDI input", n));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("tinvo-midi".into())
        .spawn(move || {
            let raw = raw;
            let mut parser = Parser::new();
            let mut buf = [0u8; 256];
            while !flag.load(Ordering::Acquire) {
                let mut ready = fds;
                // SAFETY: `n` initialized descriptors.
                if unsafe { poll(ready.as_mut_ptr(), n as c_ulong, POLL_MS) } <= 0 {
                    continue;
                }
                loop {
                    // SAFETY: a valid buffer and its size.
                    let got = unsafe { snd_rawmidi_read(raw.0, buf.as_mut_ptr().cast(), buf.len()) };
                    if got == -EAGAIN || got == 0 {
                        break;
                    }
                    if got < 0 {
                        // The device is gone; there is nothing more to read.
                        return;
                    }
                    for &b in &buf[..got as usize] {
                        if let Some(m) = parser.feed(b) {
                            handler(m);
                        }
                    }
                }
            }
        })
        .map_err(|e| format!("cannot start the MIDI thread: {e}"))?;
    Ok(Handle { stop, thread: Some(thread) })
}
