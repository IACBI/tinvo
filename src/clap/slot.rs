//! A place in the engine for one plugin instance: a plugin track's
//! instrument, or a plugin insert's effect.
//!
//! The engine never drops an instance: every way one leaves a slot hands it
//! back, stopped, for the engine to return to the UI thread. A slot can
//! lend its instance out to be restarted ([`PluginSlot::lend`]) and keep its
//! place until it comes back ([`PluginSlot::give_back`]); an instance put
//! in meanwhile, or the slot going, ends that.

use super::PluginRef;
use super::host::{Instance, Role};
use std::fmt;

enum Held {
    Empty,
    Playing(Box<Instance>),
    /// Lent out to be restarted: the token of the instance it waits for.
    Lent(u64),
}

/// A slot for an instance of one plugin, in one role, at one sample rate.
pub struct PluginSlot {
    plugin: PluginRef,
    role: Role,
    sample_rate: f64,
    held: Held,
}

impl fmt::Debug for PluginSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let held = match &self.held {
            Held::Empty => "empty".to_string(),
            Held::Playing(i) => format!("playing {}", i.token()),
            Held::Lent(t) => format!("lent {t}"),
        };
        write!(f, "PluginSlot({:?}, {held})", self.plugin)
    }
}

impl PluginSlot {
    /// An empty slot for `plugin` as a `role`, at `sample_rate`.
    pub fn new(plugin: PluginRef, role: Role, sample_rate: f64) -> PluginSlot {
        PluginSlot { plugin, role, sample_rate, held: Held::Empty }
    }

    pub fn plugin(&self) -> PluginRef {
        self.plugin
    }

    /// The instance playing here.
    pub fn instance(&mut self) -> Option<&mut Instance> {
        match &mut self.held {
            Held::Playing(i) => Some(i),
            _ => None,
        }
    }

    pub fn has_instance(&self) -> bool {
        matches!(self.held, Held::Playing(_))
    }

    /// The sample rate its instances must run at.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Whether the instance playing here has a tail still to play
    /// ([`Instance::has_tail`]).
    pub fn has_tail(&self) -> bool {
        matches!(&self.held, Held::Playing(i) if i.has_tail())
    }

    /// Whether this slot's plugin, role and sample rate are `instance`'s.
    pub fn fits(&self, instance: &Instance) -> bool {
        instance.plugin() == self.plugin && instance.role() == self.role && instance.sample_rate() == self.sample_rate
    }

    /// Puts `instance` in: returns the one it replaces, stopped, if any; or
    /// hands `instance` back if it does not fit ([`PluginSlot::fits`]).
    /// Real-time safe.
    pub fn attach(&mut self, instance: Box<Instance>) -> Result<Option<Box<Instance>>, Box<Instance>> {
        if !self.fits(&instance) {
            return Err(instance);
        }
        Ok(self.take_and(Held::Playing(instance)))
    }

    /// Takes the instance out, stopped, leaving the slot empty.
    /// Real-time safe.
    pub fn take(&mut self) -> Option<Box<Instance>> {
        self.take_and(Held::Empty)
    }

    /// Lends out the instance with `token`, stopped, keeping its place.
    /// The notes it holds are released on its next call, so one given back
    /// without a restart (which clears them) holds none the engine forgot.
    /// Real-time safe.
    pub fn lend(&mut self, token: u64) -> Option<Box<Instance>> {
        match &self.held {
            Held::Playing(i) if i.token() == token => self.take_and(Held::Lent(token)).map(|mut i| {
                i.release_all();
                i
            }),
            _ => None,
        }
    }

    /// Puts back an instance this slot lent, or hands it back if the slot
    /// does not wait for it. Real-time safe.
    pub fn give_back(&mut self, instance: Box<Instance>) -> Result<(), Box<Instance>> {
        match self.held {
            Held::Lent(t) if t == instance.token() && self.fits(&instance) => {
                self.held = Held::Playing(instance);
                Ok(())
            }
            _ => Err(instance),
        }
    }

    fn take_and(&mut self, next: Held) -> Option<Box<Instance>> {
        match std::mem::replace(&mut self.held, next) {
            Held::Playing(mut i) => {
                i.stop();
                Some(i)
            }
            _ => None,
        }
    }
}
