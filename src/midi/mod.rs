//! MIDI input: listing the input ports, opening one, and decoding its bytes
//! into note messages.
//!
//! Each backend calls a handler on its own thread for every message (WinMM's
//! callback thread, a TINVO thread polling ALSA rawmidi, CoreMIDI's receive
//! thread). The handler must not block; the app's pushes into lock-free
//! mailboxes, one for the audio engine and one for the UI.

#[cfg(target_os = "macos")]
mod coremidi;
#[cfg(target_os = "linux")]
mod rawmidi;
#[cfg(windows)]
mod winmm;

/// A decoded channel message TINVO uses. Everything else (controllers,
/// pitch bend, system messages) is skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    /// A key pressed; `velocity` is 1–127.
    NoteOn { channel: u8, note: u8, velocity: u8 },
    /// A key released (including note-on with velocity 0).
    NoteOff { channel: u8, note: u8 },
}

/// Decodes one complete channel message.
pub fn decode(status: u8, data1: u8, data2: u8) -> Option<Message> {
    let channel = status & 0x0F;
    let (note, velocity) = (data1 & 0x7F, data2 & 0x7F);
    match status & 0xF0 {
        0x90 if velocity > 0 => Some(Message::NoteOn { channel, note, velocity }),
        0x80 | 0x90 => Some(Message::NoteOff { channel, note }),
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

/// Picks the port `query` names: an exact id, else the one port whose name
/// contains `query` (ignoring case).
pub fn find_port<'a>(ports: &'a [Port], query: &str) -> Result<&'a Port, String> {
    if let Some(p) = ports.iter().find(|p| p.id == query) {
        return Ok(p);
    }
    let q = query.to_lowercase();
    let hits: Vec<&Port> = ports.iter().filter(|p| p.name.to_lowercase().contains(&q)).collect();
    match hits[..] {
        [p] => Ok(p),
        [] => Err(format!("no MIDI input matches '{query}' (tinvo midi lists them)")),
        _ => Err(format!(
            "'{query}' matches {} MIDI inputs: {}",
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

#[cfg(windows)]
use winmm as platform;

#[cfg(target_os = "linux")]
use rawmidi as platform;

#[cfg(target_os = "macos")]
use coremidi as platform;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use super::{Message, Port};

    pub struct Handle;

    pub fn inputs() -> Result<Vec<Port>, String> {
        Err("no MIDI backend for this platform".into())
    }

    pub fn open<H: FnMut(Message) + Send + 'static>(_port: &Port, _handler: H) -> Result<Handle, String> {
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
        // Controller (2 bytes), program change (1 byte), pitch bend (2), then a
        // note that relies on none of them.
        assert_eq!(parse(&[0xB0, 7, 100, 0xC0, 5, 0xE0, 0, 64, 0x90, 60, 1]), [on(60, 1)]);
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
        assert_eq!(decode(0xB0, 1, 2), None);
    }

    #[test]
    fn ports_are_found_by_id_or_unique_name() {
        let p = |id: &str, name: &str| Port { id: id.into(), name: name.into() };
        let list = [p("0", "Keystation 49"), p("1", "Launchpad Mini"), p("2", "Keystation Mini 32")];
        assert_eq!(find_port(&list, "1").unwrap().name, "Launchpad Mini");
        assert_eq!(find_port(&list, "launch").unwrap().id, "1");
        assert!(find_port(&list, "keystation").unwrap_err().contains("matches 2"));
        assert!(find_port(&list, "piano").unwrap_err().contains("no MIDI input"));
    }

    /// Exercises the platform listing; a bad FFI declaration would crash here.
    #[test]
    fn listing_inputs_is_consistent() {
        match inputs() {
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
