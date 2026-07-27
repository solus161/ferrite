use std::time::Duration;
use std::{array, thread};
use std::cell::UnsafeCell;
use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::exceptions::CustomError;
use crate::buffer::Buffer;
use crate::utils::next_wrapped;

/// A lock-free circular ring structure
/// - N slot
/// - Item being Buffer as array of M elements of type T
/// - N must be power of 2
/// Free-lock: the access of Buffer in Ring is not guarded by any Mutex
/// but the checking-for-catch-up mechanism in RingProducer.
/// Even then, that mechanism could be turned off to get the desired behavior
/// of RingConsumer missing Buffer
struct Ring<T: Copy + Default, const N: usize, const M: usize> {
    slots: [UnsafeCell<Buffer<T, M>>; N],
    head: RingCursor<N>,
}

// Arc in only Send if Ring is Sync + Send
unsafe impl<T: Copy + Default + Send, const N: usize, const M: usize> Sync
    for Ring<T, N, M> {}

/// Ring of N slot with buffer as array of M elements of type T
impl<T: Copy + Default, const N: usize, const M: usize> Ring<T, N, M> {
    pub fn new() -> Self {
        let slots = array::from_fn(|_| UnsafeCell::new(Buffer::<T, M>::new()));
        let head = RingCursor::<N>::new();
        Self { slots, head }
    }

    /// Unconditionally write into the slot at `head` and advance it.
    ///
    /// Gating is the caller's job: `RingProducer::write` checks every registered
    /// consumer cursor first, so by the time we get here the slot is known to be
    /// free.
    pub fn write(&self, src: &[T]) -> Result<(), CustomError> {
        let head = self.head.index();
        let slot = self.slots.get(head).ok_or(CustomError::InvalidIndex)?;
        let buffer = slot.get();

        // Deref for writing
        unsafe { (*buffer).write(src)? };
        self.head.next();

        Ok(())
    }

    pub fn slot(&self, i: usize) -> Result<&UnsafeCell<Buffer<T, M>>, CustomError> {
        self.slots.get(i).ok_or(CustomError::InvalidIndex)
    }

    fn head(&self) -> &RingCursor<N> {
        &self.head
    }
}

/// Producer of that Ring, hold Ring and consumers
/// For `blocking` = true, the producer writes regardless of consumers reading states
pub struct RingProducer<T: Copy + Default, const N: usize, const M: usize> {
    ring: Arc<Ring<T, N, M>>,
    /// Live consumers: the thread handle paired with the cursor it reads from.
    /// Kept as a `Vec` because `write` walks the whole thing on every call —
    /// this runs in the SDR callback, so the scan wants to be a flat iteration.
    consumers: Vec<(JoinHandle<()>, Arc<RingCursor<N>>)>,
    blocking: bool,
}

impl<T: Copy + Default, const N: usize, const M: usize> RingProducer<T, N, M> {
    pub fn new(blocking: bool) -> Self {
        Self {
            ring: Arc::new(Ring::new()),
            consumers: Vec::new(),
            blocking
        }
    }

    pub fn write(&mut self, src: &[T]) -> Result<(), CustomError> {
        // Reap consumers that have already exited first, so a dead cursor does
        // not gate this write.
        self.consumers.retain(|(h, _)| !h.is_finished());

        let head = self.ring.head();

        // If not blocking, just write
        if !self.blocking {
            self.ring.write(src)?;
            self.consumers.iter().for_each(|(_, cursor)| {
                if head.catch_up(cursor) {
                    // This cursor got catched-up, must be set to head 
                    cursor.jump_to_cursor(head);
                };
            });
        } else {
            let catch_up = self.consumers.iter().any(|(_, cursor)| head.catch_up(cursor));
            if !catch_up {
                self.ring.write(src)?;
            };
        };
        

        // Wake the survivors.
        self.consumers.iter().for_each(|(h, _)| h.thread().unpark());
        Ok(())
    }

    pub fn subscribe(&self) -> RingConsumer<T, N, M> {
        RingConsumer::new(self.ring.clone())
    }

    /// Take ownership of a spawned consumer thread so the producer can wake it
    /// (unpark) and reap it on shutdown.
    pub fn add_consumer(&mut self, thread: JoinHandle<()>, cursor: Arc<RingCursor<N>>) {
        self.consumers.push((thread, cursor));
    }

    /// Register a cursor without a real reader, so gating can be driven step by
    /// step from a single thread. The placeholder thread parks forever and is
    /// never finished, so `retain` keeps the entry — tests using this must not
    /// call `join_all`.
    #[cfg(test)]
    fn register_cursor_for_test(&mut self, cursor: Arc<RingCursor<N>>) {
        self.consumers
            .push((thread::spawn(|| loop { thread::park() }), cursor));
    }

    /// Block until every consumer thread has finished.
    pub fn join_all(&mut self) {
        for (h, _) in self.consumers.drain(..) {
            let _ = h.join();
        }
    }
}

/// Consumer of that Ring
pub struct RingConsumer<T: Copy + Default, const N: usize, const M: usize> {
    ring: Arc<Ring<T, N, M>>,
    head: Arc<RingCursor<N>>,
}

impl<T: Copy + Default, const N: usize, const M: usize> RingConsumer<T, N, M> {
    pub fn new(
        ring: Arc<Ring<T, N, M>>,
    ) -> Self {
        Self { ring, head: Arc::new(RingCursor::new()) }
    }

    pub fn head(&self) -> Arc<RingCursor<N>> {
        self.head.clone()
    }

    pub fn read(&self) -> Result<&[T], CustomError> {
        // Could only read if consumer head < ring head
        let ring_head = self.ring.head();
        if *self.head < *ring_head {
            let head_index = self.head.index();
            let slot = self.ring.slot(head_index)?;
            let output = unsafe { &(*slot.get()) };

            // Move the cursor. This is what releases the slot back to the
            // producer, which polls it through the registry.
            self.head.next();

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

    /// Jump to another cursor
    pub fn jump_to_cursor(&self, other: &Self) {
        let other_round = other.round();
        let other_index = other.index();
        self.0.0.store(other_round, Ordering::Release);
        self.0.1.store(other_index, Ordering::Release);
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
    }

    // ── Blocking mode ───────────────────────────────────────────────────────

    /// The producer refuses to overwrite a slot the slowest consumer has not
    /// read, so `head` stalls exactly N ahead of that cursor and stays there.
    #[test]
    fn test_blocking_gates_at_capacity() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.head());

        for i in 0..4u8 {
            producer.write(&[i; 2]).unwrap();
        }
        assert_eq!(producer.ring.head.tuple(), (1, 0), "one full lap fits");

        // Consumer never reads, so every further write is refused.
        for i in 4..8u8 {
            producer.write(&[i; 2]).unwrap();
            assert_eq!(
                producer.ring.head.tuple(),
                (1, 0),
                "write {i} must be gated at capacity"
            );
        }
        assert_eq!(
            consumer.head.tuple(),
            (0, 0),
            "blocking mode never moves a consumer cursor"
        );
    }

    /// Draining one slot releases exactly one write, then it gates again.
    #[test]
    fn test_blocking_resumes_after_drain() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.head());

        for i in 0..4u8 {
            producer.write(&[i; 2]).unwrap();
        }
        producer.write(&[99; 2]).unwrap();
        assert_eq!(producer.ring.head.tuple(), (1, 0), "full");

        // Blocking mode preserves order: the oldest entry is still there.
        assert_eq!(consumer.read().unwrap().to_vec(), vec![0u8, 0u8]);

        producer.write(&[99; 2]).unwrap();
        assert_eq!(
            producer.ring.head.tuple(),
            (1, 1),
            "one slot freed buys exactly one write"
        );

        producer.write(&[98; 2]).unwrap();
        assert_eq!(producer.ring.head.tuple(), (1, 1), "gated again");
    }

    // ── Non-blocking mode ───────────────────────────────────────────────────

    /// The producer never stalls; it overwrites and drags any lapped consumer
    /// cursor up to the head.
    #[test]
    fn test_non_blocking_never_stalls_and_drags_cursor() {
        let mut producer = RingProducer::<u8, 4, 2>::new(false);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.head());

        // Head advances on every single write, consumer or not.
        for i in 0..12u8 {
            let before = producer.ring.head.tuple();
            producer.write(&[i; 2]).unwrap();
            assert_ne!(
                producer.ring.head.tuple(),
                before,
                "write {i} must land in non-blocking mode"
            );
        }
        assert_eq!(producer.ring.head.tuple(), (3, 0), "three full laps");

        // The stalled cursor was dragged along rather than left behind.
        assert_eq!(consumer.head.tuple(), (3, 0));
        assert!(
            consumer.read().is_err(),
            "cursor sits at head, nothing new to read yet"
        );
    }

    /// After being dragged, the consumer resumes on the newest data rather than
    /// on a stale slot.
    #[test]
    fn test_non_blocking_consumer_resumes_on_fresh_data() {
        let mut producer = RingProducer::<u8, 4, 2>::new(false);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.head());

        for i in 0..4u8 {
            producer.write(&[i; 2]).unwrap();
        }
        assert_eq!(consumer.head.tuple(), (1, 0), "dragged to head");

        producer.write(&[42; 2]).unwrap();
        assert_eq!(
            consumer.read().unwrap().to_vec(),
            vec![42u8, 42u8],
            "reads the newest write, not a stale slot"
        );
    }

    /// Documents the current drag point: the cursor is moved after the write, so
    /// it is dragged when the lag reaches N-1, and it lands on `head` — which
    /// discards a whole ring of buffers that had not actually been overwritten.
    /// Pinning this so a deliberate change to the policy shows up as a diff.
    #[test]
    fn test_non_blocking_drag_discards_unclobbered_entries() {
        let mut producer = RingProducer::<u8, 4, 2>::new(false);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.head());

        // Exactly N writes: slots 0..3 hold seq 0..3 and nothing has been
        // overwritten yet, so all four are still readable.
        for i in 0..4u8 {
            producer.write(&[i; 2]).unwrap();
        }

        // ...but the consumer has been moved to the head and gets none of them.
        assert_eq!(consumer.head.tuple(), (1, 0));
        assert!(consumer.read().is_err(), "all 4 intact buffers skipped");
    }

    // ── Threaded smoke test ─────────────────────────────────────────────────

    #[test]
    fn test_ring_blocking() {
        use std::thread;
        let mut producer = RingProducer::<u8, 8, 8>::new(true);

        let callback = |buf: &[u8]| println!("Consumer receives: {:?}", buf);
        let consumer = producer.subscribe();
        let cursor = consumer.head();
        let handle = consumer.start_reading(callback, Some(1));
        
        // Producer now owns the consumer thread.
        producer.add_consumer(handle, cursor);

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
