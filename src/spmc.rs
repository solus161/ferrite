use std::time::Duration;
use std::{array, thread};
use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::exceptions::CustomError;
use crate::buffer::Buffer;
use crate::utils::next_wrapped;

/// A lock-free circular ring structure
/// - N slot
/// - Item being Buffer as array of M elements of type T
/// - N must be power of 2
pub struct Ring<T: Copy + Default, const N: usize, const M: usize> {
    slots: [AtomicPtr<Buffer<T, M>>; N],
    head: RingCursor<N>,
    tail: RingCursor<N>, // slowest consumer pointer
    blocking: bool,
}

/// Ring of N slot with buffer as array of M elements of type T
impl<T: Copy + Default, const N: usize, const M: usize> Ring<T, N, M> {
    pub fn new(blocking: bool) -> Self {
        let slots = array::from_fn(|_| {
            let buffer = Box::new(Buffer::<T, M>::new());
            AtomicPtr::new(Box::into_raw(buffer))
        });
        let head = RingCursor::<N>::new();
        let tail = RingCursor::<N>::new();
        Self { slots, head, tail, blocking }
    }

    /// Writing with blocking behavior: 
    /// Writing with non-blocking behavior:
    pub fn write(&self, src: &[T]) -> Result<(), CustomError> {
        let blocked_write = self.blocking && self.head.catch_up(&self.tail);

        // TODO: Must handle blocked write
        if !blocked_write {
            let head = self.head.index();
            let slot = self.slots.get(head).ok_or(CustomError::InvalidIndex)?;
            let buffer = slot.load(Ordering::Relaxed);

            // Write
            unsafe { (*buffer).write(src)? }; 
            self.head.next();
        };

        Ok(())
    }

    pub fn slot(&self, i: usize) -> Result<&AtomicPtr<Buffer<T, M>>, CustomError> {
        self.slots.get(i).ok_or(CustomError::InvalidIndex)
    }

    fn head(&self) -> &RingCursor<N> {
        &self.head
    }

    fn set_tail(&self, cursor: &RingCursor<N>) {
        self.tail.set_to_lagged(cursor);
    }
}

/// Producer of that Ring
pub struct RingProducer<T: Copy + Default, const N: usize, const M: usize> {
    ring: Arc<Ring<T, N, M>>,
    consumers: Vec<JoinHandle<()>>,
}

impl<T: Copy + Default, const N: usize, const M: usize> RingProducer<T, N, M> {
    pub fn new(blocking: bool) -> Self {
        Self {
            ring: Arc::new(Ring::new(blocking)),
            consumers: Vec::new()
        }
    }

    pub fn write(&mut self, src: &[T]) -> Result<(), CustomError> {
        self.ring.write(src)?;
        // Drop handles of consumers that have already exited, then wake the rest.
        self.consumers.retain(|h| !h.is_finished());
        self.consumers.iter().for_each(|h| h.thread().unpark());
        Ok(())
    }

    pub fn subscribe(&self) -> RingConsumer<T, N, M> {
        RingConsumer::new(self.ring.clone())
    }

    /// Take ownership of a spawned consumer thread so the producer can wake it
    /// (unpark) and reap it on shutdown.
    pub fn add_consumer(&mut self, consumer: JoinHandle<()>) {
        self.consumers.push(consumer);
    }

    /// Block until every consumer thread has finished.
    pub fn join_all(&mut self) {
        for h in self.consumers.drain(..) {
            let _ = h.join();
        }
    }
}

/// Consumer of that Ring
pub struct RingConsumer<T: Copy + Default, const N: usize, const M: usize> {
    ring: Arc<Ring<T, N, M>>,
    head: RingCursor<N>,
}

impl<T: Copy + Default, const N: usize, const M: usize> RingConsumer<T, N, M> {
    pub fn new(
        ring: Arc<Ring<T, N, M>>,
    ) -> Self {
        Self { ring, head: RingCursor::new() }
    }

    pub fn read(&self) -> Result<&[T], CustomError> {
        // Could only read if consumer head < ring head
        let ring_head = self.ring.head();
        if self.head < *ring_head {
            let head_index = self.head.index();
            let slot = self.ring.slot(head_index)?;
            let output = unsafe { &(*slot.load(Ordering::Relaxed)) };

            // Move the cursor
            self.head.next();

            // Possibly set this as ring tail
            self.ring.set_tail(&self.head);
            Ok(output)
        } else {
            Err(CustomError::SlowProducer)
        }
    }

    /// Start reading from ring, run in a separated thread
    /// timeout in sec
    pub fn start_reading<F>(self, callback: F, timeout: Option<u64>) -> JoinHandle<()>
    where
        F: Fn(&[T]) + Send + 'static,
        T: Send + 'static,
    {
        thread::spawn(move || {
            loop {
                match self.read() {
                    Ok(buf) => callback(buf),
                    Err(_) => {
                        if let Some(x) = timeout {
                            thread::park_timeout(Duration::from_secs(x));
                            // May read nothing after timeout
                            match self.read() {
                                Ok(buf) => callback(buf),
                                Err(_) => break
                            }
                        };
                    }
                }
            }
        })
    }
}

/// An cursor supportings wrapped around just like Ring,
/// having N as Ring size, N must be power of 2
/// - first value: nbr of wrapped round
/// - second value: index within ring
#[derive(Debug)]
struct RingCursor<const N: usize>((AtomicUsize, AtomicUsize));

impl<const N: usize> RingCursor<N> {
    pub fn new() -> Self {
        Self((AtomicUsize::new(0), AtomicUsize::new(0)))
    }

    pub fn next(&self) {
        let current_pos = self.0.1.load(Ordering::Relaxed);
        let next_pos = next_wrapped(current_pos, N);
        self.0.1.store(next_pos, Ordering::Release);
        if next_pos < current_pos {
            let current_round = self.0.0.load(Ordering::Relaxed);
            self.0.0.store(current_round + 1, Ordering::Release);
        };
    }

    pub fn index(&self) -> usize {
        self.0.1.load(Ordering::Relaxed)
    }

    pub fn round(&self) -> usize {
        self.0.0.load(Ordering::Relaxed)
    }

    /// Return tuple of (round, index), supporting max, min
    pub fn tuple(&self) -> (usize, usize) {
        (self.round(), self.index())
    }

    /// Whether the leading cursor to overwrite the lagging cursor
    pub fn catch_up(&self, other: &Self) -> bool {
        self > other && self.index() == other.index() 
    }

    // Update state to lagged cursor's
    pub fn set_to_lagged(&self, other: &Self) {
        let self_pos = self.tuple();
        let other_pos = other.tuple();
        let (lagged_round, lagged_index) = self_pos.min(other_pos);
        self.0.0.store(lagged_round, Ordering::Release);
        self.0.1.store(lagged_index, Ordering::Release);
    }
}

impl<const N: usize> Eq for RingCursor<N> {}

impl<const N: usize> PartialEq for RingCursor<N> {
    fn eq(&self, other: &Self) -> bool {
        if self.round() != other.round() || self.index() != other.index() {
            return false
        };
        return true
    }
}

impl<const N: usize> PartialOrd for RingCursor<N> {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        match self.round().partial_cmp(&other.round()) {
            Some(CmpOrdering::Equal) => {
                self.index().partial_cmp(&other.index())
            },
            ord => ord
        }
    }
}

impl<const N: usize> Ord for RingCursor<N> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.round().cmp(&other.round()) {
            CmpOrdering::Equal => {
                self.index().cmp(&other.index())
            },
            ord => ord
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_cursor_order() {
        let cur1 = RingCursor::<8>::new();
        let cur2 = RingCursor::<8>::new();

        // Must be equal first
        assert_eq!(cur1, cur2);
        
        // Not equal
        cur1.next();
        assert_ne!(cur1, cur2);
        assert!(cur1 > cur2);

        // Max, min
        assert_eq!(cur1.tuple().max(cur2.tuple()), cur1.tuple());
        assert_eq!(cur1.tuple().min(cur2.tuple()), cur2.tuple());

        // Wrapped around order
        for _ in 0..8 {
            cur2.next();
        };
        assert!(cur1 < cur2);
        assert_eq!(cur2.round(), 1usize);
        assert_eq!(cur2.index(), 0usize);

        // Catch up
        cur2.next();
        assert!(cur2.catch_up(&cur1));

        // Set to lagged
        cur1.next();
        cur2.set_to_lagged(&cur1);
        assert_eq!(cur1, cur2);
    }

    #[test]
    fn test_ring_non_blocking() {
        use std::thread;
        let mut producer = RingProducer::<u8, 8, 8>::new(true);

        let callback = |buf: &[u8]| println!("Consumer receives: {:?}", buf);
        let consumer = producer.subscribe();
        let handle = consumer.start_reading(callback, Some(1));
        // Producer now owns the consumer thread.
        producer.add_consumer(handle);

        // Producer writing
        for i in 0..8u8 {
            thread::sleep(Duration::from_millis(100));
            producer.write(&[i; 8]).unwrap();
        }

        // No more writes: the consumer parks up to `timeout`, finds the ring
        // still empty, breaks its loop, and terminates — so join_all returns.
        producer.join_all();
    }
}
