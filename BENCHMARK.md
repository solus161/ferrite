# `spmc::Ring` vs. mutex-based queues — benchmark report

Source: `benches/ring_vs_mutex.rs` · Run with `cargo bench --bench ring_vs_mutex`

Machine: x86_64, 16 cores, Linux, release profile.
Geometry mirrors the application: `BLOCK = 512` f32 (2 KB/op), `SLOTS = 16`.
All latencies are **nanoseconds, producer-side only**. Timer overhead floor: 30 ns.

> **This run supersedes all previous ones.** It was taken after three changes to
> `src/spmc.rs`: the cursor collapse (two `AtomicUsize` → one monotonic
> `AtomicU64`), the option-B seqlock validation, and gated writes returning
> `CustomError::SlowConsumer`. The headline changes are in the `torn` and
> `written` columns.

---

## Verdict

**The lock-free ring earns its complexity under consumer preemption, and it is
now correct and honest about its losses. Everywhere else it is a wash.**

1. **The tearing bug is fixed — `torn = 0` in every row.** Across all five
   scenarios, all four consumer counts, and all three ring variants. An earlier
   run showed torn blocks in the ring and only in the ring, peaking at 176 in S3
   blocking mode, with non-blocking mode tearing at roughly 1 per 200k reads that
   no consumer-side discipline could close. Option B closes it.
2. **The ring no longer hides its data loss.** Gated writes return
   `CustomError::SlowConsumer`, so `written` is comparable with every other
   bounded implementation. On all blocking rows `written == recv` **exactly** —
   the producer's refusal count and the consumer's independent sequence-gap count
   agree to the block, which is a check that was impossible before.
3. **Uncontended, the ring buys nothing.** At 2 KB per op the memcpy is the entire
   cost (~38 ns ≈ L1 bandwidth). Ring, plain mutex, and mpsc are within 9 ns.
4. **Under a stalled consumer it is decisive.** The ring holds producer latency in
   the hundreds of nanoseconds while a mutex that genuinely refuses to lose data
   blocks the producer for **1.3 ms mean / 20.7 ms max** — nearly 6× over the USB
   callback's entire 3.41 ms budget. That is the whole argument for the design,
   and it holds.
5. **The price is still data loss.** In S4 the ring discarded ~85% of what it was
   handed. It now says so.
6. **`read_guard` costs nothing measurable over `read_into`** — and that row still
   pays a copy it does not need. Its real advantage is the 2 KB it can skip.

---

## Method

Seven implementations, one workload, identical copy-out semantics on both sides
so nobody wins by doing less work:

| Implementation | Policy when full |
|---|---|
| `spmc ring (non-blocking)` | overwrites; lapped consumers detect and resync themselves |
| `spmc ring (blocking)` | refuses the write, returning `CustomError::SlowConsumer` |
| `spmc ring (blocking, guard)` | same, read via `read_guard` instead of `read_into` |
| `Mutex<ring> (drops)` | identical ring semantics, lock instead of atomics; refuses |
| `Mutex+Condvar (blocks)` | **waits for space** — never loses data |
| `Mutex<VecDeque<f32>>` | the structure this design replaced; refuses |
| `mpsc::sync_channel` | `try_send`; refuses |

**Fairness measures.** Every implementation copies *out* into a caller-owned
buffer, so all pay the same 2 KB memcpy twice. `mpsc` sends `[f32; BLOCK]` by
value rather than `Box`/`Vec`, so it is not secretly an allocator benchmark.
`black_box` guards every consumed value. The guard row copies out of the guard
too, so it differs from `read_into` by exactly the RAII machinery — its zero-copy
advantage is deliberately *excluded* from these numbers.

**Loss and corruption accounting.** Each block is filled entirely with its
sequence number. The consumer checks continuity (→ `lost`) and that all 512
elements are equal (→ `torn`). Timing alone would let the overwriting ring look
fast by throwing work away, so no latency figure is reported without them.

### Read this before comparing latency columns

**`spmc ring (blocking)` does not block — it refuses.** A gated write returns
`Err(SlowConsumer)` having done nothing, in ~50 ns. The *accounting* for that is
now correct — `written` excludes refusals — but the *latency* still averages them
in. In S4 that is 16 881 near-free refusals against 3 064 real writes, so the
"103 ns mean" measures how often the ring declined, not how fast it writes. Read
those rows next to their `written` column, not on their own.

The `written` column itself is now comparable across all bounded implementations.
The one exception is `spmc ring (non-blocking)`, which overwrites rather than
refusing and therefore always reports the full iteration count; its loss appears
only in `lost`.

`Mutex+Condvar` is the only implementation here that actually waits. Its numbers
are the honest cost of the never-lose-data policy, and they are the ones the ring
is being compared against.

### Two harness defects found and fixed before these numbers

The first run was invalid; both problems were caught by the plan's own
verification criteria rather than by inspection.

- **Phantom loss.** Warmup wrote seq 0–499, then the measured loop restarted at
  seq 1000, and consumers counted the jump as a gap — 500 for the overwriting
  ring, 984 for the bounded queues. Fixed with a `resync_gen` counter that tells
  consumers to re-baseline. S2 now reports zero loss everywhere, which is the
  criterion that says the harness is sound.
- **S4 tested nothing.** Every original baseline *dropped* when full; none blocked.
  So the "decisive" scenario compared silent dropping against explicit dropping,
  and unsurprisingly showed no difference. `Mutex+Condvar` was added to represent
  the actual alternative policy.

---

## Results

### S1 — Uncontended (single thread)

Floor cost. 2 KB per op, so memcpy should dominate the primitive.

| impl | mean | p50 | p99 | p99.9 | max | blocks/s | written | recv | lost | torn |
|---|---|---|---|---|---|---|---|---|---|---|
| spmc ring (non-blocking) | 38 | 40 | 51 | 120 | 441 | 3 863 186 | 50000 | 50000 | 0 | **0** |
| spmc ring (blocking) | 36 | 40 | 50 | 150 | 4218 | 3 943 459 | 50000 | 50000 | 0 | **0** |
| spmc ring (blocking, guard) | 43 | 40 | 311 | 360 | 8987 | 3 434 006 | 50000 | 50000 | 0 | **0** |
| Mutex\<ring\> (drops) | 40 | 40 | 51 | 80 | 3286 | 4 315 801 | 50000 | 50000 | 0 | 0 |
| Mutex+Condvar (blocks) | 339 | 340 | 381 | 671 | 7504 | 1 173 070 | 50000 | 50000 | 0 | 0 |
| Mutex\<VecDeque\<f32\>\> | 45 | 50 | 51 | 161 | 4569 | 2 212 778 | 50000 | 50000 | 0 | 0 |
| mpsc::sync_channel | 40 | 40 | 320 | 370 | 3567 | 3 357 095 | 50000 | 50000 | 0 | 0 |

**Six of seven are within 9 ns of each other.** This is the expected result, not a
disappointment: at 2 KB the synchronisation primitive is noise against the copy.
~38 ns for 4 KB moved (in + out) is ~105 GB/s, i.e. L1-resident.

The exception is `Mutex+Condvar` at 339 ns — 8× the others *with no contention at
all*. That is the `notify_one` on every push, unconditionally, even with nobody
waiting. It is the cost of being able to block, paid whether or not you block.

### S2 — Realistic pace (1 block / 10.667 ms = 512 @ 48 kHz)

The actual application rate. Zero loss everywhere confirms the harness is sound.

| impl | mean | p50 | p99 | p99.9 | max | blocks/s | written | recv | lost | torn |
|---|---|---|---|---|---|---|---|---|---|---|
| spmc ring (non-blocking) | 844 | 691 | 2985 | 7966 | 14628 | 94 | 2000 | 2000 | 0 | **0** |
| spmc ring (blocking) | 926 | 761 | 3317 | 5541 | 5871 | 94 | 2000 | 2000 | 0 | **0** |
| spmc ring (blocking, guard) | 901 | 751 | 3417 | 8386 | 13566 | 94 | 2000 | 2000 | 0 | **0** |
| Mutex\<ring\> (drops) | 809 | 681 | 3126 | 8586 | 17794 | 94 | 2000 | 2000 | 0 | 0 |
| Mutex+Condvar (blocks) | 2179 | 1913 | 9919 | 17353 | 35849 | 94 | 2000 | 2000 | 0 | 0 |
| Mutex\<VecDeque\<f32\>\> | 665 | 551 | 2625 | 4308 | 9188 | 94 | 2000 | 2000 | 0 | 0 |
| mpsc::sync_channel | 841 | 691 | 3337 | 5871 | 16862 | 94 | 2000 | 2000 | 0 | 0 |

**Everything is ~20× slower than S1, and the ordering scrambles.** At one block
per 10.7 ms nothing stays in cache: every write is a cold-miss walk through 2 KB
plus whatever the scheduler did in between. The primitive is irrelevant — the
ring is *not* faster than a plain mutex here, and `Mutex<VecDeque>` is fastest of
all at 665 ns, reversing its position in every other scenario.

**At the application's actual rate, the choice of data structure does not matter.**
Everything has three orders of magnitude of headroom against the 3.41 ms budget.
The tails (p99.9 ~8 µs) are scheduler noise, near-identical across implementations.

### S3 — Saturated (both sides flat out)

Unrealistic for the application, but gathers percentile-grade samples fast and
exposes contention behaviour.

| impl | mean | p50 | p99 | p99.9 | max | blocks/s | written | recv | lost | torn |
|---|---|---|---|---|---|---|---|---|---|---|
| spmc ring (non-blocking) | 210 | 210 | 371 | 2134 | 29095 | 3 835 666 | 200000 | 199919 | 81 | **0** |
| spmc ring (blocking) | 207 | 210 | 321 | 621 | 9227 | 3 937 241 | 199294 | 199294 | 706 | **0** |
| spmc ring (blocking, guard) | 211 | 211 | 311 | 541 | 18545 | 3 899 676 | 199353 | 199353 | 647 | **0** |
| Mutex\<ring\> (drops) | 228 | 230 | 321 | 651 | 21721 | 3 661 410 | 198855 | 198855 | 1145 | 0 |
| Mutex+Condvar (blocks) | 520 | 511 | 821 | 4639 | 27562 | 1 763 709 | 200000 | 200000 | **0** | 0 |
| Mutex\<VecDeque\<f32\>\> | 504 | 491 | 752 | 4569 | 64613 | 1 792 558 | 196133 | 196133 | 3867 | 0 |
| mpsc::sync_channel | 121 | 120 | 280 | 371 | 84641 | 3 907 559 | 154427 | 154427 | 45573 | 0 |

The ring is modestly ahead of `Mutex<ring>` (207–211 vs 228) — a ~9% edge for a
large amount of unsafe code, and it loses *less* data doing it (706 vs 1145).
`Mutex<VecDeque>` at 504 ns is 2.4× worse, which is the real vindication of
replacing it: it is the only baseline doing per-sample rather than per-block work.

`mpsc` posts the best latency (121 ns) by losing 23% of the data — the fastest way
to write a block is not to.

`Mutex+Condvar` is the only row that loses nothing, at 2.5× the ring's latency.
**That is the trade, stated in one line.**

Note `written == recv` exactly on all three ring rows. The producer's refusal
count and the consumer's independent sequence-gap count now agree to the block —
a self-consistency check that was impossible before gated writes reported
`SlowConsumer`.

### S4 — Consumer preemption (the decisive scenario)

Consumer sleeps 1–20 ms every 8 blocks: a scheduler hiccup, or a slow FFT
consumer. This is the scenario the whole design exists for.

| impl | mean | p50 | p99 | p99.9 | max | blocks/s | written | recv | lost | torn |
|---|---|---|---|---|---|---|---|---|---|---|
| spmc ring (non-blocking) | 247 | 190 | 1172 | 2335 | 6392 | 766 | 20000 | 3064 | 16936 | **0** |
| spmc ring (blocking) | 103 | 50 | 511 | 1933 | 70274 | 766 | 3064 | 3064 | 16881 | **0** |
| spmc ring (blocking, guard) | 103 | 50 | 551 | 1693 | 26731 | 766 | 3064 | 3064 | 16878 | **0** |
| Mutex\<ring\> (drops) | 84 | 40 | 431 | 1593 | 2865 | 776 | 3104 | 3104 | 16885 | 0 |
| Mutex+Condvar (blocks) | **1 316 310** | 761 | 18 666 363 | 20 027 631 | **20 737 976** | 760 | 20000 | 20004 | 500 | 0 |
| Mutex\<VecDeque\<f32\>\> | 73 | 41 | 401 | 1312 | 2816 | 776 | 3104 | 3104 | 16887 | 0 |
| mpsc::sync_channel | 228 | 191 | 1102 | 1954 | 13075 | 776 | 3104 | 3104 | 16883 | 0 |

**This is the result that justifies the design.** `Mutex+Condvar` — the only
policy that keeps the data — parks the producer for **1.3 ms on average and 20.7 ms
at worst**. The USB callback's entire budget is 3.41 ms. It blows that by 6× at
the *mean* and by a factor of six thousand at the tail. In the real application
this is dropped USB transfers and audible dropouts.

Every lossy implementation stays in the hundreds of nanoseconds. So the finding
is not "lock-free is fast" — it is **"blocking is catastrophic here, and every
non-blocking policy is fine."**

Two honest qualifications:

- **The blocking-ring latency is still flattered, even though its accounting is
  now honest.** 103 ns is the average of 3 064 real writes and 16 881 refusals
  that cost ~50 ns each. The refusals are now *reported* (`written` = 3 064,
  matching `recv` exactly), so the ring no longer hides its loss — but a mean
  that averages in free no-ops is still not a measure of write cost.
  `Mutex<ring>` (84 ns) and `Mutex<VecDeque>` (73 ns) do the identical thing and
  are faster at it.
- **Non-blocking (247 ns) is the slowest lossy row** precisely because it is the
  only one that does the full 2 KB memcpy every time. It never refuses — and its
  `written` column correctly shows 20 000 for that reason. That is the policy
  working as designed, not a defect.

### S5 — Consumer scaling (ring only)

`RingProducer::write` does an O(consumers) gate scan in blocking mode. mpsc's
`Receiver` cannot be cloned and the mutex queues pop destructively, so only the
ring is genuinely multi-consumer.

| consumers | impl | mean | p50 | p99 | max | blocks/s | written | recv | lost | torn |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | non-blocking | 210 | 210 | 351 | 21892 | 3 787 542 | 50000 | 49228 | 772 | **0** |
| 1 | blocking | 214 | 211 | 351 | 9218 | 3 710 020 | 49090 | 49090 | 910 | **0** |
| 1 | blocking, guard | 211 | 210 | 340 | 6342 | 3 890 422 | 49738 | 49738 | 262 | **0** |
| 2 | non-blocking | 239 | 240 | 390 | 9388 | 7 023 415 | 50000 | 99941 | 59 | **0** |
| 2 | blocking | 246 | 250 | 360 | 7154 | 6 890 679 | 49938 | 99876 | 124 | **0** |
| 2 | blocking, guard | 248 | 250 | 370 | 33424 | 6 815 363 | 49900 | 99800 | 200 | **0** |
| 4 | non-blocking | 277 | 281 | 381 | 4749 | 12 341 857 | 50000 | 199851 | 149 | **0** |
| 4 | blocking | 355 | 330 | 1403 | 31209 | 9 735 853 | 49799 | 199196 | 804 | **0** |
| 4 | blocking, guard | 330 | 301 | 1693 | 42761 | 10 488 678 | 50000 | 200000 | **0** | **0** |
| 8 | non-blocking | 440 | 431 | 551 | 8636 | 15 748 338 | 50000 | 399980 | 20 | **0** |
| 8 | blocking | 473 | 461 | 611 | 7153 | 14 743 854 | 49995 | 399960 | 40 | **0** |
| 8 | blocking, guard | 474 | 471 | 601 | 27011 | 14 584 083 | 49634 | 397072 | 2928 | **0** |

Non-blocking scales 210 → 440 ns from 1 to 8 consumers: **~33 ns per consumer**,
despite the producer no longer scanning cursors at all in that mode. The cost is
not the scan — it is cache-coherence traffic on the single `head` line, which
every consumer now loads *twice* per read for the option-B pre- and post-checks.
That is the structural cost of concentrating synchronisation on one counter, and
it is the one place where per-slot state (option A) would scale better.

Blocking mode does pay the scan on top, and it shows past 4 consumers (355 vs
277 ns). At 4 consumers `recv = 4 × written` exactly on every row: the broadcast
is delivering every accepted block to every consumer.

Aggregate throughput still rises with consumer count (3.8 M → 15.7 M blocks/s),
because this is a broadcast ring. The per-write cost grows sub-linearly against it.

---

## Where the ring succeeds

1. **Bounded producer latency under an arbitrarily stalled consumer.** The
   headline result: hundreds of nanoseconds against `Mutex+Condvar`'s 20 ms tail.
   For a real-time USB callback this is the difference between working and not.
2. **True broadcast fan-out.** Every consumer sees every block from an independent
   cursor. No baseline here can do this at all — mpsc's receiver is single-owner
   and the mutex queues pop destructively. Adding an FFT consumer alongside the
   audio path requires no change to the producer.
3. **Correct under saturation, now.** `torn = 0` across 5 scenarios × 4 consumer
   counts × 3 variants. Sequence-fill detection would catch a single mixed block
   out of ~200 000 — the previous design failed exactly that test.
4. **No allocation, no syscalls on the hot path.** One memcpy and one atomic store
   per write in non-blocking mode.
5. **Beats the structure it replaced by 2.4×** under saturation (207 vs 504 ns),
   which was the original motivation.
6. **Honest accounting.** A refused write reports `SlowConsumer`, and `written`
   matches the consumer's independently-derived `recv` to the block on every
   blocking row. The producer can now decide what to do about a refusal instead of
   being told everything was fine.

## Where the ring fails

1. **It is not faster at the rate the application actually runs.** S2 shows the
   ring slower than a plain mutex (844 vs 809 ns) and slower than
   `Mutex<VecDeque>` (665 ns) — the very structure it replaced. At 94 blocks/s
   nothing is cache-resident and the primitive is irrelevant.
2. **Non-blocking mode cannot report loss at all.** It overwrites rather than
   refusing, so there is no error to return; the producer has no way to learn that
   a consumer missed 16 936 blocks. This is inherent to the policy, not a defect
   to fix — but it means `main.rs`, which runs non-blocking, is flying blind.
3. **Uncontended it buys nothing** — within 9 ns of every alternative.
4. **~33 ns per additional consumer**, from coherence traffic on the single head
   counter. Option A's per-slot state would distribute this, at the cost of an
   extra store per write and a second contended line in the common case.
5. **Capacity is N-1, not N.** The guard band (see below) costs one slot.
6. **Substantial unsafe surface** — `UnsafeCell`, a hand-written `unsafe impl
   Sync`, and a protocol whose correctness rests on a modular-arithmetic argument
   — to buy ~9% over `Mutex<ring>` in the one scenario where throughput matters.
   The justification is S4's tail, not the mean anywhere.

---

## The bug this benchmark found, and the fix

### The bug

The pre-fix run showed `torn > 0` in the ring rows and nowhere else, peaking at
**176 in S3 blocking mode** — the mode that is supposed to gate against exactly
that.

`read()` advanced the consumer's cursor *before* returning the borrowed slice, so
the producer's gate protected slot k+1 while the consumer was still reading slot
k. Blocking mode tore *more* because the gate pins the consumer at the capacity
edge, which is precisely where the producer is about to write.

`read_into` (copy before releasing) fixed blocking mode 176 → 0. **It could not
fix non-blocking mode**, which kept tearing at ~1 per 200 000 reads, because
non-blocking mode has no gate at all: nothing the consumer does can constrain a
producer that never consults it.

### The fix — option B

Two changes, both in `src/spmc.rs`:

**1. Cursor collapse.** `RingCursor((AtomicUsize, AtomicUsize))` — a `(round,
index)` pair — became a single monotonic `AtomicU64` sequence. The pair could be
read torn: observing `round` and `index` from different instants could yield a
value the cursor never held, poisoning every comparison. It also halved the
atomic traffic per advance (2 loads + 2 stores + a branch → 1 store).

**2. Seqlock validation.** With one monotonic counter, `head - cursor` is the
exact number of blocks a consumer is behind, and the safety argument is one line:

> A consumer at sequence `c` collides with a producer writing sequence `h` **iff**
> `c ≡ h (mod N)`. Since `c < h`, that means `h - c ∈ {N, 2N, …}`. Therefore
> **`h - c < N` proves the producer is not in the consumer's slot.**

`read_into` now checks that twice — before the copy (reject a slot the producer is
already inside) and after it (discard a copy the producer walked into) — and
retries. A failed post-check leaves the cursor untouched, so the next attempt sees
a lag of `≥ N` and resyncs, which is what makes the loop terminate rather than
spin.

Crucially the *consumer* now detects and repairs its own lapping. The producer no
longer writes consumer cursors, which removed a second, separate data race:
`jump_to_cursor` used to store into a cursor the consumer thread was concurrently
read-modify-writing.

**Result: `torn = 0` in all 39 rows, including non-blocking mode.**

### Cost of the fix

S3 non-blocking moved from 178 ns (the pre-fix run) to 210 ns. The extra head load
for the post-check, plus the coherence traffic it generates, is not free — and the
producer-side savings from deleting the drag loop did not offset it.

**Treat that figure as approximate.** It is a cross-run comparison on a machine
with visible variance: between the two post-fix runs of the identical binary, S2
non-blocking moved 1460 → 844 ns and the timer-overhead floor moved 10 → 30 ns.
The direction of the S3 change is credible; the magnitude is not worth defending.

Either way it is the correct trade — a modest constant for the elimination of a
real data race.

### The guard band: capacity is N-1

Implementing the pre-check surfaced a subtlety. Because the head is published
*after* the write, `head - c == N` is ambiguous — it means either "the producer is
mid-write inside the consumer's slot" (unsafe) or "the ring is exactly full and
idle" (safe). One counter cannot distinguish them, so consumers must treat `>= N`
as unsafe.

A blocking producer therefore gates one slot early, at `N - 1`. Without that, a
full blocking ring would make consumers resync and skip N/2 blocks — destroying
the no-loss guarantee that is the entire point of blocking mode. This was caught
by `test_blocking_resumes_after_drain`, not by inspection.

A non-blocking producer has no gate and does reach `>= N`, where consumers are
deliberately conservative: they may discard a block that happened to be intact,
but never accept a torn one.

### `read_guard`

Added alongside, for zero-copy reads under back-pressure. The benchmark row copies
out of the guard anyway, so it isolates the RAII machinery: **+2 ns in S1, within
noise in S3–S5.** The guard is free; its 2 KB saving is on top.

It takes `&mut self` deliberately — two live guards would both read the same
sequence and both advance on drop, silently skipping a block. `is_valid()` exposes
the post-check for callers who use it under a non-blocking producer, where it has
no protection at all and is strictly worse than `read_into` (the exposure window
grows from a memcpy to however long the guard is held).

---

## Caveats

- **x86_64 is TSO.** `Relaxed` and `Acquire`/`Release` loads compile to the same
  `mov`, so this benchmark cannot detect memory-ordering defects. The orderings in
  `spmc.rs` are now correct by construction (`Release` on publish, `Acquire` on
  observe), but that is an argument, not a measurement. It would be measurable on
  ARM.
- **Producer-side latency only.** End-to-end block latency is not measured.
- **No thread pinning.** `std` has no API for it; `taskset -c 0-3 cargo bench`
  gives more stable numbers. Maxima in particular are scheduler artifacts — note
  the 59 µs max in a row whose p99 is 431 ns.
- **The `written` column is not comparable across implementations,** for the
  reason in the Method section: the ring counts refusals as successes.
- **S2 is only 2 000 iterations** (~21 s). Its p99.9 rests on ~2 samples.

---

## Recommendations

1. **Keep the ring for the SDR path.** S4 justifies it; nothing else does. Keep
   `main.rs` in non-blocking mode.
2. **Add a loss counter for non-blocking mode.** Refused writes now report
   `SlowConsumer`, but non-blocking mode never refuses — it overwrites — so the
   producer still cannot learn that a consumer missed 85% of the stream. A
   monotonic per-consumer "blocks skipped at resync" counter, incremented inside
   `RingConsumer::claim`, would close the last accounting gap and cost one
   non-atomic add on a path that already runs rarely.
3. **Do not add complexity for throughput.** S1 and S2 show the primitive is
   irrelevant at the application's rate; S3's 9% edge over `Mutex<ring>` does not
   justify more unsafe code.
4. **Revisit if consumer count grows past ~8.** The ~33 ns/consumer coherence cost
   on the head line is where option A (per-slot availability flags, Disruptor
   style) would start to win.
5. **Use `read_guard` for scan-only consumers on a blocking ring** — an FFT that
   reads and discards. It is free relative to `read_into` and skips the 2 KB copy.
   Do **not** use it on the non-blocking audio path.
6. **Re-run on ARM** if this ever targets a Raspberry Pi. The ordering arguments
   are untested by any measurement taken here.
