//! MIDI input and output: listing the ports, opening one, decoding an
//! input's bytes into the channel messages TINVO plays, and sending the
//! engine's messages to an output on time ([`schedule`]).
//!
//! Each backend calls an input's handler on its own thread for every message
//! (WinMM's callback thread, a TINVO thread polling the ALSA sequencer or
//! rawmidi, CoreMIDI's receive thread). The handler must not block; the
//! app's pushes into lock-free mailboxes, one for the audio engine and one
//! for the UI. An output is written from TINVO's MIDI output thread.

#[cfg(target_os = "linux")]
mod alsaseq;
#[cfg(target_os = "macos")]
mod coremidi;
#[cfg(target_os = "linux")]
mod rawmidi;
pub mod schedule;
#[cfg(windows)]
mod winmm;

pub use schedule::OutEvent;

/// The sustain pedal's controller.
pub const SUSTAIN: u8 = 64;
/// The mod wheel's controller.
pub const MOD_WHEEL: u8 = 1;

/// A decoded channel message TINVO uses. Everything else (program changes,
/// aftertouch, system messages) is skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    /// A key pressed; `velocity` is 1–127.
    NoteOn { channel: u8, note: u8, velocity: u8 },
    /// A key released (including note-on with velocity 0).
    NoteOff { channel: u8, note: u8 },
    /// A controller moved to `value` (0–127): [`SUSTAIN`], [`MOD_WHEEL`]
    /// and the others, which TINVO ignores.
    Control { channel: u8, controller: u8, value: u8 },
    /// The pitch bend wheel, −8192 (down) to 8191 (up); 0 at rest.
    Bend { channel: u8, value: i16 },
    /// Messages were lost (the input overran), or the input stopped: keys
    /// held may never send their releases.
    Lost,
}

impl Message {
    /// A note-on, or for `velocity` 0 a note-off, as a keyboard sends them.
    pub fn note(channel: u8, note: u8, velocity: u8) -> Self {
        decode(0x90 | (channel & 0x0F), note, velocity).expect("a note message")
    }
}

/// Decodes one complete channel message.
pub fn decode(status: u8, data1: u8, data2: u8) -> Option<Message> {
    let channel = status & 0x0F;
    let (note, velocity) = (data1 & 0x7F, data2 & 0x7F);
    match status & 0xF0 {
        0x90 if velocity > 0 => Some(Message::NoteOn { channel, note, velocity }),
        0x80 | 0x90 => Some(Message::NoteOff { channel, note }),
        0xB0 => Some(Message::Control { channel, controller: note, value: velocity }),
        // Seven low bits first, then seven high ones.
        0xE0 => Some(Message::Bend { channel, value: (i16::from(velocity) << 7 | i16::from(note)) - 8192 }),
        _ => None,
    }
}

/// Number of data bytes after `status`.
fn data_len(status: u8) -> usize {
    match status {
        0x80..=0xBF | 0xE0..=0xEF | 0xF2 => 2,
        0xC0..=0xDF | 0xF1 | 0xF3 => 1,
        _ => 0,
    }
}

/// Turns a raw MIDI byte stream into messages: running status, real-time
/// bytes in the middle of a message, system exclusive and system common
/// messages are all handled as the MIDI 1.0 specification describes.
#[derive(Clone, Debug, Default)]
pub struct Parser {
    /// Current status (0: none, so data bytes are ignored).
    status: u8,
    data: [u8; 2],
    len: usize,
    sysex: bool,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one byte; returns a message when it completes one.
    pub fn feed(&mut self, byte: u8) -> Option<Message> {
        match byte {
            // Real-time messages may appear anywhere and change nothing.
            0xF8..=0xFF => None,
            0xF0 => {
                (self.sysex, self.status, self.len) = (true, 0, 0);
                None
            }
            // End of exclusive, tune request and undefined system bytes end
            // any running status.
            0xF4..=0xF7 => {
                (self.sysex, self.status, self.len) = (false, 0, 0);
                None
            }
            0x80..=0xF3 => {
                (self.sysex, self.status, self.len) = (false, byte, 0);
                None
            }
            _ if self.sysex || self.status == 0 => None,
            _ => {
                self.data[self.len] = byte;
                self.len += 1;
                if self.len < data_len(self.status) {
                    return None;
                }
                self.len = 0;
                let status = self.status;
                if status >= 0xF0 {
                    // System common messages do not set running status.
                    self.status = 0;
                    return None;
                }
                decode(status, self.data[0], if data_len(status) == 2 { self.data[1] } else { 0 })
            }
        }
    }
}

/// A MIDI input port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    /// Identifier to open it by.
    pub id: String,
    /// Human-readable name.
    pub name: String,
}

/// Lists the MIDI input ports present now.
pub fn inputs() -> Result<Vec<Port>, String> {
    platform::inputs()
}

/// Picks the input port `query` names: an exact id, else the one port whose
/// name contains `query` (ignoring case).
pub fn find_port<'a>(ports: &'a [Port], query: &str) -> Result<&'a Port, String> {
    find_in(ports, query, "input")
}

/// Picks the output port `query` names, as [`find_port`] does an input.
pub fn find_output<'a>(ports: &'a [Port], query: &str) -> Result<&'a Port, String> {
    find_in(ports, query, "output")
}

fn find_in<'a>(ports: &'a [Port], query: &str, what: &str) -> Result<&'a Port, String> {
    if let Some(p) = ports.iter().find(|p| p.id == query) {
        return Ok(p);
    }
    let q = query.to_lowercase();
    let hits: Vec<&Port> = ports.iter().filter(|p| p.name.to_lowercase().contains(&q)).collect();
    match hits[..] {
        [p] => Ok(p),
        [] => Err(format!("no MIDI {what} matches '{query}' (tinvo midi lists them)")),
        _ => Err(format!(
            "'{query}' matches {} MIDI {what}s: {}",
            hits.len(),
            hits.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// An open MIDI input; closing it (dropping) stops the handler calls.
pub struct Input {
    name: String,
    _handle: platform::Handle,
}

impl Input {
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Opens the input port with `id`. `handler` runs on the backend's thread
/// for every message until the input is dropped.
pub fn open<H>(id: &str, handler: H) -> Result<Input, String>
where
    H: FnMut(Message) + Send + 'static,
{
    let port =
        inputs()?.into_iter().find(|p| p.id == id).ok_or_else(|| format!("MIDI input '{id}' is not connected"))?;
    let handle = platform::open(&port, handler)?;
    Ok(Input { name: port.name, _handle: handle })
}

/// Lists the MIDI output ports present now.
pub fn outputs() -> Result<Vec<Port>, String> {
    platform::outputs()
}

/// An open MIDI output; closing it (dropping) releases the notes the
/// backend can release.
pub struct Output {
    name: String,
    handle: platform::OutHandle,
}

impl Output {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Sends one complete message: a channel message, or a system common
    /// or real-time one.
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.handle.send(bytes)
    }
}

/// Opens the output port with `id`.
pub fn open_output(id: &str) -> Result<Output, String> {
    let port =
        outputs()?.into_iter().find(|p| p.id == id).ok_or_else(|| format!("MIDI output '{id}' is not connected"))?;
    let handle = platform::open_output(&port)?;
    Ok(Output { name: port.name, handle })
}

#[cfg(windows)]
use winmm as platform;

/// Linux uses the ALSA sequencer, which also sees software clients, and
/// falls back to rawmidi where the sequencer is missing.
#[cfg(target_os = "linux")]
mod platform {
    use super::{Message, Port, alsaseq, rawmidi};

    pub enum Handle {
        Seq(#[allow(dead_code)] alsaseq::Handle),
        Raw(#[allow(dead_code)] rawmidi::Handle),
    }

    pub enum OutHandle {
        Seq(alsaseq::OutHandle),
        Raw(rawmidi::OutHandle),
    }

    impl OutHandle {
        pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            match self {
                OutHandle::Seq(h) => h.send(bytes),
                OutHandle::Raw(h) => h.send(bytes),
            }
        }
    }

    pub fn inputs() -> Result<Vec<Port>, String> {
        alsaseq::inputs().or_else(|_| rawmidi::inputs())
    }

    pub fn outputs() -> Result<Vec<Port>, String> {
        alsaseq::outputs().or_else(|_| rawmidi::outputs())
    }

    pub fn open_output(port: &Port) -> Result<OutHandle, String> {
        if port.id.starts_with(alsaseq::PREFIX) {
            alsaseq::open_output(port).map(OutHandle::Seq)
        } else {
            rawmidi::open_output(port).map(OutHandle::Raw)
        }
    }

    pub fn open<H: FnMut(Message) + Send + 'static>(port: &Port, handler: H) -> Result<Handle, String> {
        if port.id.starts_with(alsaseq::PREFIX) {
            alsaseq::open(port, handler).map(Handle::Seq)
        } else {
            rawmidi::open(port, handler).map(Handle::Raw)
        }
    }
}

#[cfg(target_os = "macos")]
use coremidi as platform;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use super::{Message, Port};

    pub struct Handle;

    pub struct OutHandle;

    impl OutHandle {
        pub fn send(&mut self, _bytes: &[u8]) -> Result<(), String> {
            Err("no MIDI backend for this platform".into())
        }
    }

    pub fn inputs() -> Result<Vec<Port>, String> {
        Err("no MIDI backend for this platform".into())
    }

    pub fn open<H: FnMut(Message) + Send + 'static>(_port: &Port, _handler: H) -> Result<Handle, String> {
        Err("no MIDI backend for this platform".into())
    }

    pub fn outputs() -> Result<Vec<Port>, String> {
        Err("no MIDI backend for this platform".into())
    }

    pub fn open_output(_port: &Port) -> Result<OutHandle, String> {
        Err("no MIDI backend for this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Message> {
        let mut p = Parser::new();
        bytes.iter().filter_map(|&b| p.feed(b)).collect()
    }

    const fn on(note: u8, velocity: u8) -> Message {
        Message::NoteOn { channel: 0, note, velocity }
    }

    const fn off(note: u8) -> Message {
        Message::NoteOff { channel: 0, note }
    }

    #[test]
    fn notes_with_running_status() {
        // Note on, then running status for two more; velocity 0 releases.
        assert_eq!(parse(&[0x90, 60, 100, 64, 90, 60, 0]), [on(60, 100), on(64, 90), off(60)]);
        assert_eq!(parse(&[0x83, 61, 40]), [Message::NoteOff { channel: 3, note: 61 }]);
        assert_eq!(parse(&[0x9F, 1, 2]), [Message::NoteOn { channel: 15, note: 1, velocity: 2 }]);
    }

    #[test]
    fn other_messages_are_skipped_but_counted() {
        // Program change (1 byte), channel pressure (1), poly pressure (2),
        // then a note that relies on none of them.
        assert_eq!(parse(&[0xC0, 5, 0xD0, 9, 0xA0, 60, 9, 0x90, 60, 1]), [on(60, 1)]);
        // Running status applies to program changes too: 5 and 6 are two
        // programs, not a note.
        assert_eq!(parse(&[0xC0, 5, 6, 0x90, 62, 3]), [on(62, 3)]);
    }

    #[test]
    fn real_time_bytes_interleave_without_effect() {
        assert_eq!(parse(&[0x90, 0xF8, 60, 0xFE, 100, 0xF8, 62, 0xFA, 80]), [on(60, 100), on(62, 80)]);
    }

    #[test]
    fn system_exclusive_and_common_end_running_status() {
        // Data inside sysex is ignored, and running status is gone after it.
        assert_eq!(parse(&[0x90, 60, 100, 0xF0, 0x7E, 60, 100, 0xF7, 64, 100]), [on(60, 100)]);
        // Song position (2 data bytes) and song select (1) do not leave a
        // running status behind.
        assert_eq!(parse(&[0x90, 60, 100, 0xF2, 1, 2, 64, 100, 0xF3, 3, 65, 100]), [on(60, 100)]);
        // Tune request has no data and also clears it.
        assert_eq!(parse(&[0x90, 60, 100, 0xF6, 64, 100]), [on(60, 100)]);
        // A new status in the middle of a message abandons the old one.
        assert_eq!(parse(&[0x90, 60, 0x80, 61, 0]), [off(61)]);
        // Leading data bytes with no status are ignored.
        assert_eq!(parse(&[60, 100, 0x90, 60, 100]), [on(60, 100)]);
    }

    #[test]
    fn decode_masks_and_classifies() {
        assert_eq!(decode(0x91, 60, 0), Some(Message::NoteOff { channel: 1, note: 60 }));
        assert_eq!(decode(0x90, 0xBC, 0xFF), Some(on(60, 127)));
        assert_eq!(decode(0xB3, 64, 127), Some(Message::Control { channel: 3, controller: SUSTAIN, value: 127 }));
        assert_eq!(decode(0xC0, 1, 2), None);
    }

    #[test]
    fn controllers_and_bends_with_running_status() {
        let bend = |value| Message::Bend { channel: 2, value };
        assert_eq!(parse(&[0xE2, 0, 64, 0x7F, 0x7F, 0, 0, 0x7F, 0x3F]), [bend(0), bend(8191), bend(-8192), bend(-1)]);
        let wheel = |value| Message::Control { channel: 0, controller: MOD_WHEEL, value };
        assert_eq!(parse(&[0xB0, 1, 0, 1, 127, 0x90, 60, 9]), [wheel(0), wheel(127), on(60, 9)]);
    }

    #[test]
    fn ports_are_found_by_id_or_unique_name() {
        let p = |id: &str, name: &str| Port { id: id.into(), name: name.into() };
        let list = [p("0", "Keystation 49"), p("1", "Launchpad Mini"), p("2", "Keystation Mini 32")];
        assert_eq!(find_port(&list, "1").unwrap().name, "Launchpad Mini");
        assert_eq!(find_port(&list, "launch").unwrap().id, "1");
        assert!(find_port(&list, "keystation").unwrap_err().contains("matches 2"));
        assert!(find_port(&list, "piano").unwrap_err().contains("no MIDI input"));
        assert!(find_output(&list, "piano").unwrap_err().contains("no MIDI output"));
        assert!(find_output(&list, "keystation").unwrap_err().contains("2 MIDI outputs"));
    }

    /// Exercises the platform listing; a bad FFI declaration would crash here.
    #[test]
    fn listing_inputs_is_consistent() {
        listing_is_consistent(inputs());
        listing_is_consistent(outputs());
    }

    fn listing_is_consistent(list: Result<Vec<Port>, String>) {
        match list {
            Ok(ports) => {
                for p in &ports {
                    assert!(!p.id.is_empty() && !p.name.is_empty(), "{p:?}");
                }
                let mut ids: Vec<&str> = ports.iter().map(|p| p.id.as_str()).collect();
                ids.sort_unstable();
                ids.dedup();
                assert_eq!(ids.len(), ports.len(), "ids must be unique");
            }
            Err(e) => assert!(!e.is_empty()),
        }
    }
}
