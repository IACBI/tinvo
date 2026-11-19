//! The CLAP 1.2 types TINVO uses, written by hand from the headers of
//! CLAP 1.2.10 (MIT licence, github.com/free-audio/clap), field for field
//! and in the same order. `CLAP_ABI` is `__cdecl` on Windows, which is the
//! C calling convention there; C's `bool` is one byte, as Rust's.
//!
//! Every function pointer is an `Option`: a null one from the other side
//! is refused before it is called, never called.

use std::ffi::{c_char, c_void};

/// `clap_version_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClapVersion {
    pub major: u32,
    pub minor: u32,
    pub revision: u32,
}

/// The version this host and the test plugins are written against.
pub const CLAP_VERSION: ClapVersion = ClapVersion { major: 1, minor: 2, revision: 10 };

/// `clap_version_is_compatible`: every 1.x is.
pub fn clap_version_is_compatible(v: ClapVersion) -> bool {
    v.major >= 1
}

/// `CLAP_NAME_SIZE`.
pub const CLAP_NAME_SIZE: usize = 256;

/// `clap_id`, and `CLAP_INVALID_ID`.
pub type ClapId = u32;
pub const CLAP_INVALID_ID: ClapId = u32::MAX;

/// `clap_plugin_entry_t`, exported by a plugin as the data symbol
/// `clap_entry`.
#[repr(C)]
pub struct ClapPluginEntry {
    pub clap_version: ClapVersion,
    pub init: Option<unsafe extern "C" fn(plugin_path: *const c_char) -> bool>,
    pub deinit: Option<unsafe extern "C" fn()>,
    pub get_factory: Option<unsafe extern "C" fn(factory_id: *const c_char) -> *const c_void>,
}

/// `CLAP_PLUGIN_FACTORY_ID`.
pub const CLAP_PLUGIN_FACTORY_ID: &std::ffi::CStr = c"clap.plugin-factory";

/// `clap_plugin_factory_t`.
#[repr(C)]
pub struct ClapPluginFactory {
    pub get_plugin_count: Option<unsafe extern "C" fn(factory: *const ClapPluginFactory) -> u32>,
    pub get_plugin_descriptor:
        Option<unsafe extern "C" fn(factory: *const ClapPluginFactory, index: u32) -> *const ClapPluginDescriptor>,
    pub create_plugin: Option<
        unsafe extern "C" fn(
            factory: *const ClapPluginFactory,
            host: *const ClapHost,
            plugin_id: *const c_char,
        ) -> *const ClapPlugin,
    >,
}

/// `clap_plugin_descriptor_t`. `features` is a null-terminated array.
#[repr(C)]
pub struct ClapPluginDescriptor {
    pub clap_version: ClapVersion,
    pub id: *const c_char,
    pub name: *const c_char,
    pub vendor: *const c_char,
    pub url: *const c_char,
    pub manual_url: *const c_char,
    pub support_url: *const c_char,
    pub version: *const c_char,
    pub description: *const c_char,
    pub features: *const *const c_char,
}

/// `CLAP_PLUGIN_FEATURE_INSTRUMENT` and `CLAP_PLUGIN_FEATURE_AUDIO_EFFECT`.
pub const CLAP_PLUGIN_FEATURE_INSTRUMENT: &std::ffi::CStr = c"instrument";
pub const CLAP_PLUGIN_FEATURE_AUDIO_EFFECT: &std::ffi::CStr = c"audio-effect";

/// `clap_plugin_t`.
#[repr(C)]
pub struct ClapPlugin {
    pub desc: *const ClapPluginDescriptor,
    pub plugin_data: *mut c_void,
    pub init: Option<unsafe extern "C" fn(plugin: *const ClapPlugin) -> bool>,
    pub destroy: Option<unsafe extern "C" fn(plugin: *const ClapPlugin)>,
    pub activate: Option<
        unsafe extern "C" fn(
            plugin: *const ClapPlugin,
            sample_rate: f64,
            min_frames_count: u32,
            max_frames_count: u32,
        ) -> bool,
    >,
    pub deactivate: Option<unsafe extern "C" fn(plugin: *const ClapPlugin)>,
    pub start_processing: Option<unsafe extern "C" fn(plugin: *const ClapPlugin) -> bool>,
    pub stop_processing: Option<unsafe extern "C" fn(plugin: *const ClapPlugin)>,
    pub reset: Option<unsafe extern "C" fn(plugin: *const ClapPlugin)>,
    pub process:
        Option<unsafe extern "C" fn(plugin: *const ClapPlugin, process: *const ClapProcess) -> ClapProcessStatus>,
    pub get_extension: Option<unsafe extern "C" fn(plugin: *const ClapPlugin, id: *const c_char) -> *const c_void>,
    pub on_main_thread: Option<unsafe extern "C" fn(plugin: *const ClapPlugin)>,
}

/// `clap_host_t`.
#[repr(C)]
pub struct ClapHost {
    pub clap_version: ClapVersion,
    pub host_data: *mut c_void,
    pub name: *const c_char,
    pub vendor: *const c_char,
    pub url: *const c_char,
    pub version: *const c_char,
    pub get_extension:
        Option<unsafe extern "C" fn(host: *const ClapHost, extension_id: *const c_char) -> *const c_void>,
    pub request_restart: Option<unsafe extern "C" fn(host: *const ClapHost)>,
    pub request_process: Option<unsafe extern "C" fn(host: *const ClapHost)>,
    pub request_callback: Option<unsafe extern "C" fn(host: *const ClapHost)>,
}

/// `clap_process_status` and its values.
pub type ClapProcessStatus = i32;
pub const CLAP_PROCESS_ERROR: ClapProcessStatus = 0;
pub const CLAP_PROCESS_CONTINUE: ClapProcessStatus = 1;
pub const CLAP_PROCESS_CONTINUE_IF_NOT_QUIET: ClapProcessStatus = 2;
pub const CLAP_PROCESS_TAIL: ClapProcessStatus = 3;
pub const CLAP_PROCESS_SLEEP: ClapProcessStatus = 4;

/// `clap_audio_buffer_t`.
#[repr(C)]
pub struct ClapAudioBuffer {
    pub data32: *mut *mut f32,
    pub data64: *mut *mut f64,
    pub channel_count: u32,
    pub latency: u32,
    pub constant_mask: u64,
}

/// `clap_process_t`. The transport is the opaque `clap_event_transport_t`
/// until a part of TINVO sends one.
#[repr(C)]
pub struct ClapProcess {
    pub steady_time: i64,
    pub frames_count: u32,
    pub transport: *const c_void,
    pub audio_inputs: *const ClapAudioBuffer,
    pub audio_outputs: *mut ClapAudioBuffer,
    pub audio_inputs_count: u32,
    pub audio_outputs_count: u32,
    pub in_events: *const ClapInputEvents,
    pub out_events: *const ClapOutputEvents,
}

/// `clap_event_header_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClapEventHeader {
    pub size: u32,
    pub time: u32,
    pub space_id: u16,
    pub kind: u16,
    pub flags: u32,
}

/// `CLAP_CORE_EVENT_SPACE_ID` and the core event types TINVO uses.
pub const CLAP_CORE_EVENT_SPACE_ID: u16 = 0;
pub const CLAP_EVENT_NOTE_ON: u16 = 0;
pub const CLAP_EVENT_NOTE_OFF: u16 = 1;

/// `clap_event_note_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClapEventNote {
    pub header: ClapEventHeader,
    pub note_id: i32,
    pub port_index: i16,
    pub channel: i16,
    pub key: i16,
    pub velocity: f64,
}

/// `clap_input_events_t`.
#[repr(C)]
pub struct ClapInputEvents {
    pub ctx: *mut c_void,
    pub size: Option<unsafe extern "C" fn(list: *const ClapInputEvents) -> u32>,
    pub get: Option<unsafe extern "C" fn(list: *const ClapInputEvents, index: u32) -> *const ClapEventHeader>,
}

/// `clap_output_events_t`.
#[repr(C)]
pub struct ClapOutputEvents {
    pub ctx: *mut c_void,
    pub try_push: Option<unsafe extern "C" fn(list: *const ClapOutputEvents, event: *const ClapEventHeader) -> bool>,
}

/// `CLAP_EXT_AUDIO_PORTS`.
pub const CLAP_EXT_AUDIO_PORTS: &std::ffi::CStr = c"clap.audio-ports";

/// `clap_audio_port_info_t`.
#[repr(C)]
pub struct ClapAudioPortInfo {
    pub id: ClapId,
    pub name: [c_char; CLAP_NAME_SIZE],
    pub flags: u32,
    pub channel_count: u32,
    pub port_type: *const c_char,
    pub in_place_pair: ClapId,
}

/// `CLAP_AUDIO_PORT_IS_MAIN`.
pub const CLAP_AUDIO_PORT_IS_MAIN: u32 = 1;

/// `clap_plugin_audio_ports_t`.
#[repr(C)]
pub struct ClapPluginAudioPorts {
    pub count: Option<unsafe extern "C" fn(plugin: *const ClapPlugin, is_input: bool) -> u32>,
    pub get: Option<
        unsafe extern "C" fn(
            plugin: *const ClapPlugin,
            index: u32,
            is_input: bool,
            info: *mut ClapAudioPortInfo,
        ) -> bool,
    >,
}

/// `CLAP_EXT_NOTE_PORTS`.
pub const CLAP_EXT_NOTE_PORTS: &std::ffi::CStr = c"clap.note-ports";

/// `clap_note_port_info_t`.
#[repr(C)]
pub struct ClapNotePortInfo {
    pub id: ClapId,
    pub supported_dialects: u32,
    pub preferred_dialect: u32,
    pub name: [c_char; CLAP_NAME_SIZE],
}

/// `CLAP_NOTE_DIALECT_CLAP` and `CLAP_NOTE_DIALECT_MIDI`.
pub const CLAP_NOTE_DIALECT_CLAP: u32 = 1;
pub const CLAP_NOTE_DIALECT_MIDI: u32 = 2;

/// `clap_plugin_note_ports_t`.
#[repr(C)]
pub struct ClapPluginNotePorts {
    pub count: Option<unsafe extern "C" fn(plugin: *const ClapPlugin, is_input: bool) -> u32>,
    pub get: Option<
        unsafe extern "C" fn(
            plugin: *const ClapPlugin,
            index: u32,
            is_input: bool,
            info: *mut ClapNotePortInfo,
        ) -> bool,
    >,
}

/// `CLAP_EVENT_MIDI`, and `clap_event_midi_t`.
pub const CLAP_EVENT_MIDI: u16 = 10;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClapEventMidi {
    pub header: ClapEventHeader,
    pub port_index: u16,
    pub data: [u8; 3],
}

/// `CLAP_EXT_THREAD_CHECK`.
pub const CLAP_EXT_THREAD_CHECK: &std::ffi::CStr = c"clap.thread-check";

/// `clap_host_thread_check_t`.
#[repr(C)]
pub struct ClapHostThreadCheck {
    pub is_main_thread: Option<unsafe extern "C" fn(host: *const ClapHost) -> bool>,
    pub is_audio_thread: Option<unsafe extern "C" fn(host: *const ClapHost) -> bool>,
}

/// `CLAP_EXT_RENDER`, and the render modes.
pub const CLAP_EXT_RENDER: &std::ffi::CStr = c"clap.render";
pub const CLAP_RENDER_REALTIME: i32 = 0;
pub const CLAP_RENDER_OFFLINE: i32 = 1;

/// `clap_plugin_render_t`.
#[repr(C)]
pub struct ClapPluginRender {
    pub has_hard_realtime_requirement: Option<unsafe extern "C" fn(plugin: *const ClapPlugin) -> bool>,
    pub set: Option<unsafe extern "C" fn(plugin: *const ClapPlugin, mode: i32) -> bool>,
}

#[cfg(all(test, target_pointer_width = "64"))]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// The sizes and offsets the C compilers give these types on 64-bit
    /// systems, worked out from the headers by hand.
    #[test]
    fn the_layouts_match_the_c_headers() {
        assert_eq!(size_of::<ClapVersion>(), 12);
        assert_eq!(size_of::<ClapPluginEntry>(), 40);
        assert_eq!(offset_of!(ClapPluginEntry, init), 16);
        assert_eq!(size_of::<ClapPluginFactory>(), 24);
        assert_eq!(size_of::<ClapPluginDescriptor>(), 88);
        assert_eq!(offset_of!(ClapPluginDescriptor, features), 80);
        assert_eq!(size_of::<ClapPlugin>(), 96);
        assert_eq!(offset_of!(ClapPlugin, on_main_thread), 88);
        assert_eq!(size_of::<ClapHost>(), 88);
        assert_eq!(offset_of!(ClapHost, get_extension), 56);
        assert_eq!(size_of::<ClapAudioBuffer>(), 32);
        assert_eq!(size_of::<ClapProcess>(), 64);
        assert_eq!(offset_of!(ClapProcess, transport), 16);
        assert_eq!(offset_of!(ClapProcess, audio_inputs_count), 40);
        assert_eq!(size_of::<ClapEventHeader>(), 16);
        assert_eq!(offset_of!(ClapEventHeader, kind), 10);
        assert_eq!(size_of::<ClapEventNote>(), 40);
        assert_eq!(offset_of!(ClapEventNote, key), 24);
        assert_eq!(offset_of!(ClapEventNote, velocity), 32);
        assert_eq!(size_of::<ClapInputEvents>(), 24);
        assert_eq!(size_of::<ClapOutputEvents>(), 16);
        assert_eq!(size_of::<ClapAudioPortInfo>(), 288);
        assert_eq!(offset_of!(ClapAudioPortInfo, port_type), 272);
        assert_eq!(size_of::<ClapPluginAudioPorts>(), 16);
        assert_eq!(size_of::<ClapNotePortInfo>(), 268);
        assert_eq!(size_of::<ClapEventMidi>(), 24);
        assert_eq!(offset_of!(ClapEventMidi, data), 18);
        assert_eq!(size_of::<ClapHostThreadCheck>(), 16);
        assert_eq!(size_of::<ClapPluginRender>(), 16);
        assert_eq!(size_of::<Option<unsafe extern "C" fn()>>(), 8, "a null pointer is None");
    }
}
