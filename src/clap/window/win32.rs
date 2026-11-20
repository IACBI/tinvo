//! Plugin windows on Windows: a top-level window of TINVO's for each one
//! embedded, and the thread's message pump. The thread is made per-monitor
//! DPI aware: Windows puts a window in another only when both were made
//! aware alike, and sizes given or read by a thread aware otherwise than
//! the window are scaled, for TINVO's calls and the plugin's alike.
//!
//! A window's procedure mostly records what happened (closed, sized, moved
//! to a monitor of another scale) for [`pump`] to serve afterwards: Windows
//! calls it from inside `SetWindowPos`, where the host holds the window's
//! state, and a close the user asks for leaves the window standing until
//! its plugin's window is destroyed first. While the user drags its border,
//! though, Windows runs a loop of its own inside one message, so the sizes
//! the drag makes go to the plugin from the procedure, never from inside a
//! size TINVO set itself.

use super::super::host::{GuiMode, ParentWindow, PluginHandle};
use super::Event;
use crate::clap::ffi::ClapGuiResizeHints;
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};
use std::ptr::{null, null_mut};
use std::sync::OnceLock;

type Hwnd = *mut c_void;
type Handle = *mut c_void;
type WndProc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;

#[repr(C)]
struct WndClassExW {
    size: u32,
    style: u32,
    wnd_proc: Option<WndProc>,
    cls_extra: i32,
    wnd_extra: i32,
    instance: Handle,
    icon: Handle,
    cursor: Handle,
    background: Handle,
    menu_name: *const u16,
    class_name: *const u16,
    icon_small: Handle,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    x: i32,
    y: i32,
    private: u32,
}

#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct MinMaxInfo {
    reserved: Point,
    max_size: Point,
    max_position: Point,
    min_track: Point,
    max_track: Point,
}

const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_MINIMIZEBOX: u32 = 0x0002_0000;
const WS_MAXIMIZEBOX: u32 = 0x0001_0000;
const WS_THICKFRAME: u32 = 0x0004_0000;
const WS_CLIPCHILDREN: u32 = 0x0200_0000;
const FIXED: u32 = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
const CW_USEDEFAULT: i32 = 0x8000_0000_u32 as i32;
const GWL_STYLE: i32 = -16;
const GWLP_USERDATA: i32 = -21;
const SWP_NOSIZE: u32 = 0x1;
const SWP_NOMOVE: u32 = 0x2;
const SWP_NOZORDER: u32 = 0x4;
const SWP_NOACTIVATE: u32 = 0x10;
const SWP_FRAMECHANGED: u32 = 0x20;
const SW_HIDE: i32 = 0;
const SW_SHOWNORMAL: i32 = 1;
const PM_REMOVE: u32 = 1;
const WM_SIZE: u32 = 0x0005;
const WM_CLOSE: u32 = 0x0010;
const WM_QUIT: u32 = 0x0012;
const WM_GETMINMAXINFO: u32 = 0x0024;
const WM_ENTERSIZEMOVE: u32 = 0x0231;
const WM_EXITSIZEMOVE: u32 = 0x0232;
const WM_DPICHANGED: u32 = 0x02E0;
const SIZE_MINIMIZED: usize = 1;
const IDC_ARROW: usize = 32512;
const COLOR_WINDOW: usize = 5;
/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`.
const PER_MONITOR_V2: isize = -4;
/// The largest a window is let be (Windows' own limit for a coordinate).
const MAX_SIDE: i32 = 32767;
/// At most this many messages a pump: a flood of them still lets the
/// terminal be served.
const MAX_MESSAGES: usize = 256;

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassExW(class: *const WndClassExW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: Hwnd,
        menu: Handle,
        instance: Handle,
        param: *mut c_void,
    ) -> Hwnd;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
    fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    fn SetWindowPos(hwnd: Hwnd, after: Hwnd, x: i32, y: i32, w: i32, h: i32, flags: u32) -> i32;
    fn GetWindowLongPtrW(hwnd: Hwnd, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: Hwnd, index: i32, value: isize) -> isize;
    fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
    fn AdjustWindowRectEx(rect: *mut Rect, style: u32, menu: i32, ex_style: u32) -> i32;
    fn PeekMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn LoadCursorW(instance: Handle, name: *const u16) -> Handle;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Handle;
    fn GetProcAddress(module: Handle, name: *const c_char) -> *mut c_void;
}

/// What a window of TINVO's knows of the plugin window in it, kept where
/// its procedure finds it (`GWLP_USERDATA`).
struct Data {
    token: u64,
    /// Which ways the user may resize it: (across, up and down).
    axes: Cell<(bool, bool)>,
}

thread_local! {
    /// What the windows of this thread recorded since the last pump, each
    /// with the window that recorded it.
    static EVENTS: RefCell<Vec<(usize, Event)>> = const { RefCell::new(Vec::new()) };
    /// The plugins whose window is open, for the sizes a drag makes.
    static OPEN: RefCell<Vec<PluginHandle>> = const { RefCell::new(Vec::new()) };
    /// A drag of a window's border or title is under way.
    static SIZING: Cell<bool> = const { Cell::new(false) };
    /// A drag's size is being served: a size that sets is not another.
    static SERVING: Cell<bool> = const { Cell::new(false) };
    /// TINVO is sizing or moving a window itself, maybe with the plugin's
    /// window state held: the caller knows what that makes it.
    static OWN: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` as TINVO's own sizing of a window (see `OWN`).
fn own<T>(f: impl FnOnce() -> T) -> T {
    let was = OWN.replace(true);
    let out = f();
    OWN.set(was);
    out
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().filter(|&c| c != 0).chain(Some(0)).collect()
}

/// A function of `user32.dll` that older versions of Windows lack.
fn user32(name: &CStr) -> *mut c_void {
    static USER32: OnceLock<usize> = OnceLock::new();
    // SAFETY: user32 is linked, so loaded; names are NUL-terminated.
    unsafe {
        let module = *USER32.get_or_init(|| GetModuleHandleW(wide("user32.dll").as_ptr()) as usize);
        GetProcAddress(module as Handle, name.as_ptr())
    }
}

/// The window class of TINVO's plugin windows, registered once.
fn class() -> Option<&'static [u16]> {
    static CLASS: OnceLock<Option<Vec<u16>>> = OnceLock::new();
    CLASS
        .get_or_init(|| {
            let name = wide("TinvoPluginWindow");
            // SAFETY: a class described in full, its procedure below.
            let atom = unsafe {
                let class = WndClassExW {
                    size: size_of::<WndClassExW>() as u32,
                    style: 0,
                    wnd_proc: Some(wnd_proc),
                    cls_extra: 0,
                    wnd_extra: 0,
                    instance: GetModuleHandleW(null()),
                    icon: null_mut(),
                    cursor: LoadCursorW(null_mut(), IDC_ARROW as *const u16),
                    background: (COLOR_WINDOW + 1) as Handle,
                    menu_name: null(),
                    class_name: name.as_ptr(),
                    icon_small: null_mut(),
                };
                RegisterClassExW(&class)
            };
            (atom != 0).then_some(name)
        })
        .as_deref()
}

fn record(hwnd: Hwnd, event: Event) {
    EVENTS.with(|e| {
        if let Ok(mut e) = e.try_borrow_mut() {
            e.push((hwnd as usize, event));
        }
    });
}

/// Serves what a window recorded through its plugin's handle at once,
/// inside Windows' own loop for a drag; false if it cannot be now (inside
/// TINVO's own sizing or another serving), for `pump` to serve.
fn serve(token: u64, f: impl FnOnce(&PluginHandle)) -> bool {
    if SERVING.get() || OWN.get() {
        return false;
    }
    OPEN.with(|open| {
        let Ok(open) = open.try_borrow() else { return false };
        let Some(handle) = open.iter().find(|h| h.token() == token) else { return false };
        SERVING.set(true);
        f(handle);
        SERVING.set(false);
        true
    })
}

/// Drops what `hwnd` recorded that `past` says is past.
fn unrecord(hwnd: Hwnd, past: impl Fn(&Event) -> bool) {
    EVENTS.with(|e| {
        if let Ok(mut e) = e.try_borrow_mut() {
            e.retain(|(w, event)| *w != hwnd as usize || !past(event));
        }
    });
}

unsafe extern "system" fn wnd_proc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
    // SAFETY: set right after the window is made to its `Data`, which
    // outlives it, and cleared before it is destroyed; null otherwise.
    let data = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Data).as_ref() };
    let Some(data) = data else {
        // SAFETY: the default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        // Left standing: its plugin's window goes first.
        WM_CLOSE => {
            record(hwnd, Event::Closed(data.token));
            return 0;
        }
        // A size TINVO gave it is no drag.
        WM_SIZE if wparam != SIZE_MINIMIZED && !OWN.get() => {
            let (w, h) = ((lparam & 0xFFFF) as u32, ((lparam >> 16) & 0xFFFF) as u32);
            if SIZING.get() && serve(data.token, |handle| handle.gui_resized(w, h)) {
                unrecord(hwnd, |e| matches!(e, Event::Resized(..)));
            } else {
                record(hwnd, Event::Resized(data.token, w, h));
            }
        }
        WM_ENTERSIZEMOVE => SIZING.set(true),
        WM_EXITSIZEMOVE => SIZING.set(false),
        WM_GETMINMAXINFO => {
            // SAFETY: Windows passes the structure to fill.
            if let Some(info) = unsafe { (lparam as *mut MinMaxInfo).as_mut() } {
                // Any size TINVO gives it, however small or large; the
                // user only the ways the plugin can be resized (asked
                // before a drag is under way, too).
                info.min_track = Point { x: 1, y: 1 };
                info.max_track = Point { x: MAX_SIDE, y: MAX_SIDE };
                let (across, up) = data.axes.get();
                let mut r = Rect::default();
                // SAFETY: a live window, and room for its rectangle.
                if !OWN.get() && unsafe { GetWindowRect(hwnd, &mut r) } != 0 {
                    if !across {
                        (info.min_track.x, info.max_track.x) = (r.right - r.left, r.right - r.left);
                    }
                    if !up {
                        (info.min_track.y, info.max_track.y) = (r.bottom - r.top, r.bottom - r.top);
                    }
                }
                return 0;
            }
        }
        // Put where Windows suggests, so a window dragged across keeps its
        // place under the pointer, then sized by the plugin for the scale.
        WM_DPICHANGED => {
            // SAFETY: Windows passes the rectangle it suggests.
            if let Some(r) = unsafe { (lparam as *const Rect).as_ref() } {
                let (w, h) = (r.right - r.left, r.bottom - r.top);
                // SAFETY: a live window of this thread.
                own(|| unsafe { SetWindowPos(hwnd, null_mut(), r.left, r.top, w, h, SWP_NOZORDER | SWP_NOACTIVATE) });
            }
            let scale = f64::from((wparam & 0xFFFF) as u16) / 96.0;
            if !(SIZING.get() && serve(data.token, |handle| handle.gui_rescaled(scale))) {
                record(hwnd, Event::Scaled(data.token, scale));
            }
            return 0;
        }
        _ => {}
    }
    // SAFETY: the default handling of everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Makes this thread per-monitor DPI aware, where Windows can (from
/// Windows 10 1703).
pub(super) fn init() {
    let f = user32(c"SetThreadDpiAwarenessContext");
    if !f.is_null() {
        // SAFETY: the system's own function, of this signature, given a
        // context Windows defines.
        unsafe {
            let set: unsafe extern "system" fn(isize) -> isize = std::mem::transmute(f);
            set(PER_MONITOR_V2);
        }
    }
}

/// Opens `handle`'s plugin window with the Win32 API.
pub(super) fn open(handle: &PluginHandle, api: &'static CStr, title: &str) -> Result<GuiMode, String> {
    let token = handle.token();
    let mode =
        handle.gui_open(api, title, || Parent::new(token, title).map(|p| Box::new(p) as Box<dyn ParentWindow>))?;
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        o.retain(|h| h.token() != token);
        o.push(handle.clone());
    });
    Ok(mode)
}

/// None: the thread's messages wake the main loop themselves.
pub(super) fn descriptor() -> Option<i32> {
    None
}

/// Lets go of the window of the instance with `token`.
pub(super) fn forget(token: u64) {
    OPEN.with(|o| o.borrow_mut().retain(|h| h.token() != token));
}

/// Hands out the messages waiting for this thread's windows, a bounded
/// number of them, and returns what TINVO's windows recorded for the
/// instances `owns` says are the caller's; the rest wait for theirs.
pub(super) fn pump(owns: impl Fn(u64) -> bool) -> Vec<Event> {
    let mut msg = Msg { hwnd: null_mut(), message: 0, wparam: 0, lparam: 0, time: 0, x: 0, y: 0, private: 0 };
    for _ in 0..MAX_MESSAGES {
        // SAFETY: room for one message, for any window of this thread.
        if unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } == 0 {
            break;
        }
        // A plugin's quit request is not TINVO's to end on.
        if msg.message != WM_QUIT {
            // SAFETY: a message just taken from the queue.
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    EVENTS.with(|e| {
        let mut e = e.borrow_mut();
        let (mine, theirs): (Vec<_>, Vec<_>) = e.drain(..).partition(|(_, event)| owns(event.token()));
        *e = theirs;
        mine.into_iter().map(|(_, event)| event).collect()
    })
}

/// A top-level window holding one plugin window.
struct Parent {
    hwnd: Hwnd,
    data: Box<Data>,
}

impl Parent {
    fn new(token: u64, title: &str) -> Result<Self, String> {
        let class = class().ok_or("could not register a window class")?;
        let title = wide(title);
        // SAFETY: a registered class and NUL-terminated strings; the window
        // is this thread's.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                FIXED,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null_mut(),
            )
        };
        if hwnd.is_null() {
            return Err(format!("could not make a window: {}", std::io::Error::last_os_error()));
        }
        let data = Box::new(Data { token, axes: Cell::new((false, false)) });
        // SAFETY: the window just made; `data` outlives it (see `Drop`).
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*data as *const Data as isize) };
        Ok(Self { hwnd, data })
    }

    fn style(&self) -> u32 {
        // SAFETY: a live window of this thread.
        unsafe { GetWindowLongPtrW(self.hwnd, GWL_STYLE) as u32 }
    }

    fn dpi(&self) -> u32 {
        let f = user32(c"GetDpiForWindow");
        if f.is_null() {
            return 96;
        }
        // SAFETY: the system's own function, of this signature.
        let get: unsafe extern "system" fn(Hwnd) -> u32 = unsafe { std::mem::transmute(f) };
        // SAFETY: a live window.
        match unsafe { get(self.hwnd) } {
            0 => 96,
            dpi => dpi,
        }
    }

    /// The window's outer size for a client area of `width` by `height`.
    fn outer(&self, width: u32, height: u32) -> Option<(i32, i32)> {
        let side = |v: u32| i32::try_from(v).ok().filter(|&v| v <= MAX_SIDE);
        let mut r = Rect { left: 0, top: 0, right: side(width)?, bottom: side(height)? };
        let style = self.style();
        let f = user32(c"AdjustWindowRectExForDpi");
        // SAFETY: the system's own functions, of these signatures.
        let ok = unsafe {
            if f.is_null() {
                AdjustWindowRectEx(&mut r, style, 0, 0)
            } else {
                let adjust: unsafe extern "system" fn(*mut Rect, u32, i32, u32, u32) -> i32 = std::mem::transmute(f);
                adjust(&mut r, style, 0, 0, self.dpi())
            }
        };
        (ok != 0).then(|| (r.right - r.left, r.bottom - r.top))
    }

    fn client(&self) -> (u32, u32) {
        let mut r = Rect::default();
        // SAFETY: a live window, and room for its rectangle.
        unsafe { GetClientRect(self.hwnd, &mut r) };
        (r.right.max(0) as u32, r.bottom.max(0) as u32)
    }
}

impl ParentWindow for Parent {
    fn handle(&self) -> *mut c_void {
        self.hwnd
    }

    fn scale(&self) -> Option<f64> {
        Some(f64::from(self.dpi()) / 96.0)
    }

    /// True only once its client area has the size: Windows may give it
    /// another and still say it succeeded (a captioned window has a least
    /// width), and then it is given back the one it had.
    fn resize(&mut self, width: u32, height: u32) -> bool {
        let Some((w, h)) = self.outer(width, height) else { return false };
        let mut was = Rect::default();
        let flags = SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE;
        own(|| {
            // SAFETY: a live window of this thread, and room for its
            // rectangle.
            let ok = unsafe {
                GetWindowRect(self.hwnd, &mut was) != 0 && SetWindowPos(self.hwnd, null_mut(), 0, 0, w, h, flags) != 0
            };
            if ok && self.client() == (width, height) {
                return true;
            }
            // SAFETY: as above.
            unsafe { SetWindowPos(self.hwnd, null_mut(), 0, 0, was.right - was.left, was.bottom - was.top, flags) };
            false
        })
    }

    fn set_resizable(&mut self, hints: Option<ClapGuiResizeHints>) {
        let axes = hints.map_or((false, false), |h| (h.can_resize_horizontally, h.can_resize_vertically));
        self.data.axes.set(axes);
        let style = self.style();
        let new = match axes {
            (false, false) => style & !(WS_THICKFRAME | WS_MAXIMIZEBOX),
            (true, true) => style | WS_THICKFRAME | WS_MAXIMIZEBOX,
            // Maximised, it would take a size it cannot.
            _ => (style | WS_THICKFRAME) & !WS_MAXIMIZEBOX,
        };
        if new == style {
            return;
        }
        let client = self.client();
        // SAFETY: a live window of this thread; its client size is kept
        // through the frame's change.
        own(|| unsafe {
            SetWindowLongPtrW(self.hwnd, GWL_STYLE, new as isize);
            SetWindowPos(
                self.hwnd,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        });
        if client.0 > 0 && client.1 > 0 {
            self.resize(client.0, client.1);
        }
    }

    fn show(&mut self) {
        // SAFETY: a live window of this thread.
        unsafe {
            ShowWindow(self.hwnd, SW_SHOWNORMAL);
            SetForegroundWindow(self.hwnd);
        }
    }

    fn hide(&mut self) {
        // SAFETY: as above.
        unsafe { ShowWindow(self.hwnd, SW_HIDE) };
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        // SAFETY: a live window of this thread, let go of by its procedure
        // first, then destroyed once.
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            DestroyWindow(self.hwnd);
        }
        // What it recorded and was not served is not for a window opened
        // after it for the same plugin.
        let hwnd = self.hwnd as usize;
        EVENTS.with(|e| {
            if let Ok(mut e) = e.try_borrow_mut() {
                e.retain(|(w, _)| *w != hwnd);
            }
        });
    }
}
