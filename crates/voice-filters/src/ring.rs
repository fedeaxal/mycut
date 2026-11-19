//! Single-producer single-consumer f32 ring. Samples live in AtomicU32 slots,
//! so there is no unsafe here and nothing that can block the audio threads.

use std::sync::Arc;
use std::sync::atomic::{
    AtomicU32, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};

struct Inner {
    slots: Box<[AtomicU32]>,
    mask: usize,
    /// Total samples ever written / read; their difference is the fill.
    head: AtomicUsize,
    tail: AtomicUsize,
}

pub struct Producer(Arc<Inner>);
pub struct Consumer(Arc<Inner>);

pub fn ring(min_capacity: usize) -> (Producer, Consumer) {
    let cap = min_capacity.next_power_of_two();
    let inner = Arc::new(Inner {
        slots: (0..cap).map(|_| AtomicU32::new(0)).collect(),
        mask: cap - 1,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
    });
    (Producer(inner.clone()), Consumer(inner))
}

impl Producer {
    /// Returns false when full; the caller drops the sample.
    pub fn push(&mut self, v: f32) -> bool {
        let r = &self.0;
        let head = r.head.load(Relaxed);
        if head - r.tail.load(Acquire) > r.mask {
            return false;
        }
        r.slots[head & r.mask].store(v.to_bits(), Relaxed);
        r.head.store(head + 1, Release);
        true
    }

    /// Pushes `block` up to the first sample that does not fit; the rest is dropped.
    pub fn push_all(&mut self, block: &[f32]) {
        for s in block {
            if !self.push(*s) {
                break;
            }
        }
    }
}

// Nobody asks whether it is empty: the reader wants the fill as a number.
#[allow(clippy::len_without_is_empty)]
impl Consumer {
    pub fn len(&self) -> usize {
        self.0.head.load(Acquire) - self.0.tail.load(Relaxed)
    }

    pub fn pop(&mut self) -> Option<f32> {
        let r = &self.0;
        let tail = r.tail.load(Relaxed);
        if tail == r.head.load(Acquire) {
            return None;
        }
        let v = f32::from_bits(r.slots[tail & r.mask].load(Relaxed));
        r.tail.store(tail + 1, Release);
        Some(v)
    }

    pub fn skip(&mut self, n: usize) {
        let n = n.min(self.len());
        self.0.tail.fetch_add(n, Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_order_and_capacity() {
        let (mut p, mut c) = ring(4);
        assert!((0..4).all(|i| p.push(i as f32)));
        assert!(!p.push(9.0), "full ring must refuse");
        assert_eq!(c.len(), 4);
        c.skip(1);
        assert_eq!(c.pop(), Some(1.0));
        assert!(p.push(4.0));
        assert_eq!([c.pop(), c.pop(), c.pop(), c.pop()], [Some(2.0), Some(3.0), Some(4.0), None]);
    }

    #[test]
    fn survives_a_threaded_stream() {
        let (mut p, mut c) = ring(256);
        let writer = std::thread::spawn(move || {
            let mut i = 0u32;
            while i < 200_000 {
                if p.push(i as f32) {
                    i += 1;
                }
            }
        });
        let mut expect = 0u32;
        while expect < 200_000 {
            if let Some(v) = c.pop() {
                assert_eq!(v, expect as f32);
                expect += 1;
            }
        }
        writer.join().unwrap();
    }
}
