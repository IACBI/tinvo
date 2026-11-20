//! Plugins' windows on the system's own windowing: a window of TINVO's for
//! each plugin window it embeds, the events the system sends them, and
//! the window messages a plugin's code on the main thread relies on.
//!
//! [`Windows`] opens a plugin's window through its instance's handle (see
//! [`crate::clap::host::PluginHandle::gui_open`]) and keeps it until it is
//! closed by the user, by the plugin, through [`Windows::close`] or with
//! the set. [`Windows::pump`] runs on the main thread between the
//! terminal's waits: it hands out the messages waiting for the thread's
//! windows (the plugins' own among them), then serves what TINVO's windows
//! recorded, a close or a new size, through each plugin's handle.

use super::host::{GuiMode, PluginHandle};
use std::ffi::CStr;

#[cfg(windows)]
mod win32;
#[cfg(windows)]
use win32 as platform;

/// Where TINVO opens no plugin windows yet.
#[cfg(not(windows))]
mod platform {
    use super::{Event, GuiMode, PluginHandle};
    use std::ffi::CStr;

    pub(super) fn init() {}

    pub(super) fn open(_: &PluginHandle, _: &'static CStr, _: &str) -> Result<GuiMode, String> {
        Err("TINVO cannot open plugin windows on this system yet".into())
    }

    pub(super) fn pump(_: impl Fn(u64) -> bool) -> Vec<Event> {
        Vec::new()
    }

    pub(super) fn forget(_: u64) {}
}

/// What a window of TINVO's recorded for the plugin window in it.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Event {
    /// The user closed the window of the instance with this token.
    Closed(u64),
    /// Its client area took this size.
    Resized(u64, u32, u32),
    /// It moved to a monitor of this scale.
    Scaled(u64, f64),
}

impl Event {
    /// The token of the instance whose window recorded it.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn token(&self) -> u64 {
        match *self {
            Event::Closed(t) | Event::Resized(t, ..) | Event::Scaled(t, _) => t,
        }
    }
}

/// The plugin windows open, each by its instance. Made on the main thread,
/// which plays CLAP's, and its methods run there.
pub struct Windows {
    open: Vec<PluginHandle>,
}

impl Default for Windows {
    fn default() -> Self {
        Self::new()
    }
}

impl Windows {
    /// Readies the thread for plugin windows (on Windows, its DPI
    /// awareness), before the first is opened.
    pub fn new() -> Self {
        platform::init();
        Self { open: Vec::new() }
    }

    /// The window API plugin windows use on this system, if TINVO can open
    /// them here.
    pub fn api() -> Option<&'static CStr> {
        #[cfg(windows)]
        return Some(super::ffi::CLAP_WINDOW_API_WIN32);
        #[cfg(not(windows))]
        None
    }

    /// Whether the window of the instance with `token` is open.
    pub fn is_open(&self, token: u64) -> bool {
        self.open.iter().any(|h| h.token() == token)
    }

    /// Opens the window of `handle`'s plugin, titled `title`, floating if
    /// it can float one itself and otherwise in a window of TINVO's; a
    /// window open already stays as it is.
    pub fn open(&mut self, handle: PluginHandle, title: &str) -> Result<GuiMode, String> {
        if let Some(mode) = handle.gui_mode().filter(|_| self.is_open(handle.token())) {
            return Ok(mode);
        }
        let api = Self::api().ok_or("TINVO cannot open plugin windows on this system yet")?;
        let mode = platform::open(&handle, api, title)?;
        self.open.retain(|h| h.token() != handle.token());
        self.open.push(handle);
        Ok(mode)
    }

    /// Closes the window of the instance with `token`, if open.
    pub fn close(&mut self, token: u64) {
        if let Some(i) = self.open.iter().position(|h| h.token() == token) {
            self.open.remove(i).gui_close();
        }
        platform::forget(token);
    }

    /// Closes every window.
    pub fn close_all(&mut self) {
        for h in self.open.drain(..) {
            h.gui_close();
            platform::forget(h.token());
        }
    }

    /// Hands out the window messages waiting for this thread, then serves
    /// what TINVO's windows recorded: a close the user asked for, closing
    /// the plugin's window, and a new size, offered to the plugin. Returns
    /// the tokens of the windows the user closed. Windows whose plugin
    /// closed them, or went, are let go of.
    pub fn pump(&mut self) -> Vec<u64> {
        let mut events = platform::pump(|token| self.is_open(token));
        // Only the last size of each window: the ones before are past.
        let mut i = events.len();
        while i > 0 {
            i -= 1;
            if let Event::Resized(token, ..) = events[i] {
                let later = events[i + 1..].iter().any(|e| matches!(e, Event::Resized(t, ..) if *t == token));
                if later {
                    events.remove(i);
                }
            }
        }
        let mut closed = Vec::new();
        for event in events {
            match event {
                Event::Closed(token) => {
                    if self.is_open(token) {
                        self.close(token);
                        closed.push(token);
                    }
                }
                Event::Resized(token, w, h) => {
                    if let Some(handle) = self.open.iter().find(|h| h.token() == token) {
                        handle.gui_resized(w, h);
                    }
                }
                Event::Scaled(token, scale) => {
                    if let Some(handle) = self.open.iter().find(|h| h.token() == token) {
                        handle.gui_rescaled(scale);
                    }
                }
            }
        }
        self.open.retain(|h| {
            let open = h.gui_mode().is_some();
            if !open {
                platform::forget(h.token());
            }
            open
        });
        closed
    }
}

impl Drop for Windows {
    fn drop(&mut self) {
        self.close_all();
    }
}
