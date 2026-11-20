//! A plugin's window in a real window of TINVO's on macOS. AppKit makes
//! windows only on the process's main thread, and the test harness runs
//! each test on a thread of its own, so this test runs without it, on the
//! main thread. Elsewhere it does nothing.

#[cfg(target_os = "macos")]
#[allow(dead_code)]
mod support;

fn main() {
    #[cfg(target_os = "macos")]
    {
        println!("\nrunning 1 test");
        macos::a_plugin_window_is_put_in_a_window_of_tinvo_s_on_macos();
        println!("test a_plugin_window_is_put_in_a_window_of_tinvo_s_on_macos ... ok\n");
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::support::*;
    use std::ffi::{CStr, c_char, c_void};
    use std::time::Duration;
    use tinvo::clap::ffi::CLAP_WINDOW_API_COCOA;
    use tinvo::clap::host::{GuiMode, PluginHost, Request, Role};
    use tinvo::clap::window::Windows;

    type Id = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        origin: Size,
        size: Size,
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn sel_registerName(name: *const c_char) -> *const c_void;
        fn objc_msgSend();
        #[cfg(target_arch = "x86_64")]
        fn objc_msgSend_stret();
    }

    /// `[obj sel]` returning `R`, with one argument of type `A`.
    unsafe fn send<A, R>(obj: Id, sel: &CStr, arg: A) -> R {
        // SAFETY: the runtime's function, called with the method's
        // signature as the caller gives it.
        unsafe {
            let f: unsafe extern "C" fn(Id, *const c_void, A) -> R =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(obj, sel_registerName(sel.as_ptr()), arg)
        }
    }

    /// `[obj sel]` returning `R`, with no argument.
    unsafe fn get<R>(obj: Id, sel: &CStr) -> R {
        // SAFETY: as in `send`.
        unsafe {
            let f: unsafe extern "C" fn(Id, *const c_void) -> R =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(obj, sel_registerName(sel.as_ptr()))
        }
    }

    /// The window a view is in.
    fn window_of(view: u64) -> Id {
        // SAFETY: a live view.
        unsafe { get(view as Id, c"window") }
    }

    /// The size of the window's content, in points.
    fn content(window: Id) -> (u32, u32) {
        #[cfg(target_arch = "x86_64")]
        let f = objc_msgSend_stret as unsafe extern "C" fn();
        #[cfg(not(target_arch = "x86_64"))]
        let f = objc_msgSend as unsafe extern "C" fn();
        // SAFETY: `-[NSView frame]`, which returns an `NSRect`, of the
        // window's content view.
        let size = unsafe {
            let view: Id = get(window, c"contentView");
            let frame: unsafe extern "C" fn(Id, *const c_void) -> Rect = std::mem::transmute(f);
            frame(view, sel_registerName(c"frame".as_ptr())).size
        };
        (size.width as u32, size.height as u32)
    }

    fn visible(window: Id) -> bool {
        // SAFETY: a live window.
        unsafe { get(window, c"isVisible") }
    }

    /// Sizes it as a user would, to content of `w` by `h` points.
    fn drag(window: Id, w: u32, h: u32) {
        let size = Size { width: f64::from(w), height: f64::from(h) };
        // SAFETY: a live window; a size by value.
        unsafe { send::<Size, ()>(window, c"setContentSize:", size) }
    }

    /// Closes it as the user would, with its close button.
    fn close(window: Id) {
        // SAFETY: a live window; no sender.
        unsafe { send::<Id, ()>(window, c"performClose:", std::ptr::null_mut()) }
    }

    pub fn a_plugin_window_is_put_in_a_window_of_tinvo_s_on_macos() {
        let dir = temp_dir("gui-cocoa");
        let file = install(&dir, "gui");
        let mut host = PluginHost::new();
        let gain = create(&mut host, &file, GAIN, Role::Effect).unwrap();
        let probe = GuiProbe::of(&gain);
        assert_eq!(Windows::api(), Some(CLAP_WINDOW_API_COCOA));
        let mut windows = Windows::new();
        assert_eq!(windows.wake_within(), None, "nothing open to wait for");
        // Put in a window of its size, shown, sized in points and given no
        // scale.
        assert_eq!(windows.open(gain.handle(), "Test Gain"), Ok(GuiMode::Embedded));
        let window = window_of(probe.stats().parent);
        assert!(!window.is_null() && visible(window));
        assert_eq!(content(window), (300, 200));
        assert!(!probe.log().iter().any(|l| l.starts_with("scale")));
        assert_eq!(windows.wake_within(), Some(Duration::from_millis(16)), "its events wake nothing");
        // Its own sizing reaches it as no drag.
        assert_eq!(windows.pump(), Vec::<u64>::new());
        assert!(!probe.log().iter().any(|l| l.starts_with("size")));
        // Sized by the user: the plugin takes the nearest size it can, and
        // the window follows; that is no drag either.
        drag(window, 455, 333);
        windows.pump();
        assert_eq!(probe.log().last().map(String::as_str), Some("size 450x330"));
        assert_eq!(content(window), (450, 330));
        windows.pump();
        assert_eq!(probe.log().iter().filter(|l| l.starts_with("size")).count(), 1);
        // Asked for by the plugin.
        assert!(probe.poke(0, 200, 120));
        assert_eq!(content(window), (200, 120));
        windows.pump();
        assert_eq!(probe.log().iter().filter(|l| l.starts_with("size")).count(), 1);
        // Closed by the user: standing until served, then the plugin's
        // window goes, then TINVO's.
        close(window);
        assert!(visible(window), "standing until it is served");
        assert_eq!(windows.pump(), [gain.token()]);
        assert_eq!(probe.log()[probe.log().len() - 2..], ["hide", "destroy"]);
        assert!(!windows.is_open(gain.token()));
        assert_eq!(windows.wake_within(), None);
        // A close the user asked for that TINVO's closing and opening it
        // again overtook is no close of the new window.
        windows.open(gain.handle(), "Test Gain").unwrap();
        close(window_of(probe.stats().parent));
        windows.close(gain.token());
        windows.open(gain.handle(), "Test Gain").unwrap();
        assert_eq!(windows.pump(), Vec::<u64>::new());
        assert!(windows.is_open(gain.token()));
        // Closed with the set, or by its plugin: let go of either way.
        drop(windows);
        assert_eq!(probe.log().last().map(String::as_str), Some("destroy"));
        let mut windows = Windows::new();
        windows.open(gain.handle(), "Test Gain").unwrap();
        assert!(probe.poke(3, 1, 0));
        assert_eq!(host.poll(), [Request::GuiClosed(gain.token())]);
        assert_eq!(windows.pump(), Vec::<u64>::new());
        assert!(!windows.is_open(gain.token()));
        assert_eq!(probe.stats().broken, 0, "every window call by CLAP's rules");
        drop(windows);
        drop(gain);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
