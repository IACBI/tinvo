//! A bounded queue that several threads may push into and pop from, without
//! blocking or allocating.
//!
//! It wraps an SPSC ring with a try-lock on each end: a push or pop that
//! finds its end busy gives up at once instead of waiting. That suits MIDI,
//! where one input thread pushes at a time (but a closed input's thread and
//! a newly opened one may overlap briefly) and one engine pops at a time
//! (but the engine is rebuilt when the audio device changes). Nothing is
//! ever lost silently: a failed push hands the value back.

use super::spsc::{self, Consumer, Producer};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Mailbox<T> {
    tx: UnsafeCell<Producer<T>>,
    rx: UnsafeCell<Consumer<T>>,
    tx_busy: AtomicBool,
    rx_busy: AtomicBool,
}

// SAFETY: each end is only touched while its try-lock is held (acquired
// with Acquire, released with Release), so at most one thread uses the
// producer and at most one the consumer at any time, which is exactly the
// SPSC ring's contract. Values of `T` cross threads, hence `T: Send`.
unsafe impl<T: Send> Sync for Mailbox<T> {}
unsafe impl<T: Send> Send for Mailbox<T> {}

impl<T: Send> Mailbox<T> {
    /// Creates a mailbox holding at least `capacity` items.
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = spsc::channel(capacity);
        Self {
            tx: UnsafeCell::new(tx),
            rx: UnsafeCell::new(rx),
            tx_busy: AtomicBool::new(false),
            rx_busy: AtomicBool::new(false),
        }
    }

    /// Adds `value`, or hands it back if the mailbox is full or another
    /// thread is pushing right now.
    pub fn push(&self, value: T) -> Result<(), T> {
        if self.tx_busy.swap(true, Ordering::Acquire) {
            return Err(value);
        }
        // SAFETY: the try-lock above gives this thread sole use of `tx`.
        let r = unsafe { (*self.tx.get()).push(value) };
        self.tx_busy.store(false, Ordering::Release);
        r
    }

    /// Takes the oldest value, if any and if no other thread is popping.
    pub fn pop(&self) -> Option<T> {
        if self.rx_busy.swap(true, Ordering::Acquire) {
            return None;
        }
        // SAFETY: the try-lock above gives this thread sole use of `rx`.
        let v = unsafe { (*self.rx.get()).pop() };
        self.rx_busy.store(false, Ordering::Release);
        v
    }

    /// Discards everything queued now.
    pub fn clear(&self) {
        while self.pop().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn keeps_order_and_reports_full() {
        let m = Mailbox::new(4);
        for i in 0..4 {
            m.push(i).unwrap();
        }
        assert_eq!(m.push(9), Err(9));
        assert_eq!((m.pop(), m.pop()), (Some(0), Some(1)));
        m.clear();
        assert_eq!(m.pop(), None);
    }

    /// Two pushers and two poppers at once: every value accepted comes out
    /// exactly once, and each pusher's values come out in its own order.
    #[test]
    fn concurrent_ends_lose_and_duplicate_nothing() {
        const N: u32 = 200_000;
        let m = Arc::new(Mailbox::<u32>::new(64));
        let accepted = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let pushers: Vec<_> = (0..2u32)
            .map(|p| {
                let (m, accepted) = (m.clone(), accepted.clone());
                std::thread::spawn(move || {
                    for i in 0..N {
                        if m.push(p << 31 | i).is_ok() {
                            accepted.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        let poppers: Vec<_> = (0..2)
            .map(|_| {
                let (m, done) = (m.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut got = Vec::new();
                    loop {
                        match m.pop() {
                            Some(v) => got.push(v),
                            None if done.load(Ordering::Acquire) => {
                                // One last sweep: the other popper may have
                                // held the lock when we looked.
                                while let Some(v) = m.pop() {
                                    got.push(v);
                                }
                                break got;
                            }
                            None => std::hint::spin_loop(),
                        }
                    }
                })
            })
            .collect();
        for p in pushers {
            p.join().unwrap();
        }
        done.store(true, Ordering::Release);
        let mut all: Vec<Vec<u32>> = poppers.into_iter().map(|p| p.join().unwrap()).collect();
        all.push(std::iter::from_fn(|| m.pop()).collect());
        for got in &all {
            for p in 0..2u32 {
                let mine: Vec<u32> = got.iter().copied().filter(|v| v >> 31 == p).collect();
                assert!(mine.windows(2).all(|w| w[0] < w[1]), "pusher {p} out of order");
            }
        }
        let mut flat: Vec<u32> = all.concat();
        let n = flat.len();
        flat.sort_unstable();
        flat.dedup();
        assert_eq!(flat.len(), n, "a value came out twice");
        assert_eq!(n, accepted.load(Ordering::Relaxed));
        assert!(n > 0);
    }
}
