//! Lock-free SPMC ring vs. mutex-based queues, at this application's geometry.
//!
//! Run with `cargo bench --bench ring_vs_mutex`.
//!
//! ## What this is actually asking
//!
//! `spmc::Ring` exists to keep the SDR's USB callback from ever blocking. That
//! is a *trade*, not a free win: the lock-free ring holds producer latency flat
//! by **dropping data**, while a mutex holds the data by **blocking the
//! producer**. Timing alone would let the ring win by doing less work, so every
//! latency figure below is printed next to `written / recv / lost / torn`.
//!
//! Each op moves BLOCK*4 = 2 KB. A memcpy that size (~100 ns) dominates the
//! synchronisation primitive (~20 ns for atomics, ~20-50 ns for an uncontended
//! mutex), so expect S1 to look near-identical everywhere. The interesting
//! result is S4.

use std::collections::VecDeque;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rust_radio::spmc::{RingConsumer, RingProducer};

/// Mirrors RING_BLOCK / RING_SLOTS in `main.rs`.
const BLOCK: usize = 512;
const SLOTS: usize = 16;

/// One block of 512 f32 at 48 kHz.
const BLOCK_PERIOD: Duration = Duration::from_nanos(10_666_667);

// ── Traits ──────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Wrote {
    Ok,
    Dropped,
}

trait Prod {
    fn write(&mut self, src: &[f32; BLOCK]) -> Wrote;
    /// The ring owns its consumer threads via `add_consumer`, so it joins them.
    fn shutdown(&mut self) {}
}

trait Cons: Send {
    /// Copies a block into `dst`. Copy-out, not a borrow — every implementation
    /// pays the same memcpy so the comparison stays honest.
    fn read(&mut self, dst: &mut [f32; BLOCK]) -> bool;
}

// ── Counters ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Counters {
    received: AtomicU64,
    lost: AtomicU64,
    torn: AtomicU64,
    /// Bumped after warmup. Consumers reset their sequence baseline when it
    /// changes, so the seq discontinuity between the warmup and measured phases
    /// is not miscounted as loss. A generation counter rather than a flag so it
    /// works with N consumers.
    resync_gen: AtomicU64,
}

/// Each block is filled entirely with its sequence number, so the consumer can
/// check two things at once: continuity (loss) and element-uniformity (tearing).
/// f32 represents integers exactly to 2^24, far beyond any run length here.
fn fill(block: &mut [f32; BLOCK], seq: u64) {
    block.fill(seq as f32);
}

fn verify(dst: &[f32; BLOCK], last: &mut f64, c: &Counters) {
    let seq = dst[0];

    // Tearing: a partially-overwritten slot has mixed values.
    if dst.iter().any(|&x| x != seq) {
        c.torn.fetch_add(1, Ordering::Relaxed);
    }

    // Loss: sequence should advance by exactly one.
    let seq = seq as f64;
    if *last >= 0.0 && seq > *last {
        let gap = seq - *last - 1.0;
        if gap > 0.0 {
            c.lost.fetch_add(gap as u64, Ordering::Relaxed);
        }
    }
    *last = seq;
    c.received.fetch_add(1, Ordering::Relaxed);
}

// ── Implementation 1/2: the lock-free ring ──────────────────────────────────

struct RingProd {
    inner: RingProducer<f32, SLOTS, BLOCK>,
}

impl Prod for RingProd {
    fn write(&mut self, src: &[f32; BLOCK]) -> Wrote {
        // A gated write now reports `SlowConsumer`, so the ring's `written`
        // column is comparable with the other bounded implementations. Consumer-
        // side sequence gaps remain the authority on total loss, since
        // non-blocking mode overwrites without ever refusing.
        match self.inner.write(src) {
            Ok(()) => Wrote::Ok,
            Err(_) => Wrote::Dropped,
        }
    }

    fn shutdown(&mut self) {
        self.inner.join_all();
    }
}

struct RingCons {
    inner: RingConsumer<f32, SLOTS, BLOCK>,
}

impl Cons for RingCons {
    fn read(&mut self, dst: &mut [f32; BLOCK]) -> bool {
        // `read_into` runs the full option-B protocol: pre-check, copy,
        // post-check, retry on a lapped copy. Swapping this for `read()` +
        // `copy_from_slice` reintroduces the tearing this benchmark originally
        // found (S3 showed torn=176 in blocking mode).
        self.inner.read_into(dst).is_ok()
    }
}

/// Same ring, read through `read_guard` instead of `read_into`.
///
/// The copy here is *not* inherent to the guard — the guard is zero-copy, and a
/// real consumer would work straight off the `Deref`. It is copied anyway so
/// this row differs from `RingCons` by exactly the guard machinery (RAII
/// release, `is_valid`) and nothing else. Any zero-copy win is on top of
/// whatever this row shows.
struct RingGuardCons {
    inner: RingConsumer<f32, SLOTS, BLOCK>,
}

impl Cons for RingGuardCons {
    fn read(&mut self, dst: &mut [f32; BLOCK]) -> bool {
        match self.inner.read_guard() {
            Ok(g) => {
                dst.copy_from_slice(&g);
                true
            }
            Err(_) => false,
        }
    }
}

// ── Implementation 3: same ring semantics, but behind a Mutex ───────────────

struct MRing {
    slots: Box<[[f32; BLOCK]; SLOTS]>,
    head: u64, // next seq to write
    tail: u64, // next seq to read
}

#[derive(Clone)]
struct MutexRing(Arc<Mutex<MRing>>);

impl MutexRing {
    fn new() -> Self {
        MutexRing(Arc::new(Mutex::new(MRing {
            slots: Box::new([[0.0; BLOCK]; SLOTS]),
            head: 0,
            tail: 0,
        })))
    }
}

impl Prod for MutexRing {
    fn write(&mut self, src: &[f32; BLOCK]) -> Wrote {
        let mut g = self.0.lock().unwrap();
        if g.head - g.tail == SLOTS as u64 {
            return Wrote::Dropped; // bounded, same refusal as the blocking ring
        }
        let i = (g.head % SLOTS as u64) as usize;
        g.slots[i].copy_from_slice(src);
        g.head += 1;
        Wrote::Ok
    }
}

impl Cons for MutexRing {
    fn read(&mut self, dst: &mut [f32; BLOCK]) -> bool {
        let mut g = self.0.lock().unwrap();
        if g.head == g.tail {
            return false;
        }
        let i = (g.tail % SLOTS as u64) as usize;
        dst.copy_from_slice(&g.slots[i]);
        g.tail += 1;
        true
    }
}

// ── Implementation 4: Mutex + Condvar — blocks instead of dropping ──────────
//
// This is the one that makes S4 meaningful. Every other baseline fails fast when
// full, so none of them ever pay the "producer waits for a descheduled consumer"
// cost that the lock-free ring exists to avoid. This one does: it is the
// "never lose a sample" policy, and its producer latency is the price.

struct CondRing {
    m: Mutex<MRing>,
    cv: Condvar,
}

#[derive(Clone)]
struct MutexCondRing(Arc<CondRing>);

impl MutexCondRing {
    fn new() -> Self {
        MutexCondRing(Arc::new(CondRing {
            m: Mutex::new(MRing {
                slots: Box::new([[0.0; BLOCK]; SLOTS]),
                head: 0,
                tail: 0,
            }),
            cv: Condvar::new(),
        }))
    }
}

impl Prod for MutexCondRing {
    fn write(&mut self, src: &[f32; BLOCK]) -> Wrote {
        let mut g = self.0.m.lock().unwrap();
        // Bounded wait, so a stopped consumer cannot deadlock the harness at
        // shutdown. 100 ms comfortably exceeds S4's 20 ms stall, so a timeout
        // here means the consumer really is gone, not merely slow.
        let mut waited = Duration::ZERO;
        while g.head - g.tail == SLOTS as u64 {
            let (ng, r) = self
                .0
                .cv
                .wait_timeout(g, Duration::from_millis(100))
                .unwrap();
            g = ng;
            if r.timed_out() {
                waited += Duration::from_millis(100);
                if waited >= Duration::from_millis(300) {
                    return Wrote::Dropped;
                }
            }
        }
        let i = (g.head % SLOTS as u64) as usize;
        g.slots[i].copy_from_slice(src);
        g.head += 1;
        drop(g);
        self.0.cv.notify_all();
        Wrote::Ok
    }
}

impl Cons for MutexCondRing {
    fn read(&mut self, dst: &mut [f32; BLOCK]) -> bool {
        let mut g = self.0.m.lock().unwrap();
        if g.head == g.tail {
            return false;
        }
        let i = (g.tail % SLOTS as u64) as usize;
        dst.copy_from_slice(&g.slots[i]);
        g.tail += 1;
        drop(g);
        self.0.cv.notify_all();
        true
    }
}

// ── Implementation 5: Mutex<VecDeque<f32>>, what this design replaced ───────

const DEQUE_CAP: usize = SLOTS * BLOCK;

#[derive(Clone)]
struct MutexDeque(Arc<Mutex<VecDeque<f32>>>);

impl MutexDeque {
    fn new() -> Self {
        MutexDeque(Arc::new(Mutex::new(VecDeque::with_capacity(DEQUE_CAP))))
    }
}

impl Prod for MutexDeque {
    fn write(&mut self, src: &[f32; BLOCK]) -> Wrote {
        let mut g = self.0.lock().unwrap();
        if g.len() + BLOCK > DEQUE_CAP {
            return Wrote::Dropped;
        }
        g.extend(src.iter().copied());
        Wrote::Ok
    }
}

impl Cons for MutexDeque {
    fn read(&mut self, dst: &mut [f32; BLOCK]) -> bool {
        let mut g = self.0.lock().unwrap();
        if g.len() < BLOCK {
            return false;
        }
        for slot in dst.iter_mut() {
            *slot = g.pop_front().unwrap();
        }
        true
    }
}

// ── Implementation 5: std mpsc, as a familiar yardstick ─────────────────────

struct MpscProd(SyncSender<[f32; BLOCK]>);

impl Prod for MpscProd {
    fn write(&mut self, src: &[f32; BLOCK]) -> Wrote {
        // By value, not Box/Vec — a heap allocation per send would turn this
        // into an allocator benchmark.
        match self.0.try_send(*src) {
            Ok(()) => Wrote::Ok,
            Err(TrySendError::Full(_)) => Wrote::Dropped,
            Err(TrySendError::Disconnected(_)) => Wrote::Dropped,
        }
    }
}

struct MpscCons(Receiver<[f32; BLOCK]>);

impl Cons for MpscCons {
    fn read(&mut self, dst: &mut [f32; BLOCK]) -> bool {
        match self.0.try_recv() {
            Ok(b) => {
                dst.copy_from_slice(&b);
                true
            }
            Err(_) => false,
        }
    }
}

// ── Consumer thread ─────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Stall {
    every: u64,
    min_us: u64,
    max_us: u64,
}

/// Deterministic PRNG — no dev-dependency, and reproducible between runs.
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn spawn_consumer(
    mut cons: Box<dyn Cons>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    stall: Option<Stall>,
    seed: u64,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut dst = [0.0f32; BLOCK];
        let mut last = -1.0f64;
        let mut n = 0u64;
        let mut rng = seed | 1;
        let mut seen_gen = counters.resync_gen.load(Ordering::Relaxed);

        // Deliberately does not park: the producer stops writing at the end of a
        // run, and a parked consumer would never be unparked to see `stop`.
        while !stop.load(Ordering::Relaxed) {
            if cons.read(&mut dst) {
                let g = counters.resync_gen.load(Ordering::Relaxed);
                if g != seen_gen {
                    seen_gen = g;
                    last = -1.0; // adopt this block as the new baseline
                }
                verify(black_box(&dst), &mut last, &counters);
                n += 1;
                if let Some(s) = stall {
                    if n % s.every == 0 {
                        let span = s.max_us - s.min_us;
                        let us = s.min_us + xorshift(&mut rng) % span.max(1);
                        thread::sleep(Duration::from_micros(us));
                    }
                }
            } else {
                thread::yield_now();
            }
        }
    })
}

// ── Rigs ────────────────────────────────────────────────────────────────────

struct Rig {
    prod: Box<dyn Prod>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Rig {
    fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.prod.shutdown();
        for t in self.threads {
            let _ = t.join();
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Impl {
    RingNonBlocking,
    RingBlocking,
    RingGuard,
    MutexRing,
    MutexCondRing,
    MutexDeque,
    Mpsc,
}

impl Impl {
    fn name(self) -> &'static str {
        match self {
            Impl::RingNonBlocking => "spmc ring (non-blocking)",
            Impl::RingBlocking => "spmc ring (blocking)",
            Impl::RingGuard => "spmc ring (blocking, guard)",
            Impl::MutexRing => "Mutex<ring> (drops)",
            Impl::MutexCondRing => "Mutex+Condvar (blocks)",
            Impl::MutexDeque => "Mutex<VecDeque<f32>>",
            Impl::Mpsc => "mpsc::sync_channel",
        }
    }

    fn is_ring(self) -> bool {
        matches!(
            self,
            Impl::RingNonBlocking | Impl::RingBlocking | Impl::RingGuard
        )
    }

    /// `read_guard` holds the slot until drop, which only creates back-pressure
    /// under a blocking producer.
    fn ring_blocks(self) -> bool {
        matches!(self, Impl::RingBlocking | Impl::RingGuard)
    }

    /// Only the ring is genuinely multi-consumer: mpsc's Receiver cannot be
    /// cloned and the mutex queues pop destructively.
    fn supports_multi_consumer(self) -> bool {
        self.is_ring()
    }
}

fn build(imp: Impl, n_consumers: usize, stall: Option<Stall>) -> Rig {
    let counters = Arc::new(Counters::default());
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();

    let prod: Box<dyn Prod> = match imp {
        imp if imp.is_ring() => {
            let mut p = RingProducer::<f32, SLOTS, BLOCK>::new(imp.ring_blocks());
            for i in 0..n_consumers {
                let consumer = p.subscribe();
                let cursor: Arc<AtomicU64> = consumer.cursor();
                let cons: Box<dyn Cons> = if imp == Impl::RingGuard {
                    Box::new(RingGuardCons { inner: consumer })
                } else {
                    Box::new(RingCons { inner: consumer })
                };
                let h = spawn_consumer(
                    cons,
                    counters.clone(),
                    stop.clone(),
                    stall,
                    0x9E3779B97F4A7C15u64.wrapping_mul(i as u64 + 1),
                );
                // Registering is what lets the blocking gate see this consumer.
                // A non-blocking producer ignores the list except for unparking:
                // lapped consumers now resync themselves.
                p.add_consumer(h, cursor);
            }
            Box::new(RingProd { inner: p })
        }
        Impl::MutexRing => {
            let r = MutexRing::new();
            for i in 0..n_consumers {
                threads.push(spawn_consumer(
                    Box::new(r.clone()),
                    counters.clone(),
                    stop.clone(),
                    stall,
                    i as u64 + 1,
                ));
            }
            Box::new(r)
        }
        Impl::MutexCondRing => {
            let r = MutexCondRing::new();
            for i in 0..n_consumers {
                threads.push(spawn_consumer(
                    Box::new(r.clone()),
                    counters.clone(),
                    stop.clone(),
                    stall,
                    i as u64 + 1,
                ));
            }
            Box::new(r)
        }
        Impl::MutexDeque => {
            let d = MutexDeque::new();
            for i in 0..n_consumers {
                threads.push(spawn_consumer(
                    Box::new(d.clone()),
                    counters.clone(),
                    stop.clone(),
                    stall,
                    i as u64 + 1,
                ));
            }
            Box::new(d)
        }
        Impl::Mpsc => {
            let (tx, rx) = sync_channel::<[f32; BLOCK]>(SLOTS);
            threads.push(spawn_consumer(
                Box::new(MpscCons(rx)),
                counters.clone(),
                stop.clone(),
                stall,
                1,
            ));
            Box::new(MpscProd(tx))
        }
        // Unreachable: the ring variants are all taken by the `is_ring()` guard
        // above, which the exhaustiveness checker cannot see through.
        _ => unreachable!("ring variants handled by the is_ring() arm"),
    };

    Rig {
        prod,
        counters,
        stop,
        threads,
    }
}

// ── Stats ───────────────────────────────────────────────────────────────────

struct Summary {
    mean: u64,
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

fn percentiles(lat: &mut Vec<u64>) -> Summary {
    if lat.is_empty() {
        return Summary { mean: 0, p50: 0, p99: 0, p999: 0, max: 0 };
    }
    lat.sort_unstable();
    let at = |q: f64| lat[((lat.len() - 1) as f64 * q) as usize];
    Summary {
        mean: (lat.iter().sum::<u64>() / lat.len() as u64),
        p50: at(0.50),
        p99: at(0.99),
        p999: at(0.999),
        max: *lat.last().unwrap(),
    }
}

fn timer_overhead() -> u64 {
    let mut best = u64::MAX;
    for _ in 0..10_000 {
        let a = Instant::now();
        let b = Instant::now();
        best = best.min((b - a).as_nanos() as u64);
    }
    best
}

fn header(scenario: &str, note: &str) {
    println!("\n=== {scenario} ===");
    if !note.is_empty() {
        println!("{note}");
    }
    println!(
        "{:<26} {:>7} {:>7} {:>8} {:>9} {:>9} {:>10} {:>9} {:>9} {:>7} {:>6}",
        "impl", "mean", "p50", "p99", "p99.9", "max", "blocks/s", "written", "recv",
        "lost", "torn"
    );
}

fn row(name: &str, s: &Summary, written: u64, c: &Counters, elapsed: Duration) {
    let recv = c.received.load(Ordering::Relaxed);
    let rate = recv as f64 / elapsed.as_secs_f64();
    println!(
        "{:<26} {:>7} {:>7} {:>8} {:>9} {:>9} {:>10.0} {:>9} {:>9} {:>7} {:>6}",
        name,
        s.mean,
        s.p50,
        s.p99,
        s.p999,
        s.max,
        rate,
        written,
        recv,
        c.lost.load(Ordering::Relaxed),
        c.torn.load(Ordering::Relaxed),
    );
}

// ── Scenarios ───────────────────────────────────────────────────────────────

/// S1 — uncontended. Single thread, write then read back. Establishes the floor
/// cost. Expected to be memcpy-dominated and near-identical everywhere; that
/// result is the point, not a failure.
fn s1_uncontended(iters: usize) {
    header(
        "S1 uncontended (single thread, no concurrency)",
        "Floor cost. 2 KB per op, so memcpy should dominate the primitive.",
    );

    for imp in [
        Impl::RingNonBlocking,
        Impl::RingBlocking,
        Impl::RingGuard,
        Impl::MutexRing,
        Impl::MutexCondRing,
        Impl::MutexDeque,
        Impl::Mpsc,
    ] {
        // No consumer thread: drive both sides inline.
        let counters = Arc::new(Counters::default());
        let (mut prod, mut cons): (Box<dyn Prod>, Box<dyn Cons>) = match imp {
            imp if imp.is_ring() => {
                let p = RingProducer::<f32, SLOTS, BLOCK>::new(imp.ring_blocks());
                let c = p.subscribe();
                let cons: Box<dyn Cons> = if imp == Impl::RingGuard {
                    Box::new(RingGuardCons { inner: c })
                } else {
                    Box::new(RingCons { inner: c })
                };
                (Box::new(RingProd { inner: p }), cons)
            }
            Impl::MutexRing => {
                let r = MutexRing::new();
                (Box::new(r.clone()), Box::new(r))
            }
            Impl::MutexCondRing => {
                let r = MutexCondRing::new();
                (Box::new(r.clone()), Box::new(r))
            }
            Impl::MutexDeque => {
                let d = MutexDeque::new();
                (Box::new(d.clone()), Box::new(d))
            }
            Impl::Mpsc => {
                let (tx, rx) = sync_channel::<[f32; BLOCK]>(SLOTS);
                (Box::new(MpscProd(tx)), Box::new(MpscCons(rx)))
            }
            _ => unreachable!("ring variants handled by the is_ring() arm"),
        };

        let mut src = [0.0f32; BLOCK];
        let mut dst = [0.0f32; BLOCK];
        let mut last = -1.0f64;

        // Warmup.
        for seq in 0..1000u64 {
            fill(&mut src, seq);
            prod.write(black_box(&src));
            cons.read(&mut dst);
        }

        let mut lat = Vec::with_capacity(iters);
        let mut written = 0u64;
        let start = Instant::now();
        for seq in 0..iters as u64 {
            fill(&mut src, seq);
            let t = Instant::now();
            let w = prod.write(black_box(&src));
            lat.push(t.elapsed().as_nanos() as u64);
            if w == Wrote::Ok {
                written += 1;
            }
            if cons.read(&mut dst) {
                verify(black_box(&dst), &mut last, &counters);
            }
        }
        let elapsed = start.elapsed();

        row(imp.name(), &percentiles(&mut lat), written, &counters, elapsed);
    }
}

/// Shared driver for the threaded scenarios: the harness thread is the producer,
/// consumers run on their own threads.
fn threaded(
    scenario: &str,
    note: &str,
    imps: &[Impl],
    iters: usize,
    pace: Option<Duration>,
    stall: Option<Stall>,
    n_consumers: usize,
) {
    header(scenario, note);

    for &imp in imps {
        let mut rig = build(imp, n_consumers, stall);
        let mut src = [0.0f32; BLOCK];

        // Warmup.
        for seq in 0..500u64 {
            fill(&mut src, seq);
            rig.prod.write(black_box(&src));
        }
        thread::sleep(Duration::from_millis(20));
        rig.counters.received.store(0, Ordering::Relaxed);
        rig.counters.lost.store(0, Ordering::Relaxed);
        rig.counters.torn.store(0, Ordering::Relaxed);
        // Tell consumers to re-baseline, so the warmup -> measured sequence jump
        // is not miscounted as loss.
        rig.counters.resync_gen.fetch_add(1, Ordering::Relaxed);

        let mut lat = Vec::with_capacity(iters);
        let mut written = 0u64;
        let start = Instant::now();
        let mut next = start;

        for seq in 1_000u64..(1_000 + iters as u64) {
            if let Some(p) = pace {
                next += p;
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                }
            }
            fill(&mut src, seq);
            let t = Instant::now();
            let w = rig.prod.write(black_box(&src));
            lat.push(t.elapsed().as_nanos() as u64);
            if w == Wrote::Ok {
                written += 1;
            }
        }
        let elapsed = start.elapsed();

        // Let consumers drain what is still in flight before reading counters.
        thread::sleep(Duration::from_millis(50));
        let summary = percentiles(&mut lat);
        row(imp.name(), &summary, written, &rig.counters, elapsed);
        rig.finish();
    }
}

fn main() {
    let all = [
        Impl::RingNonBlocking,
        Impl::RingBlocking,
        Impl::RingGuard,
        Impl::MutexRing,
        Impl::MutexCondRing,
        Impl::MutexDeque,
        Impl::Mpsc,
    ];

    println!("ring_vs_mutex — BLOCK={BLOCK} f32 ({} KB/op), SLOTS={SLOTS}", BLOCK * 4 / 1024);
    println!("latencies in ns, measured on the producer side only");
    println!("timer overhead (Instant::now x2 floor): {} ns", timer_overhead());
    println!(
        "consumer also pays a {}-element uniformity scan per block for tear detection",
        BLOCK
    );
    println!("`written` = writes the producer reported as landed. Blocking spmc rings now");
    println!("      return SlowConsumer when gated, so this is comparable across all");
    println!("      bounded impls. Non-blocking spmc overwrites and never refuses, so its");
    println!("      `written` is always the full iteration count and its loss is in `lost`.");
    println!("`blocks/s` is aggregate across consumers — the ring broadcasts, so with n");
    println!("      consumers each block is delivered n times.");
    println!("note: x86_64 is TSO — Relaxed vs Acquire/Release compile identically here,");
    println!("      so this cannot detect spmc's outstanding memory-ordering bugs.");
    println!("      For stable numbers, prefer: taskset -c 0-3 cargo bench");

    s1_uncontended(50_000);

    threaded(
        "S2 realistic pace (1 block / 10.667 ms, i.e. 512 @ 48 kHz)",
        "The actual application rate. Every implementation should show zero loss;\n\
         if not, the harness pacing is wrong rather than the data structure.",
        &all,
        2_000,
        Some(BLOCK_PERIOD),
        None,
        1,
    );

    threaded(
        "S3 saturated (producer and consumer both flat out)",
        "Unrealistic for the application, but gathers percentile-grade samples fast\n\
         and exposes contention behaviour.",
        &all,
        200_000,
        None,
        None,
        1,
    );

    threaded(
        "S4 consumer preemption (the decisive one)",
        "Consumer sleeps 1-20 ms every 8 blocks, simulating a scheduler hiccup or a\n\
         slow FFT consumer. Expect the mutex variants to show producer-latency tail\n\
         spikes with near-zero loss, and the ring to show flat latency with real loss.\n\
         If there is no difference here, the lock-free ring is not earning its keep.",
        &all,
        20_000,
        Some(Duration::from_micros(200)),
        Some(Stall { every: 8, min_us: 1_000, max_us: 20_000 }),
        1,
    );

    // S5 — ring only: mpsc's Receiver cannot be cloned, and the mutex queues pop
    // destructively, so neither has a meaningful multi-consumer form.
    for n in [1usize, 2, 4, 8] {
        threaded(
            &format!("S5 consumer scaling — {n} consumer(s)"),
            if n == 1 {
                "RingProducer::write does an O(consumers) gate scan. This is where the\n\
                 ring may legitimately lose to an O(1)-but-contended lock."
            } else {
                ""
            },
            &all
                .iter()
                .copied()
                .filter(|i| i.supports_multi_consumer())
                .collect::<Vec<_>>(),
            50_000,
            None,
            None,
            n,
        );
    }
}
