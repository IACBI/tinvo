//! Bounded, wait-free single-producer single-consumer queue.
//!
//! Used to send commands from the UI thread to the audio thread: `push` and
//! `pop` never block, never allocate and never take a lock.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Keeps the producer and consumer indices on separate cache lines.
#[repr(align(64))]
struct CachePadded<T>(T);

struct Ring<T> {
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
    /// Next slot to read; written only by the consumer.
    head: CachePadded<AtomicUsize>,
    /// Next slot to write; written only by the producer.
    tail: CachePadded<AtomicUsize>,
}

// SAFETY: slot access is coordinated through `head`/`tail` with
// acquire/release ordering so each slot is owned by exactly one side at a
// time; values of `T` move between threads, hence `T: Send`.
unsafe impl<T: Send> Sync for Ring<T> {}
unsafe impl<T: Send> Send for Ring<T> {}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        let head = *self.head.0.get_mut();
        let tail = *self.tail.0.get_mut();
        let mut i = head;
        while i != tail {
            // SAFETY: slots in [head, tail) were written and never read.
            unsafe { (*self.slots[i & self.mask].get()).assume_init_drop() };
            i = i.wrapping_add(1);
        }
    }
}

pub struct Producer<T> {
    ring: Arc<Ring<T>>,
}

pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
}

/// Creates a queue holding at least `capacity` items (rounded up to a power
/// of two).
pub fn channel<T: Send>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let cap = capacity.max(2).next_power_of_two();
    let slots = (0..cap).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect();
    let ring = Arc::new(Ring {
        slots,
        mask: cap - 1,
        head: CachePadded(AtomicUsize::new(0)),
        tail: CachePadded(AtomicUsize::new(0)),
    });
    (Producer { ring: ring.clone() }, Consumer { ring })
}

impl<T> Producer<T> {
    /// Enqueues `value`, or hands it back if the queue is full.
    pub fn push(&mut self, value: T) -> Result<(), T> {
        let r = &*self.ring;
        let tail = r.tail.0.load(Ordering::Relaxed);
        let head = r.head.0.load(Ordering::Acquire);
        if tail.wrapping_sub(head) > r.mask {
            return Err(value);
        }
        // SAFETY: the slot at `tail` is outside [head, tail), so the consumer
        // will not touch it until we publish the new tail below.
        unsafe { (*r.slots[tail & r.mask].get()).write(value) };
        r.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    pub fn capacity(&self) -> usize {
        self.ring.mask + 1
    }
}

impl<T> Consumer<T> {
    pub fn pop(&mut self) -> Option<T> {
        let r = &*self.ring;
        let head = r.head.0.load(Ordering::Relaxed);
        let tail = r.tail.0.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        // SAFETY: the slot at `head` was fully written before the producer's
        // release store of `tail`, which our acquire load observed.
        let value = unsafe { (*r.slots[head & r.mask].get()).assume_init_read() };
        r.head.0.store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_and_capacity() {
        let (mut tx, mut rx) = channel::<u32>(3);
        assert_eq!(tx.capacity(), 4);
        for i in 0..4 {
            tx.push(i).unwrap();
        }
        assert_eq!(tx.push(99), Err(99));
        assert_eq!(rx.pop(), Some(0));
        tx.push(4).unwrap();
        assert_eq!((1..=4).map(|_| rx.pop().unwrap()).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert_eq!(rx.pop(), None);
    }

    #[test]
    fn drops_unread_items() {
        let marker = Arc::new(());
        {
            let (mut tx, _rx) = channel(8);
            for _ in 0..5 {
                tx.push(marker.clone()).unwrap();
            }
            assert_eq!(Arc::strong_count(&marker), 6);
        }
        assert_eq!(Arc::strong_count(&marker), 1);
    }

    #[test]
    fn threaded_stress_preserves_order() {
        const N: u64 = 1_000_000;
        let (mut tx, mut rx) = channel::<u64>(64);
        let producer = std::thread::spawn(move || {
            let mut i = 0;
            while i < N {
                if tx.push(i).is_ok() {
                    i += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        let mut expected = 0;
        while expected < N {
            if let Some(v) = rx.pop() {
                assert_eq!(v, expected);
                expected += 1;
            } else {
                std::hint::spin_loop();
            }
        }
        producer.join().unwrap();
    }
}
