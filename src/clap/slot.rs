//! A place in the engine for one plugin instance: a plugin track's
//! instrument, or a plugin insert's effect.
//!
//! The engine never drops an instance: every way one leaves a slot hands it
//! back, stopped, for the engine to return to the UI thread. A slot can
//! lend its instance out to be restarted ([`PluginSlot::lend`]) and keep its
//! place until it comes back ([`PluginSlot::give_back`]); an instance put
//! in meanwhile, or the slot going, ends that. Parameter changes given
//! while it is lent wait in the slot for the instance to come back.

use super::PluginRef;
use super::host::{Instance, MAX_PARAM_EVENTS, Role};
use std::fmt;

enum Held {
    Empty,
    Playing(Box<Instance>),
    /// Lent out to be restarted: the token of the instance it waits for.
    Lent(u64),
}

/// Parameter changes a slot keeps for an instance lent out, one a
/// parameter.
const WAITING: usize = 32;

/// A parameter change for an instance lent out: id, value, serial.
#[derive(Clone, Copy, Default)]
struct Change(u32, f64, u64);

/// A slot for an instance of one plugin, in one role, at one sample rate.
pub struct PluginSlot {
    plugin: PluginRef,
    role: Role,
    sample_rate: f64,
    held: Held,
    waiting: [Change; WAITING],
    waiting_count: usize,
    /// The last serial refused while changes waited, acknowledged once
    /// they are handed over.
    refused: u64,
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
        PluginSlot {
            plugin,
            role,
            sample_rate,
            held: Held::Empty,
            waiting: [Change::default(); WAITING],
            waiting_count: 0,
            refused: 0,
        }
    }

    pub fn plugin(&self) -> PluginRef {
        self.plugin
    }

    /// The instance playing here, given first what waited for it.
    pub fn instance(&mut self) -> Option<&mut Instance> {
        self.hand_over();
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

    /// The frames the instance playing here delays its output by; none
    /// while there is none (a slot lent out passes its input, or is silent).
    pub fn latency(&self) -> usize {
        match &self.held {
            Held::Playing(i) => i.latency() as usize,
            _ => 0,
        }
    }

    /// Sets parameter `id` of the instance here ([`Instance::set_param`]),
    /// or of the one lent out once it is back; one the instance has no room
    /// for yet waits too. False where there is none, or it cannot take the
    /// change. Real-time safe.
    pub fn set_param(&mut self, id: u32, value: f64, serial: u64) -> bool {
        self.hand_over();
        match &mut self.held {
            // Behind what still waits, so it is not overtaken.
            Held::Playing(i) if self.waiting_count == 0 => {
                if i.set_param(id, value, serial) {
                    return true;
                }
                if i.params_queued() < MAX_PARAM_EVENTS {
                    return false;
                }
            }
            Held::Playing(_) | Held::Lent(_) => {}
            Held::Empty => return false,
        }
        self.wait(Change(id, value, serial))
    }

    /// Acknowledges `serial` with nothing set, as a change the instance
    /// here, or the one lent out once it is back, refused. Real-time safe.
    pub fn acknowledge(&mut self, serial: u64) {
        self.hand_over();
        match &mut self.held {
            Held::Playing(i) if self.waiting_count == 0 => i.acknowledge(serial),
            Held::Playing(_) | Held::Lent(_) => self.refused = self.refused.max(serial),
            Held::Empty => {}
        }
    }

    /// Keeps `change` for later, replacing one waiting for its parameter.
    fn wait(&mut self, change: Change) -> bool {
        let n = self.waiting_count;
        if let Some(c) = self.waiting[..n].iter_mut().find(|c| c.0 == change.0) {
            *c = change;
        } else if n < WAITING {
            self.waiting[n] = change;
            self.waiting_count += 1;
        } else {
            return false;
        }
        true
    }

    /// Gives the instance playing here the changes that waited for it,
    /// keeping those its queue has no room for yet.
    fn hand_over(&mut self) {
        let Held::Playing(i) = &mut self.held else { return };
        let mut kept = 0;
        for k in 0..self.waiting_count {
            let c = self.waiting[k];
            if !i.set_param(c.0, c.1, c.2) && i.params_queued() == MAX_PARAM_EVENTS {
                self.waiting[kept] = c;
                kept += 1;
            }
        }
        self.waiting_count = kept;
        if kept == 0 && self.refused > 0 {
            i.acknowledge(std::mem::take(&mut self.refused));
        }
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
        // What waited was for the instance lent, whatever comes next.
        self.waiting_count = 0;
        self.refused = 0;
        match std::mem::replace(&mut self.held, next) {
            Held::Playing(mut i) => {
                i.stop();
                Some(i)
            }
            _ => None,
        }
    }
}
