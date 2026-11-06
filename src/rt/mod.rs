//! Primitives for crossing the real-time boundary without locks or allocation.

pub mod spsc;

use std::sync::atomic::{AtomicU32, Ordering};

/// An `f32` that can be shared between threads (stored as its bit pattern).
#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub const fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }

    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Raises the stored value to `v` if `v` is larger. Only valid for
    /// non-negative finite values, whose bit patterns order like the values.
    pub fn fetch_max_non_negative(&self, v: f32) {
        debug_assert!(v >= 0.0);
        self.0.fetch_max(v.to_bits(), Ordering::Relaxed);
    }

    /// Returns the value and resets it to zero.
    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_max_orders_like_floats() {
        let a = AtomicF32::new(0.0);
        for v in [0.1f32, 0.05, 0.7, 0.3, 1e-20, 2.5] {
            a.fetch_max_non_negative(v);
        }
        assert_eq!(a.take(), 2.5);
        assert_eq!(a.load(), 0.0);
    }
}
