use std::cell::UnsafeCell;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use std::{array, thread};

use crate::buffer::Buffer;
use crate::exceptions::CustomError;

/// The number of time the consumer retries when the buffer is partly written when reading
/// Each retry move the consumer's cursor 1 step forwards
const READ_RETRIES: usize = 4;

/// A lock-free circular ring.
/// - `N` slots, **must be a power of two** (the index is `seq & (N - 1)`)
/// - each slot is a [`Buffer`] of `M` elements of type `T`
///
/// # Synchronisation
///
/// The Ring's `head` is just an atomic sequence number
/// So it does not wrap when get to the end of the buffer
/// It's abundant and will never be exhausted: 4687 writes/s of u64 overflows in ~125 million yrs
/// Benefit: consumers could know exactly how many block they lag behind
///
/// # Index wrapped-around mechanism
///
/// The ring slot index is calculated as `s & (N - 1)`, with `s` is the above cursor number
///
/// # Block safety mechanism:
///
/// When `h` (head) is x times of N blocks ahead of `c` (consumer),
/// the `h` could overwrite what `c` is reading/copying.
/// So in that case, write is forbidden.
/// > A consumer at sequence `c` collides with a producer writing sequence `h`
/// > **iff** `c ≡ h (mod N)`. Since `c < h`, that means `h - c ∈ {N, 2N, ...}`.
/// > Therefore **`h - c < N` proves the producer is not in the consumer's slot.**
///
/// Consumers check that both before and after copying — see
/// [`RingConsumer::read_into`]. The head is published with `Release` *after* the
/// slot is written and read with `Acquire`, so a consumer that observes `head`
/// also observes the slot contents that preceded it.
///
/// # Why capacity is N-1, not N
///
/// Because the head is published *after* the write, `head - c == N` is
/// ambiguous: it means either "the producer is mid-write inside the consumer's
/// slot" (unsafe) or "the ring is exactly full and the producer is idle"
/// (perfectly safe). A single counter cannot tell those apart, so consumers must
/// treat `>= N` as unsafe.
///
/// A blocking producer therefore gates one slot early, at `N - 1`, keeping a
/// guard band so the ambiguous state is never reached. Without it, filling a
/// blocking ring would make consumers resync and skip blocks — destroying the
/// no-loss guarantee that is the entire point of blocking mode.
///
/// A non-blocking producer has no gate, so consumers do reach `>= N` and are
/// deliberately conservative there: they may discard a block that happened to be
/// intact, but never accept a torn one. That mode is lossy by definition.
struct Ring<T: Copy + Default, const N: usize, const M: usize> {
    slots: [UnsafeCell<Buffer<T, M>>; N],
    head: AtomicU64,
}

// Arc<Ring> is only Send if Ring is Sync + Send. UnsafeCell is unconditionally
// !Sync, so this has to be asserted by hand; the protocol above is the proof.
unsafe impl<T: Copy + Default + Send, const N: usize, const M: usize> Sync for Ring<T, N, M> {}

impl<T: Copy + Default, const N: usize, const M: usize> Ring<T, N, M> {
    pub fn new() -> Self {
        assert!(
            N.is_power_of_two(),
            "ring slot count N must be a power of two"
        );
        Self {
            slots: array::from_fn(|_| UnsafeCell::new(Buffer::<T, M>::new())),
            head: AtomicU64::new(0),
        }
    }

    /// Unconditionally write into the slot at `head` and publish it.
    ///
    /// Gating is the caller's job — [`RingProducer::write`] decides whether the
    /// slot is free before calling this.
    pub fn write(&self, src: &[T]) -> Result<(), CustomError> {
        // Relaxed is sufficient: the producer is the only writer of `head`, so
        // it always observes its own last store.
        let head = self.head.load(Ordering::Relaxed);
        let slot = self
            .slots
            .get(head as usize & (N - 1))
            .ok_or(CustomError::InvalidIndex)?;

        unsafe { (*slot.get()).write(src)? };

        self.publish();
        Ok(())
    }

    /// Store one element into the *open* (not yet published) slot at `head`.
    ///
    /// # Why holding a slot open is free
    ///
    /// This is for producers that compute their data one element at a time and
    /// would otherwise fill a staging array and memcpy it in. Spreading a block
    /// across many calls does **not** widen the tearing window: a consumer at
    /// sequence `c` can only be in this slot if `head - c ≡ 0 (mod N)`, which
    /// the lag check in [`RingConsumer::claim`] already excludes. Exposure is
    /// created by `head` *advancing* — a single instant either way — not by how
    /// long the producer spends before [`publish`](Self::publish).
    fn put(&self, i: usize, x: T) -> Result<(), CustomError> {
        let head = self.head.load(Ordering::Relaxed);
        let slot = self
            .slots
            .get(head as usize & (N - 1))
            .ok_or(CustomError::InvalidIndex)?;

        unsafe { (*slot.get()).set(i, x) }
    }

    /// Make the open slot visible to consumers.
    ///
    /// Release: everything written to the slot must be visible to any consumer
    /// that observes this new head value. This store is what publishes a block.
    fn publish(&self) {
        let head = self.head.load(Ordering::Relaxed);
        self.head.store(head + 1, Ordering::Release);
    }

    pub fn slot(&self, i: usize) -> Result<&UnsafeCell<Buffer<T, M>>, CustomError> {
        self.slots.get(i).ok_or(CustomError::InvalidIndex)
    }

    /// The published head, for consumers. Acquire pairs with the `Release` in
    /// [`write`](Self::write).
    fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// The head as seen by the producer itself, which needs no synchronisation
    /// with anyone.
    fn head_relaxed(&self) -> u64 {
        self.head.load(Ordering::Relaxed)
    }
}

/// Producer for a [`Ring`].
///
/// `blocking` selects the overrun policy:
/// - `true`  — refuse to overwrite a slot the slowest consumer has not read.
///   The producer stalls; no data is lost.
/// - `false` — always write. A lapped consumer notices on its next read and
///   resyncs itself; data is lost but the producer never stalls. This is what
///   the SDR callback uses, since stalling it would drop USB transfers.
pub struct RingProducer<T: Copy + Default, const N: usize, const M: usize> {
    ring: Arc<Ring<T, N, M>>,
    /// Live consumers: the thread handle paired with the cursor it reads from.
    /// Kept as a `Vec` because `write` walks the whole thing on every call in
    /// blocking mode — this runs in the SDR callback, so the scan wants to be a
    /// flat iteration.
    consumers: Vec<(JoinHandle<()>, Arc<AtomicU64>)>,
    blocking: bool,
    /// How many elements [`push`](Self::push) has put into the open slot. Zero
    /// whenever no block is under construction, which is always the case for a
    /// producer that only ever calls [`write`](Self::write).
    fill: usize,
}

impl<T: Copy + Default, const N: usize, const M: usize> RingProducer<T, N, M> {
    pub fn new(blocking: bool) -> Self {
        Self {
            ring: Arc::new(Ring::new()),
            consumers: Vec::new(),
            blocking,
            fill: 0,
        }
    }

    /// Drop consumers that have already exited, so a dead cursor cannot gate
    /// this producer forever.
    fn reap(&mut self) {
        self.consumers.retain(|(h, _)| !h.is_finished());
    }

    /// This is the safety guard for the data race in case of slow consumers:
    /// - If head and consumer both occupy a slot, write is forbidden
    /// - This is different from the seqlock mechanism
    ///   where head & consumer could be in the same slot.
    ///   But that nead 2 atomic read to check for write state.
    ///   This only need one.
    fn gated(&self) -> bool {
        let head = self.ring.head_relaxed();

        // Here the gated condition is h - c >= N
        // this means consumer is likely to loose package
        self.consumers
            .iter()
            .any(|(_, c)| head.saturating_sub(c.load(Ordering::Acquire)) >= N as u64 - 1)
    }

    /// Wake every consumer — including after a refusal, since a consumer
    /// draining a slot is exactly what unblocks the next write.
    fn wake(&self) {
        self.consumers.iter().for_each(|(h, _)| h.thread().unpark());
    }

    /// Write one block.
    ///
    /// # Errors
    ///
    /// [`CustomError::SlowConsumer`] means the write was **refused**: a consumer
    /// is too far behind and blocking mode will not overwrite its slot. The block
    /// is lost, and the caller is the only one who can decide what to do about it
    /// — retry, drop, or widen the ring. Registered consumers are still woken, so
    /// retrying after a pause is reasonable.
    ///
    /// Note this is a refusal, not a wait: the producer never parks. A blocking
    /// ring bounds latency by discarding, it does not apply back-pressure to the
    /// caller. Non-blocking mode never returns this error.
    pub fn write(&mut self, src: &[T]) -> Result<(), CustomError> {
        debug_assert_eq!(
            self.fill, 0,
            "write() would publish a block that push() is still filling"
        );
        self.reap();

        // Non-blocking never consults consumers. A lapped one detects and
        // resyncs itself, which is why the producer no longer writes consumer
        // cursors at all — that used to be a data race against their own store.
        let refused = self.blocking && self.gated();
        if !refused {
            self.ring.write(src)?;
        }

        self.wake();

        if refused {
            return Err(CustomError::SlowConsumer);
        }
        Ok(())
    }

    /// Append one element to the block under construction, publishing it once
    /// `M` elements have accumulated.
    ///
    /// For producers that compute samples one at a time. [`write`](Self::write)
    /// forces such a producer to fill a staging array and hand over a full
    /// block, which costs an extra whole-block copy; `push` writes straight into
    /// the ring slot and skips it. The two must not be interleaved within a
    /// block — `write` debug-asserts that no fill is open.
    ///
    /// Returns `Ok(true)` on the call that published a block, `Ok(false)` while
    /// one is still filling.
    ///
    /// # Errors
    ///
    /// [`CustomError::SlowConsumer`] — blocking mode only, and only ever on the
    /// *first* element of a block, since that is where the slot is committed to.
    /// Note the granularity this implies: a gated `push` producer drops
    /// individual elements rather than whole blocks, so a caller that keeps
    /// pushing through a refusal will splice a discontinuity into the middle of
    /// the next block. Callers that need block-aligned loss should count `M`
    /// refusals themselves, or use `write`.
    pub fn push(&mut self, x: T) -> Result<bool, CustomError> {
        if self.fill == 0 {
            self.reap();
            if self.blocking && self.gated() {
                self.wake();
                return Err(CustomError::SlowConsumer);
            }
        }

        self.ring.put(self.fill, x)?;
        self.fill += 1;

        if self.fill < M {
            return Ok(false);
        }

        self.fill = 0;
        self.ring.publish();
        self.wake();
        Ok(true)
    }

    /// How many elements are sitting in the block [`push`](Self::push) has not
    /// published yet.
    pub fn pending(&self) -> usize {
        self.fill
    }

    pub fn subscribe(&self) -> RingConsumer<T, N, M> {
        RingConsumer::new(self.ring.clone())
    }

    /// Take ownership of a spawned consumer thread so the producer can wake it
    /// (unpark), gate against it, and reap it on shutdown. Get the cursor from
    /// [`RingConsumer::cursor`] *before* moving the consumer into its thread.
    pub fn add_consumer(&mut self, thread: JoinHandle<()>, cursor: Arc<AtomicU64>) {
        self.consumers.push((thread, cursor));
    }

    /// Register a cursor without a real reader, so gating can be driven step by
    /// step from a single thread. The placeholder thread parks forever and is
    /// never finished, so `retain` keeps the entry — tests using this must not
    /// call `join_all`.
    #[cfg(test)]
    fn register_cursor_for_test(&mut self, cursor: Arc<AtomicU64>) {
        self.consumers.push((
            thread::spawn(|| {
                loop {
                    thread::park()
                }
            }),
            cursor,
        ));
    }

    /// Block until every consumer thread has finished.
    pub fn join_all(&mut self) {
        for (h, _) in self.consumers.drain(..) {
            let _ = h.join();
        }
    }
}

/// Consumer for a [`Ring`]. Each consumer owns an independent cursor, so every
/// consumer sees every block (this is a broadcast ring, not a work queue).
pub struct RingConsumer<T: Copy + Default, const N: usize, const M: usize> {
    ring: Arc<Ring<T, N, M>>,
    head: Arc<AtomicU64>,
}

impl<T: Copy + Default, const N: usize, const M: usize> RingConsumer<T, N, M> {
    /// Private: consumers are only ever created through
    /// [`RingProducer::subscribe`], which is what ties them to a ring.
    fn new(ring: Arc<Ring<T, N, M>>) -> Self {
        Self {
            ring,
            head: Arc::new(AtomicU64::new(0)),
        }
    }

    /// This consumer's current sequence number.
    pub fn head(&self) -> u64 {
        self.head.load(Ordering::Relaxed)
    }

    /// A handle to this consumer's cursor, for [`RingProducer::add_consumer`].
    pub fn cursor(&self) -> Arc<AtomicU64> {
        self.head.clone()
    }

    /// Jump to the newest published block, abandoning the backlog.
    ///
    /// For consumers that want freshness over completeness — a waterfall, a
    /// signal meter — where [`claim`](Self::claim)'s mid-ring resync is backlog
    /// they will never work through. A UI reading one block per frame while the
    /// producer publishes ten is lapped every frame regardless; this makes the
    /// block it does read the freshest one rather than one `N / 2` slots old.
    ///
    /// Wrong for anything that must not skip: on a bounded ring this discards
    /// exactly the cushion the ring exists to provide.
    pub fn seek_latest(&self) {
        self.head
            .store(self.ring.head().saturating_sub(1), Ordering::Release);
    }

    /// Pick a sequence to read, resyncing first if the producer has lapped us.
    ///
    /// This is the **pre-check** half of the protocol: on return, `head - seq`
    /// was `< N`, which proves the producer was not inside `seq`'s slot at that
    /// instant. It says nothing about what happens next — that is
    /// [`validate`](Self::validate)'s job.
    fn claim(&self) -> Result<u64, CustomError> {
        // Relaxed: this consumer is the only writer of its own cursor.
        let seq = self.head.load(Ordering::Relaxed);
        let ring_head = self.ring.head();

        if seq >= ring_head {
            return Err(CustomError::SlowProducer);
        };

        if ring_head - seq >= N as u64 {
            // Lapped — our block is gone. Land in the middle of the ring rather
            // than on the head: that leaves N/2 blocks of backlog to work
            // through (the cushion against a subsequent underrun) and N/2 writes
            // of headroom before being lapped again. Landing on `ring_head`
            // instead turns every overrun into a guaranteed underrun; landing on
            // `ring_head - N + 1` leaves us one write from the cliff. Both are
            // correct, this is purely policy.
            //
            // Cannot underflow: this branch requires ring_head >= N.
            let resync = ring_head - (N as u64) / 2;
            self.head.store(resync, Ordering::Release);
            return Ok(resync);
        };

        Ok(seq)
    }

    /// The **post-check**: did the producer reach `seq`'s slot while we were
    /// reading it? Cannot underflow — the ring head only grows, and `claim`
    /// established `seq < ring_head`.
    fn validate(&self, seq: u64) -> bool {
        self.ring.head() - seq < N as u64
    }

    /// Borrow the next block without copying. **Unsynchronised.**
    ///
    /// # Correctness
    ///
    /// The returned slice points into a live ring slot, and this call has
    /// already advanced the cursor — so in non-blocking mode the producer may
    /// overwrite the buffer while the caller is still reading it. The pre-check
    /// means the producer was not in the slot *at the moment of the call*, but
    /// nothing constrains what happens after the reference is handed out, and
    /// there is no way to retract it once the caller has read from it.
    ///
    /// Use [`read_into`](Self::read_into) for a validated copy, or
    /// [`read_guard`](Self::read_guard) for zero-copy under a blocking producer.
    pub fn read(&self) -> Result<&[T], CustomError> {
        let seq = self.claim()?;
        let slot = self.ring.slot(seq as usize & (N - 1))?;
        let output = unsafe { &(*slot.get()) };

        // Release the slot back to the producer immediately — see above.
        self.head.store(seq + 1, Ordering::Release);
        Ok(&output[..])
    }

    /// Copy the next block into `buf`, retrying if the producer laps us mid-copy.
    ///
    /// This is the safe default. Both halves of the protocol are applied: the
    /// pre-check in [`claim`](Self::claim) rejects a slot the producer is
    /// already inside, and the post-check in [`validate`](Self::validate)
    /// discards a copy the producer walked into. A failed post-check leaves the
    /// cursor untouched, so the next `claim` sees a lag of `>= N` and resyncs —
    /// which is what makes the retry loop terminate rather than spin.
    ///
    /// `buf` must be exactly `M` long.
    pub fn read_into(&self, buf: &mut [T]) -> Result<(), CustomError> {
        if buf.len() != M {
            return Err(CustomError::MismatchInputLength);
        }

        for _ in 0..READ_RETRIES {
            let seq = self.claim()?;
            let slot = self.ring.slot(seq as usize & (N - 1))?;

            // Copy first...
            buf.copy_from_slice(unsafe { &(*slot.get()) });

            // ...then confirm nobody wrote over it while we were copying.
            if self.validate(seq) {
                self.head.store(seq + 1, Ordering::Release);
                return Ok(());
            }
            // Torn: throw the copy away and go round again.
        }

        Err(CustomError::SlowConsumer)
    }

    /// Borrow the next block, holding the slot until the guard drops. No copy.
    ///
    /// Takes `&mut self` deliberately: two live guards would both read the same
    /// sequence and both advance the cursor on drop, silently skipping a block.
    /// The exclusive borrow makes that unrepresentable.
    ///
    /// # Back-pressure only exists in blocking mode
    ///
    /// The guard protects the slot by *not* advancing the cursor, which only
    /// helps if the producer consults cursors — i.e. `blocking == true`. Under a
    /// non-blocking producer this is **worse** than [`read_into`](Self::read_into),
    /// because the exposure window grows from a memcpy to however long the
    /// caller holds the guard. Call [`is_valid`](ReadGuard::is_valid) before
    /// trusting the data if you use it there anyway.
    pub fn read_guard(&mut self) -> Result<ReadGuard<'_, T, N, M>, CustomError> {
        let seq = self.claim()?;
        let slot = self.ring.slot(seq as usize & (N - 1))?;

        Ok(ReadGuard {
            slot: unsafe { &(*slot.get()) },
            ring: &self.ring,
            cursor: &self.head,
            seq,
        })
    }

    /// Read blocks into `callback` on a new thread, using [`read_into`].
    /// `timeout` is in seconds; the thread exits if the ring stays empty for
    /// that long.
    pub fn start_reading<F>(self, callback: F, timeout: Option<u64>) -> JoinHandle<()>
    where
        F: Fn(&[T]) + Send + 'static,
        T: Send + 'static,
    {
        thread::spawn(move || {
            let mut staging = [T::default(); M];
            loop {
                match self.read_into(&mut staging) {
                    Ok(()) => callback(&staging),
                    Err(_) => {
                        let Some(x) = timeout else { continue };
                        thread::park_timeout(Duration::from_secs(x));
                        // May still find nothing after the timeout.
                        match self.read_into(&mut staging) {
                            Ok(()) => callback(&staging),
                            Err(_) => break,
                        }
                    }
                }
            }
        })
    }

    /// Like [`start_reading`](Self::start_reading) but hands the callback a
    /// borrow straight into the ring.
    ///
    /// WARNING: uses [`read`](Self::read) — see its correctness note.
    pub fn start_reading_unsafe<F>(self, callback: F, timeout: Option<u64>) -> JoinHandle<()>
    where
        F: Fn(&[T]) + Send + 'static,
        T: Send + 'static,
    {
        thread::spawn(move || {
            loop {
                match self.read() {
                    Ok(buf) => callback(buf),
                    Err(_) => {
                        let Some(x) = timeout else { continue };
                        thread::park_timeout(Duration::from_secs(x));
                        match self.read() {
                            Ok(buf) => callback(buf),
                            Err(_) => break,
                        }
                    }
                }
            }
        })
    }
}

/// Zero-copy read borrow. The slot is released back to the producer when this
/// drops — see [`RingConsumer::read_guard`] for when that is safe.
pub struct ReadGuard<'a, T: Copy + Default, const N: usize, const M: usize> {
    slot: &'a Buffer<T, M>,
    ring: &'a Ring<T, N, M>,
    cursor: &'a AtomicU64,
    seq: u64,
}

impl<T: Copy + Default, const N: usize, const M: usize> ReadGuard<'_, T, N, M> {
    /// The sequence number this guard is holding.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether the producer has stayed out of this slot so far — the same
    /// post-check [`RingConsumer::read_into`] applies, but callable at any point
    /// while the guard is held. Always true under a blocking producer.
    pub fn is_valid(&self) -> bool {
        self.ring.head() - self.seq < N as u64
    }
}

impl<T: Copy + Default, const N: usize, const M: usize> Deref for ReadGuard<'_, T, N, M> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        &self.slot
    }
}

/// The cursor advances only when the guard drops — that delay is the whole
/// point of the type.
impl<T: Copy + Default, const N: usize, const M: usize> Drop for ReadGuard<'_, T, N, M> {
    fn drop(&mut self) {
        self.cursor.store(self.seq + 1, Ordering::Release);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Drive a run of writes, returning how many were refused. Callers that care
    /// about the refusals assert on the count; the rest use it to fill a ring.
    fn write_seq<const N: usize>(
        p: &mut RingProducer<u8, N, 2>,
        range: std::ops::Range<u8>,
    ) -> usize {
        range
            .filter(|i| p.write(&[*i; 2]) == Err(CustomError::SlowConsumer))
            .count()
    }

    // ── Ring mechanics ──────────────────────────────────────────────────────

    /// Regression: `Ring::write` used to store the head back to itself, so the
    /// ring never advanced and no consumer ever saw data.
    #[test]
    fn test_write_advances_head() {
        let mut producer = RingProducer::<u8, 4, 2>::new(false);
        assert_eq!(producer.ring.head_relaxed(), 0);
        producer.write(&[1u8; 2]).unwrap();
        assert_eq!(producer.ring.head_relaxed(), 1);
        producer.write(&[2u8; 2]).unwrap();
        assert_eq!(producer.ring.head_relaxed(), 2);
    }

    /// Regression: the consumer read path used the raw sequence as a slot index
    /// instead of `seq & (N - 1)`, so every read past the Nth returned
    /// `InvalidIndex`.
    #[test]
    fn test_reads_past_capacity_wrap_correctly() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        let mut dst = [0u8; 2];
        // Ten laps' worth, one at a time, so the consumer never falls behind.
        for i in 0..40u8 {
            producer.write(&[i; 2]).unwrap();
            consumer
                .read_into(&mut dst)
                .expect("wrapped index must stay valid");
            assert_eq!(dst, [i, i]);
        }
        assert_eq!(consumer.head(), 40);
    }

    // ── Blocking mode ───────────────────────────────────────────────────────

    /// Regression: the gate was `(head - c) % N == 0`, which is true when the
    /// consumer is fully caught up (`head - c == 0`) — so an *empty* ring
    /// refused every write.
    #[test]
    fn test_blocking_empty_ring_does_not_gate() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        producer.write(&[1u8; 2]).unwrap();
        assert_eq!(
            producer.ring.head_relaxed(),
            1,
            "empty ring must accept a write"
        );

        let mut dst = [0u8; 2];
        consumer.read_into(&mut dst).unwrap();
        assert_eq!(consumer.head(), 1, "consumer now caught up");

        producer.write(&[2u8; 2]).unwrap();
        assert_eq!(
            producer.ring.head_relaxed(),
            2,
            "caught-up consumer must not gate"
        );
    }

    /// The producer refuses to overwrite a slot the slowest consumer has not
    /// read, stalling at the N-1 guard band and staying there.
    #[test]
    fn test_blocking_gates_at_capacity() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        assert_eq!(
            write_seq(&mut producer, 0..4),
            1,
            "the 4th write is refused"
        );
        assert_eq!(
            producer.ring.head_relaxed(),
            3,
            "N-1 blocks fit, then it gates"
        );

        // Consumer never reads, so every further write is refused — and says so.
        for i in 4..8u8 {
            assert_eq!(
                producer.write(&[i; 2]),
                Err(CustomError::SlowConsumer),
                "write {i} must report the refusal, not swallow it"
            );
            assert_eq!(producer.ring.head_relaxed(), 3, "write {i} must be gated");
        }
        assert_eq!(
            consumer.head(),
            0,
            "blocking mode never moves a consumer cursor"
        );
    }

    /// Draining one slot releases exactly one write, then it gates again.
    #[test]
    fn test_blocking_resumes_after_drain() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        write_seq(&mut producer, 0..4);
        assert_eq!(producer.ring.head_relaxed(), 3, "full");

        // Blocking mode preserves order: the oldest entry is still there, and
        // the guard band keeps it readable rather than triggering a resync.
        let mut dst = [0u8; 2];
        consumer.read_into(&mut dst).unwrap();
        assert_eq!(dst, [0, 0], "oldest block survives");

        producer
            .write(&[99; 2])
            .expect("one slot freed buys one write");
        assert_eq!(producer.ring.head_relaxed(), 4);

        assert_eq!(
            producer.write(&[98; 2]),
            Err(CustomError::SlowConsumer),
            "gated again"
        );
        assert_eq!(producer.ring.head_relaxed(), 4);
    }

    /// A blocking producer never laps anyone, so no consumer ever resyncs and
    /// every block is delivered in order.
    #[test]
    fn test_blocking_loses_nothing() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        let mut dst = [0u8; 2];
        let mut got = Vec::new();
        for i in 0..20u8 {
            producer.write(&[i; 2]).unwrap();
            while consumer.read_into(&mut dst).is_ok() {
                got.push(dst[0]);
            }
        }
        assert_eq!(got, (0..20u8).collect::<Vec<_>>(), "no gaps, no reordering");
    }

    // ── Non-blocking mode ───────────────────────────────────────────────────

    /// The producer never stalls, and it no longer touches consumer cursors —
    /// the lapped consumer is left behind until it notices for itself.
    #[test]
    fn test_non_blocking_never_stalls() {
        let mut producer = RingProducer::<u8, 4, 2>::new(false);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        for i in 0..12u8 {
            let before = producer.ring.head_relaxed();
            producer.write(&[i; 2]).unwrap();
            assert_eq!(
                producer.ring.head_relaxed(),
                before + 1,
                "write {i} must land"
            );
        }
        assert_eq!(producer.ring.head_relaxed(), 12, "three full laps");
        assert_eq!(
            consumer.head(),
            0,
            "producer must not write the consumer's cursor — that was the data race"
        );
    }

    /// A lapped consumer resyncs itself to the middle of the ring on its next
    /// read, giving it N/2 blocks of backlog rather than landing on the head
    /// with nothing to read.
    #[test]
    fn test_non_blocking_lapped_consumer_resyncs_to_midpoint() {
        let mut producer = RingProducer::<u8, 8, 2>::new(false);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        // 20 writes into an 8-slot ring: the consumer at 0 is far behind.
        for i in 0..20u8 {
            producer.write(&[i; 2]).unwrap();
        }
        assert_eq!(producer.ring.head_relaxed(), 20);

        let mut dst = [0u8; 2];
        consumer.read_into(&mut dst).unwrap();

        // Resync target is head - N/2 = 20 - 4 = 16, so it reads block 16 and
        // ends at 17 — not block 0 (gone) and not block 20 (not written yet).
        assert_eq!(dst, [16, 16], "reads the midpoint block");
        assert_eq!(consumer.head(), 17);

        // And it still has backlog to work through rather than being starved.
        for expected in 17..20u8 {
            consumer.read_into(&mut dst).unwrap();
            assert_eq!(dst, [expected, expected]);
        }
        assert_eq!(
            consumer.read_into(&mut dst),
            Err(CustomError::SlowProducer),
            "backlog drained"
        );
    }

    /// Being lapped costs data — that is the trade non-blocking mode makes —
    /// but what survives is contiguous and correct, never torn.
    #[test]
    fn test_non_blocking_loses_a_prefix_but_not_correctness() {
        let mut producer = RingProducer::<u8, 4, 2>::new(false);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        for i in 0..10u8 {
            producer.write(&[i; 2]).unwrap();
        }

        let mut dst = [0u8; 2];
        let mut got = Vec::new();
        while consumer.read_into(&mut dst).is_ok() {
            assert_eq!(dst[0], dst[1], "block must never be torn");
            got.push(dst[0]);
        }

        assert!(!got.is_empty(), "something survived");
        assert!(got[0] > 0, "the oldest blocks were legitimately lost");
        // Whatever we got is a contiguous ascending run.
        for w in got.windows(2) {
            assert_eq!(w[1], w[0] + 1, "surviving blocks are contiguous: {got:?}");
        }
    }

    // ── push ────────────────────────────────────────────────────────────────

    /// A block becomes visible on the push that completes it, and not before —
    /// a half-filled slot must stay invisible to consumers.
    #[test]
    fn test_push_publishes_only_on_the_last_element() {
        let mut producer = RingProducer::<u8, 4, 4>::new(false);
        let consumer = producer.subscribe();

        let mut dst = [0u8; 4];
        for (i, x) in [10u8, 11, 12].into_iter().enumerate() {
            assert_eq!(producer.push(x), Ok(false), "element {i} does not publish");
            assert_eq!(producer.ring.head_relaxed(), 0, "head pinned mid-block");
            assert_eq!(producer.pending(), i + 1);
            assert_eq!(
                consumer.read_into(&mut dst),
                Err(CustomError::SlowProducer),
                "a partial block must not be readable"
            );
        }

        assert_eq!(producer.push(13), Ok(true), "the Mth element publishes");
        assert_eq!(producer.ring.head_relaxed(), 1);
        assert_eq!(producer.pending(), 0, "fill resets for the next block");

        consumer.read_into(&mut dst).unwrap();
        assert_eq!(dst, [10, 11, 12, 13], "elements land in push order");
    }

    /// `push` is a drop-in for `write`: same blocks, same sequence, no staging
    /// array and no whole-block copy.
    #[test]
    fn test_push_matches_write_across_laps() {
        let mut pushed = RingProducer::<u8, 4, 4>::new(false);
        let mut written = RingProducer::<u8, 4, 4>::new(false);
        let (cp, cw) = (pushed.subscribe(), written.subscribe());

        // Three laps of a 4-slot ring, so the slot index wraps repeatedly.
        for b in 0..12u8 {
            let block = [b * 4, b * 4 + 1, b * 4 + 2, b * 4 + 3];
            for x in block {
                pushed.push(x).unwrap();
            }
            written.write(&block).unwrap();

            let (mut a, mut c) = ([0u8; 4], [0u8; 4]);
            cp.read_into(&mut a).unwrap();
            cw.read_into(&mut c).unwrap();
            assert_eq!(a, c, "block {b}");
            assert_eq!(a, block);
        }
        assert_eq!(pushed.ring.head_relaxed(), written.ring.head_relaxed());
    }

    /// Blocking mode gates `push` at the block boundary only. Once a slot is
    /// committed to, the remaining elements always land — a block can never be
    /// left half-written by the gate.
    #[test]
    fn test_push_blocking_gates_at_block_boundaries_only() {
        let mut producer = RingProducer::<u8, 4, 4>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        // Fill the guard band: N-1 = 3 blocks of M = 4 elements.
        for x in 0..12u8 {
            producer.push(x).unwrap();
        }
        assert_eq!(producer.ring.head_relaxed(), 3, "at the guard band");

        // The next block cannot start...
        assert_eq!(producer.push(99), Err(CustomError::SlowConsumer));
        assert_eq!(producer.pending(), 0, "a refused push opens no slot");

        // ...until a consumer drains one.
        let mut dst = [0u8; 4];
        consumer.read_into(&mut dst).unwrap();
        assert_eq!(dst, [0, 1, 2, 3]);

        // Now the block starts, and mid-block pushes are never gated even
        // though the consumer stops reading again.
        assert_eq!(producer.push(90), Ok(false));
        for x in [91u8, 92] {
            assert_eq!(producer.push(x), Ok(false), "mid-block push must not gate");
        }
        assert_eq!(producer.push(93), Ok(true), "the block completes");
        assert_eq!(producer.ring.head_relaxed(), 4);

        for expected in [[4u8, 5, 6, 7], [8, 9, 10, 11], [90, 91, 92, 93]] {
            consumer.read_into(&mut dst).unwrap();
            assert_eq!(dst, expected);
        }
    }

    /// Non-blocking `push` never refuses, exactly like `write`.
    #[test]
    fn test_push_non_blocking_never_refuses() {
        let mut producer = RingProducer::<u8, 4, 2>::new(false);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        // Five laps with a consumer that never reads.
        for x in 0..40u8 {
            assert!(producer.push(x).is_ok(), "push {x} must not be refused");
        }
        assert_eq!(producer.ring.head_relaxed(), 20);
    }

    /// The point of the API: a pushed block is not torn by a saturated producer
    /// any more than a written one is, because holding a slot open does not
    /// widen the window — `head` still advances in a single instant.
    #[test]
    fn test_push_does_not_tear_under_saturation() {
        const N: usize = 8;
        const M: usize = 256;
        const BLOCKS: u32 = 20_000;

        let mut producer = RingProducer::<u32, N, M>::new(false);
        let consumer = producer.subscribe();
        let cursor = consumer.cursor();

        let reader = thread::spawn(move || {
            let mut staging = [0u32; M];
            let (mut torn, mut got) = (0u64, 0u64);
            let mut idle = 0u32;
            loop {
                match consumer.read_into(&mut staging) {
                    Ok(()) => {
                        idle = 0;
                        got += 1;
                        let first = staging[0];
                        if staging.iter().any(|&x| x != first) {
                            torn += 1;
                        }
                    }
                    Err(_) => {
                        idle += 1;
                        if idle > 2_000_000 {
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }
            (got, torn)
        });
        producer.add_consumer(
            thread::spawn(|| {
                loop {
                    thread::park()
                }
            }),
            cursor,
        );

        // Each block is filled element-by-element with its own sequence number,
        // so a tear shows up as elements that disagree.
        for b in 0..BLOCKS {
            for _ in 0..M {
                producer.push(b).unwrap();
            }
        }

        let (got, torn) = reader.join().unwrap();
        assert!(got > 0, "consumer received nothing — harness is broken");
        assert_eq!(torn, 0, "{torn} torn blocks out of {got} received");
        println!("push saturation: {got} received of {BLOCKS} written, {torn} torn");
    }

    // ── read_into ───────────────────────────────────────────────────────────

    #[test]
    fn test_read_into_copies_and_advances() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        producer.write(&[7u8; 2]).unwrap();

        let mut dst = [0u8; 2];
        consumer.read_into(&mut dst).unwrap();
        assert_eq!(dst, [7, 7], "block copied out");
        assert_eq!(consumer.head(), 1, "cursor advanced");

        // Ring drained: nothing more to take.
        assert_eq!(consumer.read_into(&mut dst), Err(CustomError::SlowProducer));
        assert_eq!(consumer.head(), 1, "failed read must not advance");
    }

    #[test]
    fn test_read_into_rejects_wrong_length() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());
        producer.write(&[1u8; 2]).unwrap();

        let mut too_small = [0u8; 1];
        assert_eq!(
            consumer.read_into(&mut too_small),
            Err(CustomError::MismatchInputLength)
        );
        let mut too_big = [0u8; 3];
        assert_eq!(
            consumer.read_into(&mut too_big),
            Err(CustomError::MismatchInputLength)
        );

        // A rejected length must not consume the block.
        let mut dst = [0u8; 2];
        consumer.read_into(&mut dst).unwrap();
        assert_eq!(dst, [1, 1]);
    }

    // ── ReadGuard ───────────────────────────────────────────────────────────

    #[test]
    fn test_read_guard_derefs_to_block() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let mut consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());
        producer.write(&[5u8; 2]).unwrap();

        let guard = consumer.read_guard().unwrap();
        assert_eq!(&guard[..], &[5u8, 5u8], "Deref gives the block");
        assert_eq!(guard.seq(), 0);
        assert!(guard.is_valid());
    }

    /// The cursor moves on drop, not on acquisition — that delay is what makes
    /// the borrow safe under a blocking producer.
    #[test]
    fn test_read_guard_advances_only_on_drop() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let mut consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());
        producer.write(&[5u8; 2]).unwrap();

        let cursor = consumer.cursor();
        {
            let _guard = consumer.read_guard().unwrap();
            assert_eq!(cursor.load(Ordering::Relaxed), 0, "held: cursor pinned");
        }
        assert_eq!(
            cursor.load(Ordering::Relaxed),
            1,
            "dropped: cursor released"
        );
    }

    /// A held guard gates a blocking producer at the capacity edge, and
    /// dropping it lets exactly one more write through. This is the
    /// back-pressure the type exists to provide.
    #[test]
    fn test_read_guard_applies_back_pressure() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let mut consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        write_seq(&mut producer, 0..4);
        assert_eq!(producer.ring.head_relaxed(), 3, "ring full");

        {
            let guard = consumer.read_guard().unwrap();
            assert_eq!(&guard[..], &[0u8, 0u8]);

            assert_eq!(
                producer.write(&[42; 2]),
                Err(CustomError::SlowConsumer),
                "producer must not overwrite the slot under the guard"
            );
            assert_eq!(producer.ring.head_relaxed(), 3);
            assert!(guard.is_valid(), "and so the block stays intact");
            assert_eq!(&guard[..], &[0u8, 0u8]);
        }

        producer.write(&[42; 2]).expect("released: one write lands");
        assert_eq!(producer.ring.head_relaxed(), 4);
    }

    /// A refusal is reported, loses exactly the block it refused, and leaves the
    /// ring usable: the same block written again after a drain lands intact.
    #[test]
    fn test_refused_write_is_reported_and_retryable() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        write_seq(&mut producer, 0..3);
        assert_eq!(producer.ring.head_relaxed(), 3, "at the guard band");

        assert_eq!(
            producer.write(&[42; 2]),
            Err(CustomError::SlowConsumer),
            "caller is told the block did not land"
        );

        // Drain one slot, then retry the very block that was refused.
        let mut dst = [0u8; 2];
        consumer.read_into(&mut dst).unwrap();
        assert_eq!(dst, [0, 0]);
        producer
            .write(&[42; 2])
            .expect("retry after a drain succeeds");

        // Nothing was corrupted by the refusal: 1, 2, then the retried 42.
        for expected in [1u8, 2, 42] {
            consumer.read_into(&mut dst).unwrap();
            assert_eq!(dst, [expected, expected]);
        }
        assert_eq!(consumer.read_into(&mut dst), Err(CustomError::SlowProducer));
    }

    /// Non-blocking mode never refuses — it overwrites instead, so the caller
    /// gets `Ok(())` for every write no matter how far behind consumers are.
    #[test]
    fn test_non_blocking_never_returns_slow_consumer() {
        let mut producer = RingProducer::<u8, 4, 2>::new(false);
        let consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());

        // Consumer never reads; the producer laps it five times regardless.
        assert_eq!(
            write_seq(&mut producer, 0..20),
            0,
            "no write may be refused"
        );
        assert_eq!(producer.ring.head_relaxed(), 20);
    }

    #[test]
    fn test_read_guard_empty_ring() {
        let mut producer = RingProducer::<u8, 4, 2>::new(true);
        let mut consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());
        assert!(matches!(
            consumer.read_guard(),
            Err(CustomError::SlowProducer)
        ));
    }

    /// Under a non-blocking producer the guard has no protection: `is_valid`
    /// reports the tear that the type cannot prevent. Documents why
    /// `read_guard` is the wrong tool for the non-blocking audio path.
    #[test]
    fn test_read_guard_detects_overwrite_in_non_blocking_mode() {
        let mut producer = RingProducer::<u8, 4, 2>::new(false);
        let mut consumer = producer.subscribe();
        producer.register_cursor_for_test(consumer.cursor());
        producer.write(&[1u8; 2]).unwrap();

        let guard = consumer.read_guard().unwrap();
        assert!(guard.is_valid(), "intact so far");

        // Producer laps the ring while the guard is held.
        write_seq(&mut producer, 10..14);
        assert!(
            !guard.is_valid(),
            "guard must report that its slot was reused"
        );
    }

    // ── Threaded ────────────────────────────────────────────────────────────

    /// The post-check under real contention: a saturated non-blocking producer
    /// against a consumer that is deliberately slow. Blocks are filled with
    /// their own sequence byte, so any tear shows up as a block whose elements
    /// disagree. Losses are expected and fine; tears are not.
    #[test]
    fn test_no_tearing_under_saturation() {
        const N: usize = 8;
        const M: usize = 256;
        const WRITES: u64 = 60_000;

        let mut producer = RingProducer::<u32, N, M>::new(false);
        let consumer = producer.subscribe();
        let cursor = consumer.cursor();

        let reader = thread::spawn(move || {
            let mut staging = [0u32; M];
            let (mut torn, mut got) = (0u64, 0u64);
            let mut idle = 0u32;
            loop {
                match consumer.read_into(&mut staging) {
                    Ok(()) => {
                        idle = 0;
                        got += 1;
                        let first = staging[0];
                        if staging.iter().any(|&x| x != first) {
                            torn += 1;
                        }
                    }
                    Err(_) => {
                        idle += 1;
                        // Give up once the producer has clearly stopped.
                        if idle > 2_000_000 {
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }
            (got, torn)
        });
        producer.add_consumer(
            thread::spawn(|| {
                loop {
                    thread::park()
                }
            }),
            cursor,
        );

        for i in 0..WRITES {
            producer.write(&[i as u32; M]).unwrap();
        }

        let (got, torn) = reader.join().unwrap();
        assert!(got > 0, "consumer received nothing — harness is broken");
        assert_eq!(torn, 0, "{torn} torn blocks out of {got} received");
        println!("saturation: {got} received of {WRITES} written, {torn} torn");
    }

    /// End-to-end smoke test through the threaded reader helper.
    #[test]
    fn test_ring_blocking_threaded() {
        let mut producer = RingProducer::<u8, 8, 8>::new(true);

        let consumer = producer.subscribe();
        let cursor = consumer.cursor();
        let handle =
            consumer.start_reading(|buf: &[u8]| println!("Consumer receives: {buf:?}"), Some(1));
        producer.add_consumer(handle, cursor);

        for i in 0..8u8 {
            thread::sleep(Duration::from_millis(50));
            // Tolerate a refusal: the reader has 50 ms per block and a 7-slot
            // guard band, so this should not fire, but it is a scheduling
            // question rather than an invariant.
            let _ = producer.write(&[i; 8]);
        }

        // No more writes: the consumer parks up to `timeout`, finds the ring
        // still empty, breaks its loop, and terminates — so join_all returns.
        producer.join_all();
    }
}
