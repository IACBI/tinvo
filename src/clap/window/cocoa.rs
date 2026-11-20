//! Plugin windows on macOS: an `NSWindow` of TINVO's for each one
//! embedded, through the Objective-C runtime, with AppKit loaded when the
//! first is opened (TINVO's terminal use never loads it).
//!
//! AppKit makes windows only on the process's main thread, which TINVO's
//! main loop runs on. A process started from a terminal has no bundle, and
//! AppKit gives such a process no windows until it says what kind of
//! application it is: TINVO is one with windows but no Dock icon or menu
//! bar (`NSApplicationActivationPolicyAccessory`).
//!
//! A window's delegate records a close the user asks for (left standing
//! until its plugin's window is destroyed first) and the sizes the window
//! takes, for [`pump`]; while the user drags its border AppKit runs a loop
//! of its own inside one event, so those sizes go to the plugin at once,
//! never from inside a size TINVO set itself. Sizes are in points, which
//! the plugin is given as they are (CLAP's Cocoa windows are not scaled).
//! AppKit's events wake nothing the terminal waits on, so the main loop
//! waits no longer than [`WAKE_WITHIN`] while a window is open.

use super::super::host::{GuiMode, ParentWindow, PluginHandle};
use super::Event;
use crate::clap::ffi::ClapGuiResizeHints;
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::null_mut;
use std::sync::OnceLock;
use std::time::Duration;

type Id = *mut c_void;
type Sel = *const c_void;

/// How long the main loop waits at most while a window is open: about a
/// frame, so the window's events are handed out as smoothly as it draws.
pub(super) const WAKE_WITHIN: Duration = Duration::from_millis(16);

const APPKIT: &CStr = c"/System/Library/Frameworks/AppKit.framework/AppKit";
const RTLD_NOW: c_int = 2;
/// `NSWindowStyleMask` (`NSWindow.h`).
const TITLED: usize = 1 << 0;
const CLOSABLE: usize = 1 << 1;
const MINIATURIZABLE: usize = 1 << 2;
const RESIZABLE: usize = 1 << 3;
/// `NSBackingStoreBuffered` (`NSGraphics.h`).
const BUFFERED: usize = 2;
/// `NSApplicationActivationPolicyAccessory` (`NSRunningApplication.h`).
const ACCESSORY: isize = 1;
/// `NSEventMaskAny` (`NSEvent.h`).
const ANY_EVENT: usize = usize::MAX;
/// The largest side a window is given, in points.
const MAX_SIDE: f64 = 32767.0;
/// Events handed out in one pump, so a flood cannot keep the terminal
/// waiting.
const MAX_EVENTS: usize = 256;
/// A delegate's `BOOL`-returning method, as the runtime encodes it.
#[cfg(target_arch = "aarch64")]
const SHOULD_CLOSE_TYPES: &CStr = c"B@:@";
#[cfg(not(target_arch = "aarch64"))]
const SHOULD_CLOSE_TYPES: &CStr = c"c@:@";

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Size {
    width: f64,
    height: f64,
}

/// `NSRect`, its origin unread.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)]
struct Rect {
    x: f64,
    y: f64,
    size: Size,
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
    fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra: usize) -> Id;
    fn class_addMethod(class: Id, sel: Sel, imp: *const c_void, types: *const c_char) -> bool;
    fn objc_registerClassPair(class: Id);
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

unsafe extern "C" {
    fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
}

/// Sends `$sel` to `$obj` with arguments of the types given, as a call of
/// exactly that signature (the runtime's `objc_msgSend` must never be
/// called as a variadic function).
macro_rules! send {
    ($ret:ty; $obj:expr, $sel:literal $(, $arg:expr => $ty:ty)*) => {{
        let f: unsafe extern "C" fn(Id, Sel $(, $ty)*) -> $ret =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f($obj, sel_registerName($sel.as_ptr()) $(, $arg)*)
    }};
}

/// A class of the runtime's, by name.
unsafe fn class(name: &CStr) -> Id {
    // SAFETY: a NUL-terminated name.
    unsafe { objc_getClass(name.as_ptr()) }
}

/// `[view frame].size`: a structure returned in memory on x86-64.
unsafe fn frame_size(view: Id) -> Size {
    #[cfg(target_arch = "x86_64")]
    let send = objc_msgSend_stret as unsafe extern "C" fn();
    #[cfg(not(target_arch = "x86_64"))]
    let send = objc_msgSend as unsafe extern "C" fn();
    // SAFETY: the runtime's own function, called with the signature of
    // `-[NSView frame]`, which returns an `NSRect`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Rect = std::mem::transmute(send);
        f(view, sel_registerName(c"frame".as_ptr())).size
    }
}

/// An `NSString` of `s`, autoreleased.
unsafe fn string(s: &str) -> Id {
    let s = std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default();
    // SAFETY: a NUL-terminated UTF-8 string.
    unsafe { send!(Id; class(c"NSString"), c"stringWithUTF8String:", s.as_ptr() => *const c_char) }
}

/// Runs `f` in an autorelease pool of its own: TINVO runs no run loop of
/// AppKit's that would drain one.
fn pool<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: a pool pushed and popped on this thread, around `f` only.
    let p = unsafe { objc_autoreleasePoolPush() };
    let out = f();
    // SAFETY: as above.
    unsafe { objc_autoreleasePoolPop(p) };
    out
}

thread_local! {
    /// The application, readied with the first window.
    static APP: Cell<Option<usize>> = const { Cell::new(None) };
    /// TINVO's windows, each by the instance whose plugin window it holds.
    static WINDOWS: RefCell<Vec<(usize, u64)>> = const { RefCell::new(Vec::new()) };
    /// What the windows recorded since the last pump, each with its window.
    static EVENTS: RefCell<Vec<(usize, Event)>> = const { RefCell::new(Vec::new()) };
    /// The plugins whose window is open, for the sizes a drag makes.
    static OPEN: RefCell<Vec<PluginHandle>> = const { RefCell::new(Vec::new()) };
    /// A drag's size is being served: a size that sets is not another.
    static SERVING: Cell<bool> = const { Cell::new(false) };
    /// TINVO is sizing a window itself, maybe with the plugin's window
    /// state held: the caller knows what that makes it.
    static OWN: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` as TINVO's own sizing of a window (see `OWN`).
fn own<T>(f: impl FnOnce() -> T) -> T {
    let was = OWN.replace(true);
    let out = f();
    OWN.set(was);
    out
}

fn token_of(window: Id) -> Option<u64> {
    WINDOWS.with(|w| w.borrow().iter().find(|(win, _)| *win == window as usize).map(|(_, t)| *t))
}

fn record(window: Id, event: Event) {
    EVENTS.with(|e| {
        if let Ok(mut e) = e.try_borrow_mut() {
            e.push((window as usize, event));
        }
    });
}

/// Offers a size the user's drag made to the plugin at once, inside
/// AppKit's own loop for it; false if it cannot be now (inside TINVO's own
/// sizing or another serving), for `pump` to serve.
fn serve_drag(token: u64, width: u32, height: u32) -> bool {
    if SERVING.get() || OWN.get() {
        return false;
    }
    OPEN.with(|open| {
        let Ok(open) = open.try_borrow() else { return false };
        let Some(handle) = open.iter().find(|h| h.token() == token) else { return false };
        SERVING.set(true);
        handle.gui_resized(width, height);
        SERVING.set(false);
        true
    })
}

/// `-windowShouldClose:`: the plugin's window goes first, at the pump.
unsafe extern "C" fn window_should_close(_: Id, _: Sel, window: Id) -> bool {
    if let Some(token) = token_of(window) {
        record(window, Event::Closed(token));
    }
    false
}

/// `-windowDidResize:`.
unsafe extern "C" fn window_did_resize(_: Id, _: Sel, notification: Id) {
    if OWN.get() {
        return;
    }
    // SAFETY: AppKit's notification, whose object is the window.
    let (window, size, live) = unsafe {
        let window = send!(Id; notification, c"object");
        let view = send!(Id; window, c"contentView");
        (window, frame_size(view), send!(bool; window, c"inLiveResize"))
    };
    let Some(token) = token_of(window) else { return };
    let (w, h) = (size.width.round().max(1.0) as u32, size.height.round().max(1.0) as u32);
    if live && serve_drag(token, w, h) {
        EVENTS.with(|e| {
            if let Ok(mut e) = e.try_borrow_mut() {
                e.retain(|(win, ev)| *win != window as usize || !matches!(ev, Event::Resized(..)));
            }
        });
    } else {
        record(window, Event::Resized(token, w, h));
    }
}

/// The delegate every window of TINVO's has, made once.
fn delegate() -> Id {
    static DELEGATE: OnceLock<usize> = OnceLock::new();
    *DELEGATE.get_or_init(|| {
        // SAFETY: a class of TINVO's made once, under NSObject, with the
        // two methods above of the signatures their encodings give.
        unsafe {
            let cls = objc_allocateClassPair(class(c"NSObject"), c"TinvoPluginWindowDelegate".as_ptr(), 0);
            class_addMethod(
                cls,
                sel_registerName(c"windowShouldClose:".as_ptr()),
                window_should_close as *const c_void,
                SHOULD_CLOSE_TYPES.as_ptr(),
            );
            class_addMethod(
                cls,
                sel_registerName(c"windowDidResize:".as_ptr()),
                window_did_resize as *const c_void,
                c"v@:@".as_ptr(),
            );
            objc_registerClassPair(cls);
            let d = send!(Id; cls, c"alloc");
            send!(Id; d, c"init") as usize
        }
    }) as Id
}

/// Loads AppKit and readies the application, once, on the main thread.
fn app() -> Result<Id, String> {
    if let Some(app) = APP.get() {
        return Ok(app as Id);
    }
    // SAFETY: a NUL-terminated path; the framework stays loaded.
    if unsafe { dlopen(APPKIT.as_ptr(), RTLD_NOW) }.is_null() {
        return Err("AppKit could not be loaded".into());
    }
    // SAFETY: AppKit's own classes and methods, of these signatures.
    let app = pool(|| unsafe {
        if !send!(bool; class(c"NSThread"), c"isMainThread") {
            return Err("plugin windows open only on the main thread on macOS".to_string());
        }
        let app = send!(Id; class(c"NSApplication"), c"sharedApplication");
        send!(bool; app, c"setActivationPolicy:", ACCESSORY => isize);
        send!(();  app, c"finishLaunching");
        Ok(app)
    })?;
    APP.set(Some(app as usize));
    Ok(app)
}

/// Nothing to ready: AppKit is loaded with the first window.
pub(super) fn init() {}

/// None: AppKit's events come through no descriptor (see [`WAKE_WITHIN`]).
pub(super) fn descriptor() -> Option<i32> {
    None
}

/// AppKit's queue is read whole by each pump.
pub(super) fn queued() -> bool {
    false
}

/// Opens `handle`'s plugin window with the Cocoa API.
pub(super) fn open(handle: &PluginHandle, api: &'static CStr, title: &str) -> Result<GuiMode, String> {
    let token = handle.token();
    let mode = pool(|| {
        handle.gui_open(api, title, || Parent::new(token, title).map(|p| Box::new(p) as Box<dyn ParentWindow>))
    })?;
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        o.retain(|h| h.token() != token);
        o.push(handle.clone());
    });
    Ok(mode)
}

/// Lets go of the window of the instance with `token`.
pub(super) fn forget(token: u64) {
    OPEN.with(|o| o.borrow_mut().retain(|h| h.token() != token));
}

/// Hands out the events waiting for the application, a bounded number of
/// them, and returns what TINVO's windows recorded for the instances
/// `owns` says are the caller's; the rest wait for theirs.
pub(super) fn pump(owns: impl Fn(u64) -> bool) -> Vec<Event> {
    if let Some(app) = APP.get() {
        let app = app as Id;
        pool(|| {
            // SAFETY: AppKit's own classes and methods, of these
            // signatures, on the main thread (checked when readied).
            unsafe {
                let past = send!(Id; class(c"NSDate"), c"distantPast");
                // `NSDefaultRunLoopMode`'s value: modes are compared by it.
                let mode = string("kCFRunLoopDefaultMode");
                for _ in 0..MAX_EVENTS {
                    let event = send!(Id; app, c"nextEventMatchingMask:untilDate:inMode:dequeue:",
                        ANY_EVENT => usize, past => Id, mode => Id, true => bool);
                    if event.is_null() {
                        break;
                    }
                    send!((); app, c"sendEvent:", event => Id);
                }
                send!((); app, c"updateWindows");
            }
        });
    }
    EVENTS.with(|e| {
        let mut e = e.borrow_mut();
        let (mine, theirs): (Vec<_>, Vec<_>) = e.drain(..).partition(|(_, event)| owns(event.token()));
        *e = theirs;
        mine.into_iter().map(|(_, event)| event).collect()
    })
}

/// A window holding one plugin window.
struct Parent {
    window: Id,
    /// Which ways the user may resize it: (across, up and down).
    axes: (bool, bool),
}

impl Parent {
    fn new(token: u64, title: &str) -> Result<Self, String> {
        app()?;
        // SAFETY: AppKit's own classes and methods, of these signatures,
        // on the main thread; the window is TINVO's, kept until dropped.
        let window = unsafe {
            let rect = Rect { x: 0.0, y: 0.0, size: Size { width: 1.0, height: 1.0 } };
            let alloc = send!(Id; class(c"NSWindow"), c"alloc");
            let window = send!(Id; alloc, c"initWithContentRect:styleMask:backing:defer:",
                rect => Rect, TITLED | CLOSABLE | MINIATURIZABLE => usize, BUFFERED => usize, false => bool);
            if window.is_null() {
                return Err("could not make a window".into());
            }
            send!((); window, c"setReleasedWhenClosed:", false => bool);
            send!((); window, c"setTitle:", string(title) => Id);
            send!((); window, c"setDelegate:", delegate() => Id);
            window
        };
        WINDOWS.with(|w| w.borrow_mut().push((window as usize, token)));
        Ok(Self { window, axes: (false, false) })
    }

    fn content_size(&self) -> Size {
        // SAFETY: a window of TINVO's and its content view.
        unsafe { frame_size(send!(Id; self.window, c"contentView")) }
    }

    /// Holds the axes the plugin cannot resize at `size`.
    fn limit(&self, size: Size) {
        let min = Size {
            width: if self.axes.0 { 1.0 } else { size.width },
            height: if self.axes.1 { 1.0 } else { size.height },
        };
        let max = Size {
            width: if self.axes.0 { MAX_SIDE } else { size.width },
            height: if self.axes.1 { MAX_SIDE } else { size.height },
        };
        // SAFETY: a window of TINVO's; sizes by value.
        unsafe {
            send!((); self.window, c"setContentMinSize:", min => Size);
            send!((); self.window, c"setContentMaxSize:", max => Size);
        }
    }
}

impl ParentWindow for Parent {
    fn handle(&self) -> *mut c_void {
        // SAFETY: a window of TINVO's; its content view lives as long.
        unsafe { send!(Id; self.window, c"contentView") }
    }

    /// None: Cocoa windows are sized in points, which the plugin is given
    /// as they are.
    fn scale(&self) -> Option<f64> {
        None
    }

    /// True only once its content has the size: AppKit may keep it within
    /// the screen, and then it is given back the one it had.
    fn resize(&mut self, width: u32, height: u32) -> bool {
        let size = Size { width: f64::from(width), height: f64::from(height) };
        if !(1.0..=MAX_SIDE).contains(&size.width) || !(1.0..=MAX_SIDE).contains(&size.height) {
            return false;
        }
        let was = self.content_size();
        pool(|| {
            own(|| {
                self.limit(size);
                // SAFETY: a window of TINVO's; a size by value.
                unsafe { send!((); self.window, c"setContentSize:", size => Size) };
                if self.content_size() == size {
                    return true;
                }
                self.limit(was);
                // SAFETY: as above.
                unsafe { send!((); self.window, c"setContentSize:", was => Size) };
                false
            })
        })
    }

    fn set_resizable(&mut self, hints: Option<ClapGuiResizeHints>) {
        self.axes = hints.map_or((false, false), |h| (h.can_resize_horizontally, h.can_resize_vertically));
        let resizable = self.axes.0 || self.axes.1;
        // SAFETY: a window of TINVO's; its style as a mask.
        pool(|| unsafe {
            let style = send!(usize; self.window, c"styleMask");
            let new = if resizable { style | RESIZABLE } else { style & !RESIZABLE };
            if new != style {
                own(|| send!((); self.window, c"setStyleMask:", new => usize));
            }
        });
        self.limit(self.content_size());
    }

    fn show(&mut self) {
        pool(|| {
            // SAFETY: a window of TINVO's and the application, on the main
            // thread.
            unsafe {
                send!((); self.window, c"center");
                send!((); self.window, c"makeKeyAndOrderFront:", null_mut::<c_void>() => Id);
                if let Some(app) = APP.get() {
                    send!((); app as Id, c"activateIgnoringOtherApps:", true => bool);
                }
            }
        });
    }

    fn hide(&mut self) {
        // SAFETY: a window of TINVO's.
        pool(|| unsafe { send!((); self.window, c"orderOut:", null_mut::<c_void>() => Id) });
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        let window = self.window as usize;
        WINDOWS.with(|w| w.borrow_mut().retain(|(win, _)| *win != window));
        // What it recorded and was not served is not for a window opened
        // after it for the same plugin.
        EVENTS.with(|e| {
            if let Ok(mut e) = e.try_borrow_mut() {
                e.retain(|(win, _)| *win != window);
            }
        });
        // SAFETY: a window of TINVO's, let go of by its delegate first,
        // closed and released once.
        pool(|| unsafe {
            send!((); self.window, c"setDelegate:", null_mut::<c_void>() => Id);
            send!((); self.window, c"orderOut:", null_mut::<c_void>() => Id);
            send!((); self.window, c"close");
            send!((); self.window, c"release");
        });
    }
}
