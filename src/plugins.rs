//! The editor's CLAP plugins: the scan of what is installed, run in the
//! background; the host the editor's instances come from; and what could
//! not play, kept for the plugin list until the next try.

use crate::clap::host::{Instance, Mode, PluginHost, Request, Role};
use crate::clap::scan::{self, Catalog};
use crate::clap::{PluginInfo, PluginRef, Roles};
use crate::engine::BLOCK;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::JoinHandle;

/// The editor's plugins. Not `Send`: it lives on the thread its host was
/// made on, which plays CLAP's main thread.
pub struct Plugins {
    host: PluginHost,
    catalog: Option<Catalog>,
    scanning: Option<Receiver<Result<Catalog, String>>>,
    /// The scan's thread, joined when this is dropped, and what calls it
    /// off: a plugin file hanging its scanner must not outlive the editor.
    scan_thread: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    /// Why the last scan could not be read.
    scan_error: Option<String>,
    /// The plugin of each instance made, by token.
    made: HashMap<u64, PluginRef>,
    /// Instances restarted once after failing: one that fails again is
    /// left silent.
    restarted: HashSet<u64>,
    /// Instances that failed twice, whose requests are no longer served.
    given_up: HashSet<u64>,
    /// Instances asked of the engine to restart and not yet back: what they
    /// ask meanwhile the restart under way answers.
    lending: HashSet<u64>,
    /// What could not play, as lines for the plugin list.
    problems: Vec<String>,
}

impl Default for Plugins {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugins {
    /// No catalog, so no instances, until a scan or [`Plugins::with_catalog`].
    pub fn new() -> Self {
        Self {
            host: PluginHost::new(),
            catalog: None,
            scanning: None,
            scan_thread: None,
            cancel: Arc::new(AtomicBool::new(false)),
            scan_error: None,
            made: HashMap::new(),
            restarted: HashSet::new(),
            given_up: HashSet::new(),
            lending: HashSet::new(),
            problems: Vec::new(),
        }
    }

    /// Scans the installed plugins on a thread of its own (with the cache,
    /// as `tinvo plugins` does); [`Plugins::poll_scan`] takes the catalog.
    ///
    /// # Safety
    /// As [`PluginHost::create`], for every plugin the scan finds: their
    /// code will run in this process, trusted, and this is on the thread
    /// CLAP calls main.
    pub unsafe fn start_scan(&mut self) {
        let (tx, rx) = mpsc::channel();
        let cancel = self.cancel.clone();
        let spawned = std::thread::Builder::new().name("plugin scan".into()).spawn(move || {
            let _ = tx.send(scan::installed(false, &cancel).map(|(c, _)| c));
        });
        match spawned {
            Ok(thread) => {
                self.scanning = Some(rx);
                self.scan_thread = Some(thread);
            }
            Err(e) => self.scan_error = Some(format!("cannot start the plugin scan: {e}")),
        }
    }

    /// Uses `catalog` as the scan's.
    ///
    /// # Safety
    /// As [`Plugins::start_scan`], for the plugins in `catalog`.
    pub unsafe fn with_catalog(mut self, catalog: Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// The scan's result once it has come: the number of plugins found, or
    /// why there are none. `None` while it runs, and after.
    pub fn poll_scan(&mut self) -> Option<Result<usize, String>> {
        let got = match self.scanning.as_ref()?.try_recv() {
            Err(TryRecvError::Empty) => return None,
            Ok(got) => got,
            Err(TryRecvError::Disconnected) => Err("the plugin scan stopped".into()),
        };
        self.scanning = None;
        Some(match got {
            Ok(catalog) => {
                let n = catalog.plugins().len();
                self.catalog = Some(catalog);
                Ok(n)
            }
            Err(e) => {
                self.scan_error = Some(e.clone());
                Err(e)
            }
        })
    }

    pub fn scanning(&self) -> bool {
        self.scanning.is_some()
    }

    pub fn catalog(&self) -> Option<&Catalog> {
        self.catalog.as_ref()
    }

    pub fn scan_error(&self) -> Option<&str> {
        self.scan_error.as_deref()
    }

    /// What the catalog says of `plugin`.
    pub fn info(&self, plugin: PluginRef) -> Option<&PluginInfo> {
        self.catalog.as_ref()?.find(plugin.id()).map(|(_, p)| p)
    }

    /// The plugin's name, or its id if the catalog does not know it.
    pub fn name(&self, plugin: PluginRef) -> String {
        self.info(plugin).map_or_else(|| plugin.id().to_string(), |p| p.name.clone())
    }

    /// The plugins the catalog offers as a `role`, by name.
    pub fn choices(&self, role: Role) -> Vec<&PluginInfo> {
        let want = match role {
            Role::Instrument => Roles::INSTRUMENT,
            Role::Effect => Roles::EFFECT,
        };
        let mut out: Vec<&PluginInfo> = match &self.catalog {
            Some(c) => c.plugins().into_iter().map(|(_, p)| p).filter(|p| p.roles.contains(want)).collect(),
            None => Vec::new(),
        };
        out.sort_by_cached_key(|p| (p.name.to_lowercase(), p.id.clone()));
        out
    }

    /// Why there is no catalog to make instances from.
    pub fn why_no_catalog(&self) -> String {
        match (&self.scan_error, self.scanning()) {
            (_, true) => "the plugin scan is still running".into(),
            (Some(e), false) => format!("the plugin scan failed ({e})"),
            (None, false) => "no plugin catalog was read".into(),
        }
    }

    /// An instance of `plugin` as a `role`, to play in real time at
    /// `sample_rate`, loading `state` first if given.
    pub fn make(
        &mut self,
        plugin: PluginRef,
        role: Role,
        sample_rate: u32,
        state: Option<&[u8]>,
    ) -> Result<Instance, String> {
        let Some(catalog) = &self.catalog else { return Err(self.why_no_catalog()) };
        let (file, _) = catalog.find(plugin.id()).ok_or("not found in the plugin catalog")?;
        // SAFETY: a file of the catalog, which whoever gave it promised
        // could be trusted; on the thread this was made on, which they
        // promised plays CLAP's main thread.
        let sr = f64::from(sample_rate);
        let instance =
            unsafe { self.host.create_with_state(file, plugin.id(), role, Mode::Realtime, sr, BLOCK, state) }?;
        self.made.insert(instance.token(), plugin);
        Ok(instance)
    }

    /// What the instances asked for since the last call, after destroying
    /// those that came home ([`PluginHost::poll`]).
    pub fn poll(&mut self) -> Vec<Request> {
        self.host.poll()
    }

    /// The plugin of the instance with `token`, if it was made here for
    /// the engine now playing and is still alive.
    pub fn plugin_of(&self, token: u64) -> Option<PluginRef> {
        self.made.get(&token).copied()
    }

    /// Forgets the instance with `token`, destroyed.
    pub fn forget(&mut self, token: u64) {
        self.made.remove(&token);
        self.restarted.remove(&token);
        self.given_up.remove(&token);
        self.lending.remove(&token);
    }

    /// Whether the instance with `token` should be asked of the engine to
    /// restart: not while it is already.
    pub fn lend(&mut self, token: u64) -> bool {
        self.lending.insert(token)
    }

    /// The instance with `token` is back from the engine, to restart.
    pub fn lent_back(&mut self, token: u64) {
        self.lending.remove(&token);
    }

    /// Whether the instance with `token` failed twice and is left be.
    pub fn given_up(&self, token: u64) -> bool {
        self.given_up.contains(&token)
    }

    /// Whether any instance has been made for the engine now playing.
    pub fn any_made(&self) -> bool {
        !self.made.is_empty()
    }

    /// Forgets the instances made: a new engine starts with none.
    pub fn forget_made(&mut self) {
        self.made.clear();
        self.restarted.clear();
        self.given_up.clear();
        self.lending.clear();
    }

    /// Whether the instance with `token` should be restarted after failing:
    /// once, so one that keeps failing is not restarted forever. One that
    /// is not is given up on.
    pub fn restart_after_failure(&mut self, token: u64) -> bool {
        let first = self.restarted.insert(token);
        if !first {
            self.given_up.insert(token);
        }
        first
    }

    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    pub fn clear_problems(&mut self) {
        self.problems.clear();
    }

    pub fn add_problem(&mut self, problem: String) {
        // The newest last; a list of thousands helps no one.
        const MAX_PROBLEMS: usize = 200;
        if self.problems.len() == MAX_PROBLEMS {
            self.problems.remove(0);
        }
        self.problems.push(problem);
    }
}

impl Drop for Plugins {
    fn drop(&mut self) {
        // The scanner ends its child, so none is left running.
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(thread) = self.scan_thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_catalog_nothing_is_made_and_the_list_says_why() {
        let mut p = Plugins::new();
        let r = PluginRef::new("org.tinvo.test.unit").unwrap();
        assert_eq!(p.make(r, Role::Instrument, 48_000, None).err().as_deref(), Some("no plugin catalog was read"));
        p.scan_error = Some("no".into());
        assert_eq!(p.why_no_catalog(), "the plugin scan failed (no)");
        let (_tx, rx) = mpsc::channel();
        p.scanning = Some(rx);
        assert_eq!(p.why_no_catalog(), "the plugin scan is still running");
        assert_eq!(p.poll_scan(), None);
        assert_eq!(p.name(r), "org.tinvo.test.unit");
        assert!(p.choices(Role::Effect).is_empty() && !p.any_made());
    }

    #[test]
    fn a_failed_instance_is_restarted_once_and_problems_are_capped() {
        let mut p = Plugins::new();
        assert!(p.restart_after_failure(7) && !p.given_up(7));
        assert!(!p.restart_after_failure(7) && p.given_up(7) && p.restart_after_failure(8));
        p.forget(8);
        assert!(p.restart_after_failure(8), "forgotten");
        assert!(p.lend(8) && !p.lend(8), "once till it is back");
        p.lent_back(8);
        assert!(p.lend(8));
        p.forget_made();
        assert!(p.restart_after_failure(7) && !p.given_up(7), "a new engine's instances start afresh");
        for i in 0..250 {
            p.add_problem(format!("{i}"));
        }
        assert_eq!(p.problems().len(), 200);
        assert_eq!((p.problems()[0].as_str(), p.problems()[199].as_str()), ("50", "249"));
        p.clear_problems();
        assert!(p.problems().is_empty());
    }

    #[test]
    fn a_scan_that_ends_without_a_catalog_is_reported() {
        let mut p = Plugins::new();
        let (tx, rx) = mpsc::channel();
        p.scanning = Some(rx);
        drop(tx);
        assert_eq!(p.poll_scan(), Some(Err("the plugin scan stopped".into())));
        assert!(!p.scanning() && p.scan_error() == Some("the plugin scan stopped"));
        assert_eq!(p.poll_scan(), None);
        let (tx, rx) = mpsc::channel();
        p.scanning = Some(rx);
        tx.send(Ok(Catalog::default())).unwrap();
        assert_eq!(p.poll_scan(), Some(Ok(0)));
        assert!(p.catalog().is_some());
    }
}
