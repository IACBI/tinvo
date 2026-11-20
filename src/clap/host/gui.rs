//! A plugin's own window (`clap.gui`), and the timers and file
//! descriptors (`clap.timer-support`, `clap.posix-fd-support`) a plugin's
//! window, or anything else it does on the main thread, runs on.
//!
//! The window is driven through a [`PluginHandle`], since the instance
//! itself plays inside the engine; CLAP lets `clap.gui`'s main-thread calls
//! run while the audio thread processes. A plugin that can float its own
//! window does; any other is embedded in a window of TINVO's, a
//! [`ParentWindow`], which this module owns from the moment the plugin's
//! window is made until after it is destroyed. However it goes (closed
//! through the handle, by the plugin, or with its instance), it goes the
//! one way, [`close`]: hidden unless the plugin already destroyed it,
//! destroyed, and then its parent.
//!
//! Nothing here holds a lock over a plugin's code: what a callback asks
//! for while a call into the plugin runs is kept, and served after it.

use super::*;
use std::time::{Duration, Instant};

/// The shortest period a timer runs at: CLAP lets a host lengthen shorter
/// ones, and asks that 30 Hz be allowed.
const MIN_TIMER: Duration = Duration::from_millis(15);

/// At most this many timers and descriptors for one instance.
const MAX_REGISTERED: usize = 64;

/// A window of TINVO's that a plugin's window is embedded in: made hidden,
/// sized to the plugin's window, then shown. Dropping it destroys it, which
/// happens only after the plugin's window is destroyed. Its methods are
/// called on the main thread, never over a call into the plugin, and with
/// the window's state held: one must not call back into the plugin's
/// handle, and reports what it learns (a drag, a close) later, from the
/// main loop.
pub trait ParentWindow {
    /// The handle CLAP's `clap_window_t` carries: its `HWND`, X11 `Window`
    /// or `NSView *`.
    fn handle(&self) -> *mut c_void;
    /// The scale its system draws it at (2.0 for 200 %), for a window API
    /// that sizes in physical pixels; `None` for one in logical pixels
    /// (Cocoa), whose plugin is given no scale.
    fn scale(&self) -> Option<f64>;
    /// Gives its client area this size; false if it cannot.
    fn resize(&mut self, width: u32, height: u32) -> bool;
    /// Lets the user resize it as `hints` say, or not at all with `None`.
    fn set_resizable(&mut self, hints: Option<ClapGuiResizeHints>);
    fn show(&mut self);
    fn hide(&mut self);
}

/// How a plugin's window is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuiMode {
    /// A window of its own, which the plugin floats.
    Floating,
    /// Inside a [`ParentWindow`].
    Embedded,
}

/// A plugin's open window.
pub(super) struct Open {
    mode: GuiMode,
    parent: Option<Box<dyn ParentWindow>>,
    /// The size last agreed on, which a refused resize goes back to.
    size: (u32, u32),
    /// The size TINVO last gave the parent itself: the parent's report of
    /// it is no drag of the user's to pass on.
    echo: Option<(u32, u32)>,
    /// Counts the sizes the plugin asked for and got: a drag that one
    /// overtook while the plugin took it leaves the size alone.
    sized: u32,
    shown: bool,
}

/// What a plugin asked of its window since it was last served.
#[derive(Default)]
pub(super) struct Asked {
    /// Shown (true) or hidden (false): the last it asked for.
    show: Option<bool>,
    resize: Option<(u32, u32)>,
    hints: bool,
    /// It closed its window (whether it destroyed it too is
    /// `GuiData::gone`).
    closed: bool,
}

/// One of a plugin's timers.
pub(super) struct Timer {
    id: ClapId,
    period: Duration,
    due: Instant,
}

/// The window, timer and descriptor state of one instance, in its
/// [`HostData`].
#[derive(Default)]
pub(super) struct GuiData {
    /// Its window, while open; locked on the main thread only, and never
    /// over a call into the plugin (a callback that finds it locked keeps
    /// its request for later).
    pub(super) open: Mutex<Option<Open>>,
    /// Open, and how: read by callbacks on any thread.
    is_open: AtomicBool,
    embedded: AtomicBool,
    /// The plugin destroyed its window itself (`closed(true)`): nothing but
    /// `destroy` is called from then on, set the moment it says so.
    gone: AtomicBool,
    pub(super) asked: Mutex<Asked>,
    pub(super) timers: Mutex<Vec<Timer>>,
    next_timer: AtomicU32,
    /// Its file descriptors, with the CLAP flags it waits for and a serial
    /// of the registration, which a descriptor closed and its number
    /// registered again does not share.
    pub(super) fds: Mutex<Vec<(i32, u32, u64)>>,
    /// The registrations as of [`PluginHost::fds`]: only their readiness
    /// is passed on.
    waited: Mutex<Vec<u64>>,
}

/// The serial of the next descriptor registered, by any instance.
static NEXT_FD: AtomicU64 = AtomicU64::new(0);

impl GuiData {
    /// Lets go of its timers and descriptors, as of an instance destroyed.
    pub(super) fn forget_registrations(&self) {
        lock(&self.timers).clear();
        lock(&self.fds).clear();
        lock(&self.waited).clear();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The plugin's `clap.gui` functions that every use needs.
#[derive(Clone, Copy)]
pub(super) struct GuiFns {
    is_api_supported: unsafe extern "C" fn(*const ClapPlugin, *const c_char, bool) -> bool,
    create: unsafe extern "C" fn(*const ClapPlugin, *const c_char, bool) -> bool,
    destroy: unsafe extern "C" fn(*const ClapPlugin),
    show: unsafe extern "C" fn(*const ClapPlugin) -> bool,
    hide: unsafe extern "C" fn(*const ClapPlugin) -> bool,
    get_size: Option<unsafe extern "C" fn(*const ClapPlugin, *mut u32, *mut u32) -> bool>,
    set_parent: Option<unsafe extern "C" fn(*const ClapPlugin, *const ClapWindow) -> bool>,
    set_scale: Option<unsafe extern "C" fn(*const ClapPlugin, f64) -> bool>,
    can_resize: Option<unsafe extern "C" fn(*const ClapPlugin) -> bool>,
    get_resize_hints: Option<unsafe extern "C" fn(*const ClapPlugin, *mut ClapGuiResizeHints) -> bool>,
    adjust_size: Option<unsafe extern "C" fn(*const ClapPlugin, *mut u32, *mut u32) -> bool>,
    set_size: Option<unsafe extern "C" fn(*const ClapPlugin, u32, u32) -> bool>,
    suggest_title: Option<unsafe extern "C" fn(*const ClapPlugin, *const c_char)>,
}

impl GuiFns {
    /// # Safety
    /// `gui` is null or the plugin's own `clap.gui`.
    pub(super) unsafe fn of(gui: *const ClapPluginGui) -> Option<GuiFns> {
        // SAFETY: by the caller's promise.
        let g = unsafe { gui.as_ref() }?;
        Some(GuiFns {
            is_api_supported: g.is_api_supported?,
            create: g.create?,
            destroy: g.destroy?,
            show: g.show?,
            hide: g.hide?,
            get_size: g.get_size,
            set_parent: g.set_parent,
            set_scale: g.set_scale,
            can_resize: g.can_resize,
            get_resize_hints: g.get_resize_hints,
            adjust_size: g.adjust_size,
            set_size: g.set_size,
            suggest_title: g.suggest_title,
        })
    }
}

/// The ways a plugin can show its window with one window API.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuiSupport {
    pub floating: bool,
    pub embedded: bool,
}

pub(super) static HOST_GUI: ClapHostGui = ClapHostGui {
    resize_hints_changed: Some(host_gui_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};
pub(super) static HOST_TIMER: ClapHostTimerSupport =
    ClapHostTimerSupport { register_timer: Some(host_register_timer), unregister_timer: Some(host_unregister_timer) };
pub(super) static HOST_FD: ClapHostPosixFdSupport = ClapHostPosixFdSupport {
    register_fd: Some(host_register_fd),
    modify_fd: Some(host_modify_fd),
    unregister_fd: Some(host_unregister_fd),
};

/// The host data of a callback made on the instance's main thread.
///
/// # Safety
/// As [`host_data`].
unsafe fn on_main<'a>(host: *const ClapHost) -> Option<&'a HostData> {
    // SAFETY: by the caller's promise.
    unsafe { host_data(host) }.filter(|h| h.owner == thread_key())
}

unsafe extern "C" fn host_gui_hints_changed(host: *const ClapHost) {
    // SAFETY: the pointer this instance was given.
    if let Some(h) = unsafe { host_data(host) } {
        lock(&h.gui.asked).hints = true;
    }
}

unsafe extern "C" fn host_gui_request_resize(host: *const ClapHost, width: u32, height: u32) -> bool {
    // SAFETY: as above.
    let Some(h) = (unsafe { host_data(host) }) else { return false };
    if !h.gui.embedded.load(Ordering::Acquire) || width == 0 || height == 0 {
        return false;
    }
    // On the main thread the answer is the parent's; if the window state
    // is busy (this comes from inside a call into the plugin), or from
    // another thread, true only says it was taken, and a resize the parent
    // then refuses sends the plugin back to its last size.
    if h.owner == thread_key() {
        if let Ok(mut open) = h.gui.open.try_lock() {
            if let Some(o) = open.as_mut().filter(|o| o.mode == GuiMode::Embedded) {
                let ok = o.parent.as_mut().is_some_and(|p| p.resize(width, height));
                if ok {
                    (o.size, o.echo) = ((width, height), Some((width, height)));
                    o.sized = o.sized.wrapping_add(1);
                    lock(&h.gui.asked).resize = None;
                }
                return ok;
            }
        }
    }
    lock(&h.gui.asked).resize = Some((width, height));
    true
}

unsafe extern "C" fn host_gui_request_show(host: *const ClapHost) -> bool {
    // SAFETY: as above.
    let Some(h) = (unsafe { host_data(host) }) else { return false };
    let open = h.gui.is_open.load(Ordering::Acquire);
    if open {
        lock(&h.gui.asked).show = Some(true);
    }
    open
}

unsafe extern "C" fn host_gui_request_hide(host: *const ClapHost) -> bool {
    // SAFETY: as above.
    let Some(h) = (unsafe { host_data(host) }) else { return false };
    let open = h.gui.is_open.load(Ordering::Acquire);
    if open {
        lock(&h.gui.asked).show = Some(false);
    }
    open
}

unsafe extern "C" fn host_gui_closed(host: *const ClapHost, was_destroyed: bool) {
    // SAFETY: as above.
    if let Some(h) = unsafe { host_data(host) } {
        if was_destroyed {
            h.gui.gone.store(true, Ordering::Release);
        }
        lock(&h.gui.asked).closed = true;
    }
}

unsafe extern "C" fn host_register_timer(host: *const ClapHost, period_ms: u32, timer_id: *mut ClapId) -> bool {
    // SAFETY: the pointer this instance was given.
    let Some(h) = (unsafe { on_main(host) }) else { return false };
    if timer_id.is_null() {
        return false;
    }
    let mut timers = lock(&h.gui.timers);
    if timers.len() >= MAX_REGISTERED {
        return false;
    }
    let id = loop {
        let id = h.gui.next_timer.fetch_add(1, Ordering::Relaxed);
        if id != CLAP_INVALID_ID && !timers.iter().any(|t| t.id == id) {
            break id;
        }
    };
    let period = Duration::from_millis(u64::from(period_ms)).max(MIN_TIMER);
    timers.push(Timer { id, period, due: Instant::now() + period });
    // SAFETY: a plugin passes room for the id.
    unsafe { timer_id.write(id) };
    true
}

unsafe extern "C" fn host_unregister_timer(host: *const ClapHost, timer_id: ClapId) -> bool {
    // SAFETY: as above.
    let Some(h) = (unsafe { on_main(host) }) else { return false };
    let mut timers = lock(&h.gui.timers);
    let before = timers.len();
    timers.retain(|t| t.id != timer_id);
    timers.len() < before
}

const FD_FLAGS: u32 = CLAP_POSIX_FD_READ | CLAP_POSIX_FD_WRITE | CLAP_POSIX_FD_ERROR;

unsafe extern "C" fn host_register_fd(host: *const ClapHost, fd: c_int, flags: u32) -> bool {
    // SAFETY: the pointer this instance was given.
    let Some(h) = (unsafe { on_main(host) }) else { return false };
    let mut fds = lock(&h.gui.fds);
    if fd < 0 || flags & !FD_FLAGS != 0 || fds.len() >= MAX_REGISTERED || fds.iter().any(|f| f.0 == fd) {
        return false;
    }
    fds.push((fd, flags, NEXT_FD.fetch_add(1, Ordering::Relaxed)));
    true
}

unsafe extern "C" fn host_modify_fd(host: *const ClapHost, fd: c_int, flags: u32) -> bool {
    // SAFETY: as above.
    let Some(h) = (unsafe { on_main(host) }) else { return false };
    if flags & !FD_FLAGS != 0 {
        return false;
    }
    let mut fds = lock(&h.gui.fds);
    match fds.iter_mut().find(|f| f.0 == fd) {
        Some(f) => {
            f.1 = flags;
            true
        }
        None => false,
    }
}

unsafe extern "C" fn host_unregister_fd(host: *const ClapHost, fd: c_int) -> bool {
    // SAFETY: as above.
    let Some(h) = (unsafe { on_main(host) }) else { return false };
    let mut fds = lock(&h.gui.fds);
    let before = fds.len();
    fds.retain(|f| f.0 != fd);
    fds.len() < before
}

/// Closes `h`'s window, if open: hidden (unless the plugin destroyed it
/// already), destroyed, then its parent. On the main thread, with the
/// plugin alive.
///
/// # Safety
/// `plugin` is `h`'s live instance and `gui` its own functions, on its main
/// thread.
pub(super) unsafe fn close(h: &HostData, plugin: *const ClapPlugin, gui: GuiFns) {
    let destroyed = h.gui.gone.swap(false, Ordering::AcqRel);
    let Some(mut open) = lock(&h.gui.open).take() else { return };
    h.gui.is_open.store(false, Ordering::Release);
    h.gui.embedded.store(false, Ordering::Release);
    // SAFETY: by the caller's promise.
    unsafe {
        if open.shown && !destroyed {
            (gui.hide)(plugin);
        }
        (gui.destroy)(plugin);
    }
    if let (true, Some(p)) = (open.shown, open.parent.as_mut()) {
        p.hide();
    }
    drop(open);
    // What it asked of the window it had is nothing to the next.
    *lock(&h.gui.asked) = Asked::default();
}

impl HostData {
    /// Closes its window if it has one open, as of its instance going: on
    /// the main thread, before the instance is deactivated and destroyed.
    pub(super) fn close_gui(&self) {
        let plugin = self.plugin.load(Ordering::Acquire);
        let gui = self.exts.get().and_then(|e| e.gui);
        if let (false, Some(gui)) = (plugin.is_null(), gui) {
            // SAFETY: called on the main thread with the instance alive.
            unsafe { close(self, plugin, gui) };
        }
    }

    /// Serves what the plugin asked of its window: closing it (reported),
    /// showing or hiding it, a resize its parent then makes or refuses,
    /// and new resize hints. On the main thread, from [`PluginHost::poll`].
    pub(super) fn serve_gui(&self, plugin: *const ClapPlugin, requests: &mut Vec<Request>) {
        let Some(gui) = self.exts.get().and_then(|e| e.gui) else { return };
        // A resize stays asked for until it is served, so one the plugin
        // gets on the main thread meanwhile (inside `show`, say) cancels it.
        let asked = {
            let mut a = lock(&self.gui.asked);
            let resize = a.resize.take();
            let asked = std::mem::take(&mut *a);
            a.resize = resize;
            asked
        };
        if !self.gui.is_open.load(Ordering::Acquire) {
            return;
        }
        // Closed, before or during any call below: closed with nothing
        // more asked of it.
        let closed = |requests: &mut Vec<Request>| {
            // SAFETY: main thread, a live plugin and its own functions.
            unsafe { close(self, plugin, gui) };
            requests.push(Request::GuiClosed(self.token));
        };
        if asked.closed {
            return closed(requests);
        }
        if let Some(show) = asked.show {
            self.set_shown(plugin, gui, show);
        }
        if self.gui.gone.load(Ordering::Acquire) {
            return closed(requests);
        }
        let resize = lock(&self.gui.asked).resize.take();
        if let Some((w, h)) = resize {
            let refused = {
                let mut open = lock(&self.gui.open);
                match open.as_mut().filter(|o| o.mode == GuiMode::Embedded) {
                    Some(o) => {
                        if o.parent.as_mut().is_some_and(|p| p.resize(w, h)) {
                            (o.size, o.echo) = ((w, h), Some((w, h)));
                            o.sized = o.sized.wrapping_add(1);
                            None
                        } else {
                            Some(o.size)
                        }
                    }
                    None => None,
                }
            };
            if let (Some((w, h)), Some(set_size)) = (refused, gui.set_size) {
                // SAFETY: main thread, a live plugin, embedded.
                unsafe { set_size(plugin, w, h) };
            }
            if self.gui.gone.load(Ordering::Acquire) {
                return closed(requests);
            }
        }
        if asked.hints {
            // SAFETY: as above.
            let hints = unsafe { resize_hints(self, plugin, gui) };
            if let Some(p) = lock(&self.gui.open).as_mut().and_then(|o| o.parent.as_mut()) {
                p.set_resizable(hints);
            }
        }
    }

    fn set_shown(&self, plugin: *const ClapPlugin, gui: GuiFns, show: bool) {
        if self.gui.gone.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: main thread, a live plugin and its own functions.
        let ok = unsafe { if show { (gui.show)(plugin) } else { (gui.hide)(plugin) } };
        let mut open = lock(&self.gui.open);
        let Some(o) = open.as_mut() else { return };
        if ok || !show {
            o.shown = show && ok;
            if let Some(p) = o.parent.as_mut() {
                if o.shown { p.show() } else { p.hide() }
            }
        }
    }
}

/// How an embedded plugin window may be resized: `None` if not at all, or
/// if the plugin destroys it meanwhile (which the caller checks).
///
/// # Safety
/// Main thread, `plugin` live, `gui` its own.
unsafe fn resize_hints(h: &HostData, plugin: *const ClapPlugin, gui: GuiFns) -> Option<ClapGuiResizeHints> {
    // SAFETY: by the caller's promise.
    unsafe {
        if !gui.can_resize.is_some_and(|f| f(plugin)) || h.gui.gone.load(Ordering::Acquire) {
            return None;
        }
        let both =
            ClapGuiResizeHints { can_resize_horizontally: true, can_resize_vertically: true, ..Default::default() };
        let mut hints = ClapGuiResizeHints::default();
        match gui.get_resize_hints {
            Some(f) if f(plugin, &mut hints) => Some(hints),
            _ => Some(both),
        }
    }
}

impl PluginHandle {
    fn gui(&self) -> Option<(*const ClapPlugin, GuiFns)> {
        let (plugin, exts) = self.plugin()?;
        Some((plugin, exts.gui?))
    }

    /// How the plugin can show its window with window API `api`.
    pub fn gui_support(&self, api: &CStr) -> GuiSupport {
        let Some((plugin, gui)) = self.gui() else { return GuiSupport::default() };
        // SAFETY: main thread, a live plugin, its own function and a
        // NUL-terminated API name.
        unsafe {
            GuiSupport {
                floating: (gui.is_api_supported)(plugin, api.as_ptr(), true),
                embedded: (gui.is_api_supported)(plugin, api.as_ptr(), false),
            }
        }
    }

    /// How its window is shown, while open.
    pub fn gui_mode(&self) -> Option<GuiMode> {
        lock(&self.host.gui.open).as_ref().map(|o| o.mode)
    }

    /// Opens the plugin's window with window API `api`: one it floats
    /// itself, titled `title`, if it can; or else embedded in the window
    /// `parent` makes (hidden; it is sized, the plugin put in it, and shown
    /// here). Open already, it stays as it is.
    pub fn gui_open(
        &self,
        api: &'static CStr,
        title: &str,
        parent: impl FnOnce() -> Result<Box<dyn ParentWindow>, String>,
    ) -> Result<GuiMode, String> {
        let Some((plugin, gui)) = self.gui() else {
            return Err(match self.plugin() {
                Some(_) => "has no window of its own".into(),
                None => "is not playing".into(),
            });
        };
        if let Some(mode) = self.gui_mode() {
            return Ok(mode);
        }
        let support = self.gui_support(api);
        let h = &*self.host;
        let set_open = |mode, parent| {
            *lock(&h.gui.open) = Some(Open { mode, parent, size: (0, 0), echo: None, sized: 0, shown: false });
            h.gui.embedded.store(mode == GuiMode::Embedded, Ordering::Release);
            h.gui.is_open.store(true, Ordering::Release);
        };
        // SAFETY: main thread, a live plugin, its own functions; every
        // failure after `create` destroys the window again (`close`).
        unsafe {
            let fail = |why: &str| {
                close(h, plugin, gui);
                Err(format!("could not {why}"))
            };
            // It may destroy its window itself during any call, which
            // `close` then only acknowledges.
            let gone = || h.gui.gone.load(Ordering::Acquire);
            let closed = || {
                close(h, plugin, gui);
                Err("closed its window".to_string())
            };
            h.gui.gone.store(false, Ordering::Release);
            if support.floating {
                if !(gui.create)(plugin, api.as_ptr(), true) {
                    return Err("could not make its window".into());
                }
                set_open(GuiMode::Floating, None);
                if gone() {
                    return closed();
                }
                if let (Some(f), Ok(title)) = (gui.suggest_title, CString::new(title)) {
                    f(plugin, title.as_ptr());
                }
                if gone() {
                    return closed();
                }
                if !(gui.show)(plugin) {
                    return fail("show its window");
                }
                if gone() {
                    return closed();
                }
                if let Some(o) = lock(&h.gui.open).as_mut() {
                    o.shown = true;
                }
                return Ok(GuiMode::Floating);
            }
            if !support.embedded {
                return Err("has no window TINVO can open on this system".into());
            }
            let (Some(get_size), Some(set_parent)) = (gui.get_size, gui.set_parent) else {
                return Err("has an incomplete window extension".into());
            };
            let window = parent()?;
            // The maker may have used this handle: dropped the instance, or
            // opened the window itself.
            if self.gui().is_none() {
                return Err("is not playing".into());
            }
            if let Some(mode) = self.gui_mode() {
                return Ok(mode);
            }
            let (handle, scale) = (window.handle(), window.scale());
            if !(gui.create)(plugin, api.as_ptr(), false) {
                return Err("could not make its window".into());
            }
            set_open(GuiMode::Embedded, Some(window));
            if gone() {
                return closed();
            }
            if let (Some(scale), Some(f)) = (scale, gui.set_scale) {
                // One the plugin ignores is allowed.
                f(plugin, scale);
            }
            if gone() {
                return closed();
            }
            let hints = resize_hints(h, plugin, gui);
            let (mut w, mut hgt) = (0, 0);
            if gone() {
                return closed();
            }
            if !get_size(plugin, &mut w, &mut hgt) || w == 0 || hgt == 0 {
                return fail("learn its window's size");
            }
            if gone() {
                return closed();
            }
            let sized = {
                let mut open = lock(&h.gui.open);
                let o = open.as_mut().expect("opened above, closed only on this thread");
                let p = o.parent.as_mut().expect("embedded");
                p.set_resizable(hints);
                (o.size, o.echo) = ((w, hgt), Some((w, hgt)));
                p.resize(w, hgt)
            };
            if !sized {
                return fail("make a window its size");
            }
            let window = ClapWindow { api: api.as_ptr(), handle };
            if !set_parent(plugin, &window) {
                return fail("put its window in TINVO's");
            }
            if gone() {
                return closed();
            }
            if !(gui.show)(plugin) {
                return fail("show its window");
            }
            if gone() {
                return closed();
            }
            if let Some(o) = lock(&h.gui.open).as_mut() {
                o.shown = true;
                if let Some(p) = o.parent.as_mut() {
                    p.show();
                }
            }
            Ok(GuiMode::Embedded)
        }
    }

    /// Closes the plugin's window, if open.
    pub fn gui_close(&self) {
        if let Some((plugin, gui)) = self.gui() {
            // SAFETY: main thread, a live plugin and its own functions.
            unsafe { close(&self.host, plugin, gui) };
        }
    }

    /// The user resized the window an embedded plugin window is in to this
    /// client size: the plugin's window follows, as near as it can, and
    /// the window is made the plugin's size again where it differs (or
    /// cannot be resized at all).
    pub fn gui_resized(&self, width: u32, height: u32) {
        let Some((plugin, gui)) = self.gui() else { return };
        let gone = || self.host.gui.gone.load(Ordering::Acquire);
        if gone() {
            return;
        }
        let (size, sized) = {
            let mut open = lock(&self.host.gui.open);
            let Some(o) = open.as_mut().filter(|o| o.mode == GuiMode::Embedded) else { return };
            if o.echo.take() == Some((width, height)) {
                return;
            }
            (o.size, o.sized)
        };
        let (mut w, mut h) = (width, height);
        // SAFETY: main thread, a live plugin, embedded.
        let fitted = unsafe {
            // Destroyed by the plugin between any two calls: no more calls.
            match (resize_hints(&self.host, plugin, gui), gui.adjust_size, gui.set_size) {
                (Some(_), Some(adjust), Some(set)) => {
                    adjust(plugin, &mut w, &mut h) && w > 0 && h > 0 && !gone() && set(plugin, w, h)
                }
                (Some(_), None, Some(set)) => !gone() && set(plugin, w, h),
                _ => false,
            }
        };
        let (w, h) = if fitted { (w, h) } else { size };
        let mut open = lock(&self.host.gui.open);
        // Closed meanwhile, or sized as the plugin asked while it took
        // this: either wins.
        let Some(o) = open.as_mut().filter(|o| o.sized == sized && !gone()) else { return };
        o.size = (w, h);
        if (w, h) != (width, height) && o.parent.as_mut().is_some_and(|p| p.resize(w, h)) {
            o.echo = Some((w, h));
        }
    }
}

impl PluginHost {
    /// When the next timer of a plugin of this host is due.
    pub fn next_timer(&self) -> Option<Instant> {
        self.live
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|h| !h.plugin.load(Ordering::Acquire).is_null())
            .filter_map(|h| lock(&h.gui.timers).iter().map(|t| t.due).min())
            .min()
    }

    /// The file descriptors this host's plugins wait on, each once with
    /// the CLAP flags any of them waits for.
    pub fn fds(&self) -> Vec<(i32, u32)> {
        let mut all: Vec<(i32, u32)> = Vec::new();
        for h in self.live.iter().filter_map(Weak::upgrade) {
            if h.plugin.load(Ordering::Acquire).is_null() {
                continue;
            }
            let fds = lock(&h.gui.fds);
            *lock(&h.gui.waited) = fds.iter().map(|f| f.2).collect();
            for &(fd, flags, _) in fds.iter() {
                match all.iter_mut().find(|a| a.0 == fd) {
                    Some(a) => a.1 |= flags,
                    None => all.push((fd, flags)),
                }
            }
        }
        all
    }

    /// Calls each plugin's timers due by `now`, then `on_fd` for each of
    /// `ready` (a descriptor and the CLAP flags it is ready with) that a
    /// plugin waits on, with the flags it waits for (and an error). A
    /// timer late by more than its period runs once and is due a period
    /// from now. Each is checked still registered right before its call:
    /// one call may take away the next. A descriptor's readiness reaches
    /// only a registration [`PluginHost::fds`] gave, not one the plugin
    /// made since for a descriptor that took its number.
    pub fn dispatch(&mut self, now: Instant, ready: &[(i32, u32)]) {
        // Instances come home first: they are no longer to be called.
        let home = lock(&self.inbox).as_mut().map(std::mem::take);
        drop(home);
        let live: Vec<Arc<HostData>> = self.live.iter().filter_map(Weak::upgrade).collect();
        for h in &live {
            let due: Vec<ClapId> = {
                let mut timers = lock(&h.gui.timers);
                timers
                    .iter_mut()
                    .filter(|t| t.due <= now)
                    .map(|t| {
                        t.due = if now.duration_since(t.due) >= t.period { now + t.period } else { t.due + t.period };
                        t.id
                    })
                    .collect()
            };
            for id in due {
                let plugin = h.plugin.load(Ordering::Acquire);
                let on_timer = h.exts.get().and_then(|e| e.on_timer);
                let (false, Some(f)) = (plugin.is_null(), on_timer) else { break };
                if lock(&h.gui.timers).iter().any(|t| t.id == id) {
                    // SAFETY: main thread (this host's), a live plugin and
                    // its own function, no lock held.
                    unsafe { f(plugin, id) };
                }
            }
        }
        for &(fd, flags) in ready {
            for h in &live {
                let plugin = h.plugin.load(Ordering::Acquire);
                let on_fd = h.exts.get().and_then(|e| e.on_fd);
                let (false, Some(f)) = (plugin.is_null(), on_fd) else { continue };
                let wanted = {
                    let waited = lock(&h.gui.waited);
                    lock(&h.gui.fds).iter().find(|f| f.0 == fd && waited.contains(&f.2)).map(|f| f.1)
                };
                let Some(wanted) = wanted else { continue };
                let flags = flags & (wanted | CLAP_POSIX_FD_ERROR);
                if flags != 0 {
                    // SAFETY: as above.
                    unsafe { f(plugin, fd, flags) };
                }
            }
        }
    }
}
