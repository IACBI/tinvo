//! A CLAP plugin file loaded and initialised: the operating system's
//! library handle, its `clap_entry`, and the `init` / `deinit` pair the
//! spec asks for (every `init` that succeeded gets one `deinit`).
//!
//! A library, once loaded, stays loaded until the process ends. A plugin
//! can leave code behind that runs after its `deinit`: thread-exit
//! destructors (Rust's standard library and C++ `thread_local` register
//! them on threads that called into it) and threads of its own. Unloading
//! the library would pull that code out from under them, and the next
//! thread to exit would crash. CLAP lets a file be initialised again after
//! its `deinit`, so loading it again later costs only that.

use super::ffi::*;
use super::{PluginInfo, Roles};
use std::ffi::{CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// At most this many descriptors are read from one factory.
pub(super) const MAX_DESCRIPTORS: u32 = 256;
/// At most this many features are read from one descriptor.
pub(super) const MAX_FEATURES: usize = 64;
/// At most this many bytes are read from one of a descriptor's strings.
const MAX_STRING: usize = 1024;

/// An initialised plugin file. Dropping it calls `deinit`; the library
/// stays loaded.
pub struct Dso {
    entry: *const ClapPluginEntry,
    deinit: unsafe extern "C" fn(),
    library: sys::Library,
}

// SAFETY: what a `Dso` holds is the entry and its factory, whose functions
// CLAP marks thread-safe; `init` ran in `open`, and `deinit` runs in `drop`,
// which the plugin host keeps on its main thread (`host.rs`).
unsafe impl Send for Dso {}
// SAFETY: as above; `&Dso` reaches only the thread-safe functions.
unsafe impl Sync for Dso {}

impl Dso {
    /// Loads the plugin file at `path` (a `.clap` file, or on macOS a
    /// `.clap` bundle) and initialises it.
    ///
    /// # Safety
    /// Loading a library runs its code in this process, and nothing TINVO
    /// checks (pointers, versions, layouts) can make that safe: the file is
    /// trusted by being loaded. Its `clap_entry` must be a
    /// `clap_plugin_entry_t`. No other thread may be loading or using the
    /// same file meanwhile, since CLAP allows one `init` at a time, and the
    /// file must not be initialised already in this process: the scanner
    /// calls this once in a child of its own, and the plugin host goes
    /// through [`Dso::open_among`].
    pub unsafe fn open(path: &Path) -> Result<Dso, String> {
        // SAFETY: passed on from the caller.
        match unsafe { Self::open_library(path, &[]) }? {
            Opened::New(d) => Ok(d),
            Opened::Loaded(_) => unreachable!("nothing to share"),
        }
    }

    /// Like [`Dso::open`], but if the library the operating system loads
    /// for `path` is one of `loaded` already (another path to the same
    /// image, as the system tells), that one is returned and not
    /// initialised again.
    ///
    /// # Safety
    /// As [`Dso::open`], with `loaded` holding every plugin file this
    /// process has initialised and not yet deinitialised.
    pub unsafe fn open_among(path: &Path, loaded: &[Arc<Dso>]) -> Result<Arc<Dso>, String> {
        // SAFETY: passed on from the caller.
        Ok(match unsafe { Self::open_library(path, loaded) }? {
            Opened::New(d) => Arc::new(d),
            Opened::Loaded(d) => d,
        })
    }

    /// # Safety
    /// As [`Dso::open_among`].
    unsafe fn open_library(path: &Path, loaded: &[Arc<Dso>]) -> Result<Opened, String> {
        let path = std::path::absolute(path).map_err(|e| e.to_string())?;
        let binary = sys::binary_of(&path)?;
        let library = sys::Library::open(&binary)?;
        // The system hands out one handle per loaded image, so a second
        // load of the same one only counts it again.
        if let Some(d) = loaded.iter().find(|d| d.library.handle() == library.handle()) {
            return Ok(Opened::Loaded(d.clone()));
        }
        let entry = library.entry()?;
        // SAFETY: `entry` points at the plugin's exported `clap_entry`, which
        // lives as long as the library; its version is read first, as the
        // rest of its layout depends on it.
        let version = unsafe { (*entry).clap_version };
        if !clap_version_is_compatible(version) {
            return Err(format!("is written for CLAP {}.{}.{}", version.major, version.minor, version.revision));
        }
        // SAFETY: as above, for a CLAP 1 entry.
        let (init, deinit, get_factory) = unsafe { ((*entry).init, (*entry).deinit, (*entry).get_factory) };
        let (Some(init), Some(deinit), Some(_)) = (init, deinit, get_factory) else {
            return Err("has an incomplete clap_entry".into());
        };
        // CLAP passes the plugin's own path as UTF-8: the bundle on macOS,
        // the file elsewhere.
        let text = path.to_str().ok_or("has a path that is not valid Unicode")?;
        let c_path = CString::new(text).map_err(|_| "has a path with a NUL in it")?;
        // SAFETY: the entry's init, called first and once, with a valid path.
        if !unsafe { init(c_path.as_ptr()) } {
            return Err("refused to start (its init failed)".into());
        }
        Ok(Opened::New(Dso { entry, deinit, library }))
    }

    /// Its plugin factory, valid as long as the `Dso`.
    pub fn factory(&self) -> Result<*const ClapPluginFactory, String> {
        // SAFETY: checked non-null in `open`; get_factory is thread-safe.
        let factory = unsafe {
            let get_factory = (*self.entry).get_factory.ok_or("has no get_factory")?;
            get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()).cast::<ClapPluginFactory>()
        };
        if factory.is_null() { Err("has no plugin factory".into()) } else { Ok(factory) }
    }

    /// The plugins its factory offers.
    pub fn plugins(&self) -> Result<Vec<PluginInfo>, String> {
        let factory = self.factory()?;
        // SAFETY: a factory the plugin returned, valid while it is initialised.
        let (count, get) = unsafe { ((*factory).get_plugin_count, (*factory).get_plugin_descriptor) };
        let (Some(count), Some(get)) = (count, get) else {
            return Err("has an incomplete plugin factory".into());
        };
        // SAFETY: the factory's own function, given the factory.
        let n = unsafe { count(factory) }.min(MAX_DESCRIPTORS);
        let mut plugins = Vec::new();
        for i in 0..n {
            // SAFETY: an index below the count the factory gave.
            let desc = unsafe { get(factory, i) };
            // SAFETY: a descriptor the factory returned, read before deinit.
            if let Some(info) = unsafe { describe(desc) } {
                plugins.push(info);
            }
        }
        Ok(plugins)
    }
}

impl Drop for Dso {
    fn drop(&mut self) {
        // SAFETY: init succeeded in `open`; this is its one deinit, and the
        // library stays loaded after it.
        unsafe { (self.deinit)() };
    }
}

/// What [`Dso::open_library`] found.
enum Opened {
    New(Dso),
    Loaded(Arc<Dso>),
}

/// The executable of the macOS bundle at `path`, as CoreFoundation finds it
/// and [`Dso::open`] loads it; `None` elsewhere, or for a bundle without one.
pub fn bundle_executable(path: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "macos") && path.is_dir() { sys::binary_of(path).ok() } else { None }
}

/// A descriptor's contents, copied; `None` for a null descriptor, one of
/// another CLAP major version, or one without an id or a name.
///
/// # Safety
/// `desc` must be null or point at a descriptor whose strings are null or
/// NUL-terminated.
unsafe fn describe(desc: *const ClapPluginDescriptor) -> Option<PluginInfo> {
    if desc.is_null() {
        return None;
    }
    // SAFETY: non-null, and valid by the caller's promise.
    let d = unsafe { &*desc };
    if !clap_version_is_compatible(d.clap_version) {
        return None;
    }
    // SAFETY: the descriptor's strings, by the caller's promise.
    let text = |p: *const c_char| unsafe { bounded(p) }.map(|b| String::from_utf8_lossy(&b).into_owned());
    let text = |p: *const c_char| text(p).unwrap_or_default();
    // The id is what a host asks the factory for, so it is taken whole and
    // exact or not at all.
    // SAFETY: as above.
    let id = unsafe { bounded(d.id) }.filter(|b| b.len() < MAX_STRING).and_then(|b| String::from_utf8(b).ok())?;
    let name = text(d.name);
    if !super::is_valid_id(&id) || name.trim().is_empty() {
        return None;
    }
    let mut features = Vec::new();
    if !d.features.is_null() {
        for i in 0..MAX_FEATURES {
            // SAFETY: a null-terminated array, read up to its null.
            let f = unsafe { *d.features.add(i) };
            if f.is_null() {
                break;
            }
            features.push(text(f));
        }
    }
    let roles = Roles::from_features(&features);
    Some(PluginInfo {
        id,
        name,
        vendor: text(d.vendor),
        version: text(d.version),
        description: text(d.description),
        features,
        roles,
    })
}

/// The bytes of the string at `p`, read up to its NUL or [`MAX_STRING`]
/// bytes (so exactly `MAX_STRING` of them means it was cut short); `None`
/// for a null pointer.
///
/// # Safety
/// `p` must be null or point at a NUL-terminated string, or at least
/// [`MAX_STRING`] readable bytes.
unsafe fn bounded(p: *const c_char) -> Option<Vec<u8>> {
    if p.is_null() {
        return None;
    }
    let mut bytes = Vec::new();
    for i in 0..MAX_STRING {
        // SAFETY: within the string, by the caller's promise.
        let b = unsafe { *p.add(i) } as u8;
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    Some(bytes)
}

#[cfg(unix)]
mod sys {
    use super::{ClapPluginEntry, Path, PathBuf, c_char, c_void};
    use std::ffi::{CStr, CString, c_int};
    use std::os::unix::ffi::OsStrExt;

    const RTLD_NOW: c_int = 2;
    #[cfg(target_os = "macos")]
    const RTLD_LOCAL: c_int = 0x4;
    #[cfg(not(target_os = "macos"))]
    const RTLD_LOCAL: c_int = 0;
    // Symbols are looked up in this image only, not its dependencies.
    #[cfg(target_os = "macos")]
    const RTLD_FIRST: c_int = 0x100;
    #[cfg(not(target_os = "macos"))]
    const RTLD_FIRST: c_int = 0;

    unsafe extern "C" {
        fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
        fn dlerror() -> *mut c_char;
    }

    pub struct Library {
        handle: *mut c_void,
        path: PathBuf,
    }

    /// `dlerror`'s message, if any.
    fn last_error() -> String {
        // SAFETY: dlerror returns a NUL-terminated message or null.
        let why = unsafe { dlerror() };
        if why.is_null() {
            String::new()
        } else {
            // SAFETY: just checked non-null.
            unsafe { CStr::from_ptr(why) }.to_string_lossy().into_owned()
        }
    }

    impl Library {
        pub fn open(path: &Path) -> Result<Library, String> {
            let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| "has a path with a NUL in it")?;
            // SAFETY: a NUL-terminated path.
            let handle = unsafe { dlopen(c.as_ptr(), RTLD_NOW | RTLD_LOCAL | RTLD_FIRST) };
            if handle.is_null() {
                return Err(format!("cannot be loaded ({})", last_error()));
            }
            Ok(Library { handle, path: path.to_path_buf() })
        }

        /// The system's handle, the same for every load of one image.
        pub fn handle(&self) -> usize {
            self.handle as usize
        }

        /// The library's own `clap_entry`.
        pub fn entry(&self) -> Result<*const ClapPluginEntry, String> {
            // SAFETY: a live handle and a NUL-terminated name.
            let p = unsafe { dlsym(self.handle, c"clap_entry".as_ptr()) };
            if p.is_null() {
                return Err("is not a CLAP plugin (it has no clap_entry)".into());
            }
            if !in_image(p, &self.path) {
                return Err("is not a CLAP plugin (its clap_entry comes from another library)".into());
            }
            Ok(p.cast())
        }
    }

    /// Whether the symbol at `p` is in the library loaded from `path`.
    /// macOS looks only there already (`RTLD_FIRST`).
    #[cfg(target_os = "macos")]
    fn in_image(_p: *mut c_void, _path: &Path) -> bool {
        true
    }

    #[cfg(not(target_os = "macos"))]
    fn in_image(p: *mut c_void, path: &Path) -> bool {
        /// `Dl_info`.
        #[repr(C)]
        struct DlInfo {
            fname: *const c_char,
            fbase: *mut c_void,
            sname: *const c_char,
            saddr: *mut c_void,
        }
        unsafe extern "C" {
            fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
        }
        let mut info = DlInfo {
            fname: std::ptr::null(),
            fbase: std::ptr::null_mut(),
            sname: std::ptr::null(),
            saddr: std::ptr::null_mut(),
        };
        // SAFETY: an address from dlsym and a properly laid out out-pointer.
        if unsafe { dladdr(p, &mut info) } == 0 || info.fname.is_null() {
            return false;
        }
        // SAFETY: dladdr gives a NUL-terminated file name.
        let fname = unsafe { CStr::from_ptr(info.fname) };
        let canonical = |p: &Path| std::fs::canonicalize(p).ok();
        let from = Path::new(std::ffi::OsStr::from_bytes(fname.to_bytes()));
        canonical(from).is_some_and(|f| Some(f) == canonical(path))
    }

    /// The file to load for the plugin at `path`: on macOS the executable
    /// of a `.clap` bundle, elsewhere the file itself.
    #[cfg(not(target_os = "macos"))]
    pub fn binary_of(path: &Path) -> Result<PathBuf, String> {
        Ok(path.to_path_buf())
    }

    #[cfg(target_os = "macos")]
    pub fn binary_of(path: &Path) -> Result<PathBuf, String> {
        type CFTypeRef = *const c_void;
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            fn CFURLCreateFromFileSystemRepresentation(
                alloc: CFTypeRef,
                buffer: *const u8,
                len: isize,
                is_directory: u8,
            ) -> CFTypeRef;
            fn CFBundleCreate(alloc: CFTypeRef, url: CFTypeRef) -> CFTypeRef;
            fn CFBundleCopyExecutableURL(bundle: CFTypeRef) -> CFTypeRef;
            fn CFURLGetFileSystemRepresentation(url: CFTypeRef, resolve: u8, buffer: *mut u8, max: isize) -> u8;
            fn CFRelease(cf: CFTypeRef);
        }
        if !path.is_dir() {
            return Ok(path.to_path_buf());
        }
        let bytes = path.as_os_str().as_bytes();
        let fail = || "is a bundle with no executable".to_string();
        // SAFETY: each object created is released once, after its last use.
        unsafe {
            let url =
                CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 1);
            if url.is_null() {
                return Err(fail());
            }
            let bundle = CFBundleCreate(std::ptr::null(), url);
            CFRelease(url);
            if bundle.is_null() {
                return Err(fail());
            }
            let exe = CFBundleCopyExecutableURL(bundle);
            CFRelease(bundle);
            if exe.is_null() {
                return Err(fail());
            }
            let mut buf = vec![0u8; 4096];
            let ok = CFURLGetFileSystemRepresentation(exe, 1, buf.as_mut_ptr(), buf.len() as isize);
            CFRelease(exe);
            if ok == 0 {
                return Err(fail());
            }
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..end])))
        }
    }
}

#[cfg(windows)]
mod sys {
    use super::{ClapPluginEntry, Path, PathBuf, c_char, c_void};
    use std::os::windows::ffi::OsStrExt;

    // The plugin's own folder for the libraries it needs, then the system's;
    // not the current folder or PATH.
    const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x0000_0100;
    const LOAD_LIBRARY_SEARCH_DEFAULT_DIRS: u32 = 0x0000_1000;

    unsafe extern "system" {
        fn LoadLibraryExW(file: *const u16, reserved: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    pub struct Library {
        handle: *mut c_void,
    }

    impl Library {
        pub fn open(path: &Path) -> Result<Library, String> {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            if wide[..wide.len() - 1].contains(&0) {
                return Err("has a path with a NUL in it".into());
            }
            // SAFETY: a NUL-terminated absolute path.
            let handle = unsafe {
                LoadLibraryExW(
                    wide.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
                )
            };
            if handle.is_null() {
                return Err(format!("cannot be loaded ({})", std::io::Error::last_os_error()));
            }
            Ok(Library { handle })
        }

        /// The module's handle, the same for every load of one image.
        pub fn handle(&self) -> usize {
            self.handle as usize
        }

        /// The library's own `clap_entry` (GetProcAddress looks in this
        /// module only, and finds exported data as well as functions).
        pub fn entry(&self) -> Result<*const ClapPluginEntry, String> {
            // SAFETY: a live module and a NUL-terminated name.
            let p = unsafe { GetProcAddress(self.handle, c"clap_entry".as_ptr()) };
            if p.is_null() {
                return Err("is not a CLAP plugin (it has no clap_entry)".into());
            }
            Ok(p.cast())
        }
    }

    pub fn binary_of(path: &Path) -> Result<PathBuf, String> {
        Ok(path.to_path_buf())
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use super::{ClapPluginEntry, Path, PathBuf};

    pub struct Library;

    impl Library {
        pub fn open(_path: &Path) -> Result<Library, String> {
            Err("cannot be loaded on this system".into())
        }

        pub fn entry(&self) -> Result<*const ClapPluginEntry, String> {
            Err("cannot be loaded on this system".into())
        }

        pub fn handle(&self) -> usize {
            0
        }
    }

    pub fn binary_of(path: &Path) -> Result<PathBuf, String> {
        Ok(path.to_path_buf())
    }
}
