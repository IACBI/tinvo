//! Plugin windows on X11: a top-level window of TINVO's for each one
//! embedded, on a connection of TINVO's own to the display, through Xlib
//! loaded at run time (`libX11.so.6`), so TINVO needs no X libraries to
//! build or to run without windows.
//!
//! The connection is opened on the main thread when the first window is,
//! and kept. Its descriptor is waited on with the terminal (see
//! [`descriptor`]); [`pump`] reads what the server sent: a close the
//! window manager asks for (`WM_DELETE_WINDOW`) and the sizes a window
//! takes (`ConfigureNotify`). X11 sizes are asked for, not set: a window
//! manager may give another, which comes back as a size like a drag's.

use super::super::host::{GuiMode, ParentWindow, PluginHandle};
use super::Event;
use crate::clap::ffi::ClapGuiResizeHints;
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::OnceLock;

type Display = c_void;
type Window = c_ulong;
type Atom = c_ulong;
type ErrorHandler = unsafe extern "C" fn(*mut Display, *mut c_void) -> c_int;

const LIBRARY: &CStr = c"libX11.so.6";
const RTLD_NOW: c_int = 2;
/// `X.h`.
const STRUCTURE_NOTIFY_MASK: c_long = 1 << 17;
const CONFIGURE_NOTIFY: c_int = 22;
const CLIENT_MESSAGE: c_int = 33;
const PROP_MODE_REPLACE: c_int = 0;
/// `Xutil.h`.
const P_MIN_SIZE: c_long = 1 << 4;
const P_MAX_SIZE: c_long = 1 << 5;
/// The largest side X11 sizes take (a `CARD16`, kept to a signed one).
const MAX_SIDE: u32 = 32767;
/// Events read in one pump, so a flood cannot keep the terminal waiting.
const MAX_EVENTS: usize = 256;

unsafe extern "C" {
    fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

/// `XConfigureEvent`, as `Xlib.h` declares it.
#[repr(C)]
#[derive(Clone, Copy)]
struct ConfigureEvent {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    event: Window,
    window: Window,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    border_width: c_int,
    above: Window,
    override_redirect: c_int,
}

/// `XClientMessageEvent`, its data as five longs.
#[repr(C)]
#[derive(Clone, Copy)]
struct ClientMessageEvent {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    window: Window,
    message_type: Atom,
    format: c_int,
    data: [c_long; 5],
}

/// `XEvent`: 24 longs, read as the kind its first field says.
#[repr(C)]
union XEvent {
    kind: c_int,
    configure: ConfigureEvent,
    client: ClientMessageEvent,
    pad: [c_long; 24],
}

/// `XSizeHints`.
#[repr(C)]
#[derive(Default)]
struct SizeHints {
    flags: c_long,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    min_width: c_int,
    min_height: c_int,
    max_width: c_int,
    max_height: c_int,
    width_inc: c_int,
    height_inc: c_int,
    min_aspect: (c_int, c_int),
    max_aspect: (c_int, c_int),
    base_width: c_int,
    base_height: c_int,
    win_gravity: c_int,
}

/// The Xlib functions TINVO calls, as `Xlib.h` and `Xutil.h` declare them.
struct Lib {
    open_display: unsafe extern "C" fn(*const c_char) -> *mut Display,
    default_screen: unsafe extern "C" fn(*mut Display) -> c_int,
    root_window: unsafe extern "C" fn(*mut Display, c_int) -> Window,
    black_pixel: unsafe extern "C" fn(*mut Display, c_int) -> c_ulong,
    create_simple_window:
        unsafe extern "C" fn(*mut Display, Window, c_int, c_int, c_uint, c_uint, c_uint, c_ulong, c_ulong) -> Window,
    select_input: unsafe extern "C" fn(*mut Display, Window, c_long) -> c_int,
    intern_atom: unsafe extern "C" fn(*mut Display, *const c_char, c_int) -> Atom,
    set_wm_protocols: unsafe extern "C" fn(*mut Display, Window, *mut Atom, c_int) -> c_int,
    store_name: unsafe extern "C" fn(*mut Display, Window, *const c_char) -> c_int,
    change_property: unsafe extern "C" fn(*mut Display, Window, Atom, Atom, c_int, c_int, *const u8, c_int) -> c_int,
    set_wm_normal_hints: unsafe extern "C" fn(*mut Display, Window, *mut SizeHints),
    map_window: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    unmap_window: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    destroy_window: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    resize_window: unsafe extern "C" fn(*mut Display, Window, c_uint, c_uint) -> c_int,
    pending: unsafe extern "C" fn(*mut Display) -> c_int,
    next_event: unsafe extern "C" fn(*mut Display, *mut XEvent) -> c_int,
    sync: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    connection_number: unsafe extern "C" fn(*mut Display) -> c_int,
    get_default: unsafe extern "C" fn(*mut Display, *const c_char, *const c_char) -> *mut c_char,
    set_error_handler: unsafe extern "C" fn(Option<ErrorHandler>) -> Option<ErrorHandler>,
}

/// Looks `name` up in the loaded library as a function of type `T`.
unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &CStr) -> Result<T, String> {
    // SAFETY: a handle from dlopen and a NUL-terminated name.
    let p = unsafe { dlsym(handle, name.as_ptr()) };
    if p.is_null() {
        return Err(format!("{} has no {}", LIBRARY.to_string_lossy(), name.to_string_lossy()));
    }
    assert_eq!(size_of::<T>(), size_of::<*mut c_void>());
    // SAFETY: T is a function pointer type matching the C declaration.
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

fn lib() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: a NUL-terminated name; the library is never closed, so
        // its functions stay valid for the life of the process.
        let handle = unsafe { dlopen(LIBRARY.as_ptr(), RTLD_NOW) };
        if handle.is_null() {
            return Err(format!("X11 is not installed ({} not found)", LIBRARY.to_string_lossy()));
        }
        // SAFETY: each symbol is looked up with the type its header declares.
        unsafe {
            Ok(Lib {
                open_display: symbol(handle, c"XOpenDisplay")?,
                default_screen: symbol(handle, c"XDefaultScreen")?,
                root_window: symbol(handle, c"XRootWindow")?,
                black_pixel: symbol(handle, c"XBlackPixel")?,
                create_simple_window: symbol(handle, c"XCreateSimpleWindow")?,
                select_input: symbol(handle, c"XSelectInput")?,
                intern_atom: symbol(handle, c"XInternAtom")?,
                set_wm_protocols: symbol(handle, c"XSetWMProtocols")?,
                store_name: symbol(handle, c"XStoreName")?,
                change_property: symbol(handle, c"XChangeProperty")?,
                set_wm_normal_hints: symbol(handle, c"XSetWMNormalHints")?,
                map_window: symbol(handle, c"XMapWindow")?,
                unmap_window: symbol(handle, c"XUnmapWindow")?,
                destroy_window: symbol(handle, c"XDestroyWindow")?,
                resize_window: symbol(handle, c"XResizeWindow")?,
                pending: symbol(handle, c"XPending")?,
                next_event: symbol(handle, c"XNextEvent")?,
                sync: symbol(handle, c"XSync")?,
                connection_number: symbol(handle, c"XConnectionNumber")?,
                get_default: symbol(handle, c"XGetDefault")?,
                set_error_handler: symbol(handle, c"XSetErrorHandler")?,
            })
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Xlib's default answer to an error ends the process; a window gone
/// before a request about it reached the server is no reason to.
unsafe extern "C" fn ignore_error(_: *mut Display, _: *mut c_void) -> c_int {
    0
}

/// TINVO's connection to the display, and the atoms it uses.
#[derive(Clone, Copy)]
struct Connection {
    lib: &'static Lib,
    display: *mut Display,
    protocols: Atom,
    delete_window: Atom,
    net_wm_name: Atom,
    utf8_string: Atom,
}

thread_local! {
    /// Opened with the first window.
    static CONNECTION: Cell<Option<Connection>> = const { Cell::new(None) };
    /// TINVO's windows of this thread, each by the instance whose plugin
    /// window it holds.
    static WINDOWS: RefCell<Vec<(Window, u64)>> = const { RefCell::new(Vec::new()) };
    /// What the server sent for windows another set of windows owns, kept
    /// for its pump.
    static EVENTS: RefCell<Vec<(Window, Event)>> = const { RefCell::new(Vec::new()) };
}

fn connection() -> Result<Connection, String> {
    if let Some(c) = CONNECTION.get() {
        return Ok(c);
    }
    let lib = lib()?;
    // SAFETY: Xlib's own functions; the display named by `DISPLAY`, and
    // NUL-terminated atom names.
    let c = unsafe {
        let display = (lib.open_display)(std::ptr::null());
        if display.is_null() {
            return Err("there is no X display to open plugin windows on".into());
        }
        (lib.set_error_handler)(Some(ignore_error));
        let atom = |name: &CStr| (lib.intern_atom)(display, name.as_ptr(), 0);
        Connection {
            lib,
            display,
            protocols: atom(c"WM_PROTOCOLS"),
            delete_window: atom(c"WM_DELETE_WINDOW"),
            net_wm_name: atom(c"_NET_WM_NAME"),
            utf8_string: atom(c"UTF8_STRING"),
        }
    };
    CONNECTION.set(Some(c));
    Ok(c)
}

/// Nothing to ready: the connection is opened with the first window.
pub(super) fn init() {}

/// The descriptor of TINVO's connection to the display, once opened: the
/// main loop wakes when the server sends something.
pub(super) fn descriptor() -> Option<i32> {
    // SAFETY: an open display.
    CONNECTION.get().map(|c| unsafe { (c.lib.connection_number)(c.display) })
}

/// Opens `handle`'s plugin window with the X11 API.
pub(super) fn open(handle: &PluginHandle, api: &'static CStr, title: &str) -> Result<GuiMode, String> {
    let token = handle.token();
    handle.gui_open(api, title, || Parent::new(token, title).map(|p| Box::new(p) as Box<dyn ParentWindow>))
}

/// Lets go of the window of the instance with `token`.
pub(super) fn forget(token: u64) {
    WINDOWS.with(|w| {
        let w = w.borrow();
        let gone: Vec<Window> = w.iter().filter(|(_, t)| *t == token).map(|(win, _)| *win).collect();
        EVENTS.with(|e| e.borrow_mut().retain(|(win, _)| !gone.contains(win)));
    });
}

/// Reads what the server sent, a bounded number of events, and returns
/// what it says of the windows of the instances `owns` says are the
/// caller's; the rest wait for theirs.
pub(super) fn pump(owns: impl Fn(u64) -> bool) -> Vec<Event> {
    let Some(c) = CONNECTION.get() else { return Vec::new() };
    let mut read = Vec::new();
    let mut ev = XEvent { pad: [0; 24] };
    for _ in 0..MAX_EVENTS {
        // SAFETY: an open display, and room for one event.
        if unsafe { (c.lib.pending)(c.display) } <= 0 {
            break;
        }
        // SAFETY: as above; an event is waiting, so this does not block.
        unsafe { (c.lib.next_event)(c.display, &mut ev) };
        // SAFETY: every event starts with its kind, and each kind read is
        // the structure Xlib wrote.
        // A size taken, or `None` for a close asked for.
        let (window, size) = unsafe {
            match ev.kind {
                CONFIGURE_NOTIFY => {
                    let e = ev.configure;
                    (e.window, Some((e.width.max(1) as u32, e.height.max(1) as u32)))
                }
                CLIENT_MESSAGE => {
                    let e = ev.client;
                    if e.message_type != c.protocols || e.format != 32 || e.data[0] as Atom != c.delete_window {
                        continue;
                    }
                    (e.window, None)
                }
                _ => continue,
            }
        };
        let Some(token) = WINDOWS.with(|w| w.borrow().iter().find(|(win, _)| *win == window).map(|(_, t)| *t)) else {
            continue;
        };
        let event = match size {
            Some((w, h)) => Event::Resized(token, w, h),
            None => Event::Closed(token),
        };
        read.push((window, event));
    }
    EVENTS.with(|e| {
        let mut e = e.borrow_mut();
        e.extend(read);
        let (mine, theirs): (Vec<_>, Vec<_>) = e.drain(..).partition(|(_, event)| owns(event.token()));
        *e = theirs;
        mine.into_iter().map(|(_, event)| event).collect()
    })
}

/// A top-level window holding one plugin window.
struct Parent {
    c: Connection,
    window: Window,
    /// The size last asked for.
    size: (u32, u32),
    /// Which ways the user may resize it: (across, up and down).
    axes: (bool, bool),
}

impl Parent {
    fn new(token: u64, title: &str) -> Result<Self, String> {
        let c = connection()?;
        // SAFETY: an open display; the window is TINVO's, its name
        // NUL-terminated and its UTF-8 title of the length given.
        let window = unsafe {
            let screen = (c.lib.default_screen)(c.display);
            let root = (c.lib.root_window)(c.display, screen);
            let black = (c.lib.black_pixel)(c.display, screen);
            let window = (c.lib.create_simple_window)(c.display, root, 0, 0, 1, 1, 0, black, black);
            if window == 0 {
                return Err("could not make a window".into());
            }
            (c.lib.select_input)(c.display, window, STRUCTURE_NOTIFY_MASK);
            let mut delete = c.delete_window;
            (c.lib.set_wm_protocols)(c.display, window, &mut delete, 1);
            let latin: String = title.chars().map(|ch| if ch.is_ascii() && ch != '\0' { ch } else { '?' }).collect();
            if let Ok(name) = std::ffi::CString::new(latin) {
                (c.lib.store_name)(c.display, window, name.as_ptr());
            }
            let len = c_int::try_from(title.len()).unwrap_or(0);
            (c.lib.change_property)(
                c.display,
                window,
                c.net_wm_name,
                c.utf8_string,
                8,
                PROP_MODE_REPLACE,
                title.as_ptr(),
                len,
            );
            window
        };
        WINDOWS.with(|w| w.borrow_mut().push((window, token)));
        Ok(Self { c, window, size: (1, 1), axes: (false, false) })
    }

    /// Tells the window manager which ways the user may resize it: the
    /// others are held at the size asked for.
    fn hint(&self) {
        let (w, h) = (self.size.0 as c_int, self.size.1 as c_int);
        let mut hints = SizeHints { flags: P_MIN_SIZE | P_MAX_SIZE, ..SizeHints::default() };
        let side = MAX_SIDE as c_int;
        (hints.min_width, hints.max_width) = if self.axes.0 { (1, side) } else { (w, w) };
        (hints.min_height, hints.max_height) = if self.axes.1 { (1, side) } else { (h, h) };
        // SAFETY: an open display, a window of its own, and hints in full.
        unsafe { (self.c.lib.set_wm_normal_hints)(self.c.display, self.window, &mut hints) };
    }

    /// Waits until the server has done what was asked: the plugin's
    /// window, on a connection of its own, is asked about it next.
    fn sync(&self) {
        // SAFETY: an open display.
        unsafe { (self.c.lib.sync)(self.c.display, 0) };
    }
}

impl ParentWindow for Parent {
    fn handle(&self) -> *mut c_void {
        self.window as *mut c_void
    }

    /// The scale `Xft.dpi` gives, as desktops set it; 1 without it.
    fn scale(&self) -> Option<f64> {
        // SAFETY: an open display and NUL-terminated names; the result is
        // Xlib's, NUL-terminated, or null.
        let dpi = unsafe {
            let v = (self.c.lib.get_default)(self.c.display, c"Xft".as_ptr(), c"dpi".as_ptr());
            (!v.is_null()).then(|| CStr::from_ptr(v).to_string_lossy().trim().parse::<f64>().ok()).flatten()
        };
        Some(dpi.filter(|d| d.is_finite() && *d > 0.0).map_or(1.0, |d| d / 96.0))
    }

    /// Asked of the server, which gives it unless a window manager says
    /// otherwise; what it gives comes back as a size taken.
    fn resize(&mut self, width: u32, height: u32) -> bool {
        if !(1..=MAX_SIDE).contains(&width) || !(1..=MAX_SIDE).contains(&height) {
            return false;
        }
        self.size = (width, height);
        self.hint();
        // SAFETY: an open display and a window of its own.
        unsafe { (self.c.lib.resize_window)(self.c.display, self.window, width, height) };
        self.sync();
        true
    }

    fn set_resizable(&mut self, hints: Option<ClapGuiResizeHints>) {
        self.axes = hints.map_or((false, false), |h| (h.can_resize_horizontally, h.can_resize_vertically));
        self.hint();
    }

    fn show(&mut self) {
        // SAFETY: an open display and a window of its own.
        unsafe { (self.c.lib.map_window)(self.c.display, self.window) };
        self.sync();
    }

    fn hide(&mut self) {
        // SAFETY: as above.
        unsafe { (self.c.lib.unmap_window)(self.c.display, self.window) };
        self.sync();
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        let window = self.window;
        WINDOWS.with(|w| w.borrow_mut().retain(|(win, _)| *win != window));
        // What it was sent and was not served is not for a window opened
        // after it for the same plugin.
        EVENTS.with(|e| {
            if let Ok(mut e) = e.try_borrow_mut() {
                e.retain(|(win, _)| *win != window);
            }
        });
        // SAFETY: an open display and a window of its own, destroyed once.
        unsafe { (self.c.lib.destroy_window)(self.c.display, window) };
        self.sync();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_laid_out_as_xlib_s() {
        assert_eq!(size_of::<XEvent>(), 24 * size_of::<c_long>());
        assert_eq!(std::mem::offset_of!(ConfigureEvent, window), 40);
        assert_eq!(std::mem::offset_of!(ConfigureEvent, width), 56);
        assert_eq!(std::mem::offset_of!(ClientMessageEvent, message_type), 40);
        assert_eq!(std::mem::offset_of!(ClientMessageEvent, data), 56);
        assert_eq!(size_of::<SizeHints>(), 80);
    }
}
