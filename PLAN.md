# ferrite — design notes & plan

FM radio receiver: RTL-SDR → demodulate → audio out, with a terminal UI (spectrum + controls).

**Current stack:** `rtlsdr_mt` 2.2 (binds system `librtlsdr`) + `cpal` 0.18 + `ratatui`.

> ### ⚠️ Driver pivot (why parts of this doc are historical)
> This project started on **`rs-rtl`** (pure-Rust, on `nusb`) and much of the analysis
> below — §2 buffer/allocation strategy, §5 vendored fork & upstream PR — was written for
> it. **The hardware on hand has a Fitipower FC0013 tuner, which `rs-rtl` does not support**
> (it probes only R820T/R828D). We switched to **`librtlsdr`** via the `rtlsdr_mt` crate,
> which supports it.
>
> - **Still fully applicable:** §1 (threads/deadlines), §3 (ring design), §4 (sizing), §6
>   (measurement mindset), §7 (build order).
> - **Now moot:** §2 and §5. The DMA-buffer copy happens *inside* the C library, out of
>   reach — no `allocate()`/pool to tune, no crate to fork or PR. Kept for the reasoning,
>   which still transfers if we ever return to a Rust driver (or add FC0013 support to
>   `rs-rtl` as a future contribution — see §5).
> - The `vendor/rs-rtl/` copy has been deleted (was unused by the build).

See **§0** for machine setup (kernel module blacklist, `librtlsdr-dev`, udev).
See **§7** for what's built and **§8** for the prioritised roadmap of what's next.

---

## 0. Machine setup (Linux)

One-time host prep to make an RTL-SDR dongle usable from userspace. All need root; in this
session run them with the `!` prefix.

### 0.1 Unbind the kernel DVB-T driver

At plug-in the kernel claims the dongle as a TV tuner (`dvb_usb_rtl28xxu` → `rtl2832` →
`rtl2832_sdr`), putting it in DVB mode so the SDR tuner won't answer. Blacklist the stack:

```sh
sudo sh -c 'printf "blacklist dvb_usb_rtl28xxu\nblacklist rtl2832_sdr\nblacklist rtl2832\n" > /etc/modprobe.d/blacklist-rtlsdr.conf'
```

Unload for the current session (blacklist only stops *future* auto-load):

```sh
sudo modprobe -r dvb_usb_rtl28xxu rtl2832_sdr rtl2832
```

Then **physically unplug and replug** the dongle — it must re-enumerate to leave DVB mode.
Confirm the USB device number changed: `lsusb | grep 0bda:2838`.

> Note: `librtlsdr` auto-detaches the kernel driver on open, so `rtl_test`/`rtl_fm` work
> even without the blacklist. The blacklist matters mainly for a pure-`nusb` driver, which
> does not auto-detach. Harmless to keep either way.

### 0.2 Install librtlsdr + dev files

Runtime is `librtlsdr2`; the `rtlsdr_sys` build script needs the **dev** package for
`librtlsdr.pc` (pkg-config), the header, and the `librtlsdr.so` link:

```sh
sudo apt install -y librtlsdr-dev
```

### 0.3 udev permissions (non-root USB access)

If `cargo run` errors opening the device (rather than producing sound), add a rule so your
user can claim it without sudo:

```sh
echo 'SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE="0666"' | sudo tee /etc/udev/rules.d/20-rtlsdr.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Then re-plug. (`0bda:2838` = this RTL2838 dongle; adjust IDs for other hardware.)

### 0.4 Verify

`rtl_test -t` (from the `rtl-sdr` package) is the reference check — it prints the detected
tuner. Ours reports **`Fitipower FC0013`**, which is why the driver choice matters.

---

## 1. Architecture: three threads, three deadline classes

You only *create* one of these. On `librtlsdr`/`rtlsdr_mt`: the USB thread is internal to
librtlsdr, and `read_async` blocks *your* calling thread to deliver buffers via callback.
The audio thread is spawned by cpal when you call `build_output_stream`. (Under the old
`rs-rtl` design the USB thread was `rs-rtl::start_streaming` — same three-thread shape.)

| Thread | Created by | Job | Deadline | Miss = |
|---|---|---|---|---|
| USB harvest + DSP | `rs-rtl` | poll USB → demod → write rings | hard, ~60 ms slack | FIFO overflow → audible pop |
| audio callback | **cpal** | drain audio ring → DAC | hard, ~10.7 ms | underrun → click |
| UI | **you** | input, FFT, render | **soft**, ~16 ms | dropped frame, nobody notices |

**Why threads and not one event loop:** a deadline is a time by which work must finish,
where finishing late is a failure even if the output is correct. Work in one loop runs
sequentially, so the *slowest* thing in the loop sets the worst case for everything in it.
A terminal write can block for 200 ms over SSH. Putting that in the same loop as USB
harvest converts "dropped frame" into "audible click". Separate threads let the scheduler
preempt the stalled UI and run the audio callback on time.

Threads here buy **isolation of failure**, not throughput.

**Why cpal must own a thread:** the DAC consumes at exactly 48 kHz, paced by a crystal on
the sound card. Every ~10.7 ms the driver raises an interrupt: "give me 512 more frames
now." Something must be blocked in the kernel ready to answer it. You can't poll for this —
the sound card's crystal, the dongle's crystal, and your CPU clock are three independent
oscillators that drift apart by milliseconds per minute. The callback is driven by the
device's own clock, so it can't drift relative to it.

---

## 2. Buffer strategy

> **HISTORICAL (rs-rtl / nusb).** Moot on `librtlsdr` — the DMA buffer, its allocation, and
> the copy to userspace all happen inside the C library. `rtlsdr_mt::Reader::read_async`
> hands your callback a `&[u8]` view of a librtlsdr-owned buffer; there is nothing to
> `allocate()`, pool, or zero-copy from Rust. Kept for the reasoning, which applies again
> if we return to a Rust driver.

### The options considered

| | DMA buffer alloc | Kernel→user copy | Per-chunk user alloc | Per-chunk memcpy | Public API |
|---|---|---|---|---|---|
| **1** current upstream | malloc, per chunk | yes (bounce) | `Vec` malloc | yes | `Vec<u8>` |
| **2b** | mmap ×15, **once** | **no** (direct DMA) | `Vec` malloc | yes | `Vec<u8>` unchanged |
| **2c** ← **chosen** | mmap ×15, **once** | **no** (direct DMA) | **none** | yes (into ring) | + `read(&mut [u8])` |
| **3** buffer pool | mmap ×~48, once | no | none | none | new `Chunk` guard |

**Decision: 2c in the vendored fork. PR 2b upstream first.**

The two differ only in what replaces the `mpsc` channel at `rtlsdr.rs:725`:

- **2b** keeps the channel, sends `Vec<u8>`. One `Vec` malloc per chunk survives.
  Non-breaking — `recv() -> Option<Vec<u8>>` untouched, zero downstream risk, ~10 line diff.
- **2c** replaces it with a preallocated **circular byte ring** (ring **a**, §3). The
  completion handler memcpys straight from the DMA buffer into the ring; no `Vec` is ever
  created. Adds `read(&mut [u8]) -> usize` to the API.

2c is strictly better for the app, for three reasons beyond the malloc:

- **Decouples USB chunk size from DSP block size.** USB delivers 8192 samples; the FFT
  wants 4096. With a queue of `Vec`s you stitch chunks by hand. With a byte ring the
  consumer reads whatever it wants.
- **The caller supplies the destination.** `read(&mut [u8])` means the consumer's buffer
  can be a fixed array owned by the DSP struct, allocated once at startup. This is as close
  to the original "fixed buffer, no per-read allocation" goal as the hardware permits.
- **Keeps the option-3 footgun unrepresentable.** You never hold a DMA buffer, so you can
  never hold one too long. Same reason POSIX `read()` copies — it decouples the kernel's
  buffer lifetime from the application's.

Both share the same DMA-side change (`allocate()` + in-thread recycling), which is where
most of the win lives. Since 2c changes the public API, **PR 2b upstream** (non-breaking,
easy to accept) and carry 2c locally, or open an issue proposing `read()` as an additive
method. Option 3 stays on the table as an additive `recv_buffer() -> Chunk` only if
measurement ever justifies it. Don't do it speculatively.

### 2c in code

```rust
// startup — mmap once per buffer, N times total
for _ in 0..num_transfers {
    let buf = ep_in.allocate(transfer_size);   // NOT Buffer::new
    ep_in.submit(buf);
}

// per completion — recycle in-thread, buffer never leaves this thread
let mut buf = completion.buffer;
ring.write(&buf[..]);          // memcpy into preallocated ring — no Vec, len() == actual_len
buf.clear();                   // preserves capacity AND requested_len
ep_in.submit(buf);             // resubmit BEFORE any blocking handoff

// 2b variant: let data = buf[..].to_vec(); … ; if tx.send(data).is_err() { break; }
```

### Facts that make this work (nusb 0.2.5)

- `Buffer::len()` **already equals `actual_len`** after an IN transfer completes
  (`buffer.rs:100`) — the `[..completion.actual_len]` slicing upstream is redundant.
- `Buffer` is `Send + Sync` (`buffer.rs:204`) and `Deref<Target=[u8]>` (`buffer.rs:255`).
- `clear()` sets `len = 0` but preserves `capacity` and `requested_len` (`buffer.rs:149`)
  — so **no `set_requested_len()` call is needed** on the recycle path. Both
  `Buffer::new` (`buffer.rs:65`) and `Buffer::mmap` (`buffer.rs:91`) set it at creation,
  and the Linux backend restores it on completion (`linux_usbfs/transfer.rs:106`).
- `ep.allocate()` mmaps a kernel-shared page so the kernel DMAs **directly** into it,
  no bounce copy (`device.rs:708-713`).

### Traps ruled out along the way

- **`allocate()` per chunk without recycling is WORSE than upstream** — trades a
  ~100 ns warm-arena malloc for an mmap syscall + page faults + munmap/TLB shootdown.
  nusb's own doc: *"only beneficial for buffers that will be used repeatedly."*
  `allocate()` and recycling are one change, not two.
- **`Arc<&[u8]>` / `Arc<[u8]>` doesn't avoid the copy.** `Arc::from(&[u8])` allocates and
  memcpys. And `Arc<T>` gives shared ownership of an allocation you already own outright —
  the move is to *move* the `Buffer`, not wrap it.
- **Sending `(ptr, len)` downstream and resubmitting immediately is a data race.** The
  kernel DMAs into those pages within ~60 ms while the consumer reads. UB (aliasing a live
  `&[u8]` with kernel writes), plus use-after-free on the teardown path when the `Buffer`
  is dropped. `ptr` is `pub(crate)` for this reason.
- **Stack-allocated DMA is impossible**, in any design, in any crate. `submit()` takes an
  owned `Buffer` because the kernel holds that memory after the call returns. Single
  threading doesn't change it. Writing a driver from scratch hits the same signature.

---

## 3. Ring design: three rings, two policies

### "Circular" ≠ "overwriting"

**Circular** describes the *memory layout*: a fixed array with wraparound indexing, so you
never move data and never allocate. It says nothing about what happens when the producer
catches the consumer. That is a **separate, independent policy**:

| Policy | When full, producer… | Consumer loses | Right for |
|---|---|---|---|
| **Overwriting** (lossy / broadcast) | writes anyway, laps the reader | old data, silently | only-newest-matters: waterfall, telemetry, video preview |
| **Bounded** (SPSC queue) | drops-and-counts, or blocks | nothing silently | every-item-matters: audio, IQ byte streams |

Both are circular rings. The split across our three rings is not "which cursor" but
**"may this consumer skip or not."**

Note that "bounded" here does **not** mean "blocks". Blocking the DSP thread would stall
USB harvest — the exact failure we're avoiding. The rule for bounded rings is:

> A full or empty ring is a **defect to measure**, not a normal operating mode.
> Drop-and-count on overrun, silence-and-count on underrun. A nonzero counter means fix
> the rate mismatch (resampling), not enlarge the ring.

### The three rings

```
                    ┌─ ring c.write(iq) ─→ [broadcast ring, OVERWRITING]
                    │                            ↑ cursor      ↑ cursor
USB Buffer ─→ ring a│                          FFT/UI       (future: recorder, scanner…)
   ↓ resubmit now   │
[back to kernel]    └─ demod → f32 48k ──→ [ring b, BOUNDED] ──→ cpal callback
                                                    │
                                            fill level → resampler trim (later)
```

| # | Where | Carries | Rate | Policy |
|---|---|---|---|---|
| **a** | inside rs-rtl: USB thread → DSP (replaces `mpsc` at `rtlsdr.rs:725`) | raw IQ `u8` | 2.048 MS/s | bounded |
| **b** | DSP → cpal | demod'd `f32` | 48 kHz | bounded |
| **c** | DSP → FFT / UI | raw IQ | 2.048 MS/s | overwriting |

### Ring a — USB delivery (bounded byte ring)

This is the 2c change in §2. Preallocated circular `[u8]`, sized to a few chunks.

- **Bounded, not overwriting.** A gap in the IQ stream is a discontinuity the demodulator
  turns into a pop — same reason as ring b. Overwrite is wrong here.
- Producer memcpys from the DMA `Buffer` and resubmits immediately; the `Buffer` never
  leaves the USB thread and is never aliased.
- Consumer API is `read(&mut [u8]) -> usize` — caller supplies a fixed destination buffer
  owned by the DSP struct.
- Size: ≥ a few transfer chunks so short DSP stalls don't overrun. 8 × 16 KB = 128 KB is a
  reasonable start; it plays the role `queue_depth` (32) plays today.

### Ring b — audio (bounded SPSC)

Audio is a continuous-time signal: lapping the reader splices the waveform and you hear a
click. And cpal *can't catch up* — it consumes at exactly 48 kHz, paced by the sound card
crystal. It isn't a slow consumer that will speed up; it's a fixed-rate one.

cpal also wants *different data* from the FFT — demodulated f32 at 48 kHz, not raw IQ at
2.048 MS/s — so one ring couldn't serve both regardless of cursor policy.

- Must be ≥ the worst-case producer stall, since absorbing that is its whole job.
- Start at **4096 f32 frames ≈ 85 ms** at 48 kHz. Costs 85 ms of latency; irrelevant for
  broadcast listening.
- On underrun: fill silence, bump a counter, never block. The callback must never
  allocate, lock, or block.
- **Later:** clock drift between the dongle's crystal and the sound card's needs a
  resampler whose ratio is trimmed by a control loop watching ring fill level. Not v1.

### Ring c — broadcast (overwriting, multi-cursor)

LMAX Disruptor / `PACKET_MMAP` pattern. Producer never blocks on any consumer; a lagging
consumer gets lapped and jumps forward. Correct for a waterfall — you want the newest
spectrum, and a skipped frame is invisible.

- Plain userspace memory, **not** a DMA target. The memcpy into it is the safety boundary.
- Publish with an `AtomicU64` write sequence, `Release` store after the memcpy;
  consumers `Acquire`-load.
- Lap detection: after reading, check `write_seq - cursor > capacity`. If so, discard the
  read, jump to `write_seq - capacity + margin`, bump a `lapped` counter (UI health metric,
  same role `dropped_chunks()` plays for USB).
- Single producer ⇒ no CAS on the write side, plain store + release fence.
- Size for the slowest consumer's window: FFT needs the most recent 4096 samples, so 64K
  samples is generous. Power of two so wraparound is a mask.

### Anti-pattern

**A DMA `Buffer` (or a `Chunk`, if option 3 is ever adopted) must never leave the DSP
thread.** Hand one to the renderer and a slow terminal write pins it for 200 ms; at 60fps
that drains the pool into permanent mmap-per-chunk — worse than upstream, and it only
shows up over SSH or on a loaded machine. Copy out what the UI needs, drop immediately.

---

## 4. Sizing

`transfer_size` (16384) is a **byte quantum** set by USB, not by radio. Must be a nonzero
multiple of max packet size (512 for high-speed bulk) or the transfer fails with
`InvalidArgument` (`device.rs:751`). Leave it.

`num_transfers` (15) is where sample rate bites. It sets the **time** cushion:

```
cushion = num_transfers × transfer_size / (2 × sample_rate)
```

| Sample rate | Cushion at `num_transfers = 15` |
|---|---|
| 250 kS/s | 491 ms |
| 1.024 MS/s | 120 ms |
| 2.048 MS/s | **60 ms** |
| 2.4 MS/s | 51 ms |
| 3.2 MS/s | **38 ms** |

15 is inherited from librtlsdr's default (`rtlsdr.rs:56`), not derived. It's a fixed
*count*, so the cushion shrinks exactly when the hardware is most stressed. To hold a
constant cushion:

```rust
// ~100 ms of runway regardless of sample rate
let num_transfers = ((sample_rate as usize * 2 / 10) / transfer_size).max(15);
```

Memory cost is trivial (15 × 16 KB = 240 KB; 39 buffers ≈ 640 KB). **Separate concern —
do not put this in the allocation PR.**

Rough end-to-end latency budget at 2.048 MS/s: 60 ms USB pool + ~30 ms ring a (128 KB) +
85 ms ring b ≈ 175 ms. (Under 2b it's 128 ms for the `queue_depth` 32 channel instead of
ring a, ≈ 270 ms.) Fine for broadcast; tune down later once `dropped_chunks()` is proven
to sit at zero.

---

## 5. Vendored fork & upstream PR

> **HISTORICAL / not the current path.** We no longer build on `rs-rtl`; `vendor/rs-rtl/`
> is unused and can be deleted. The allocation PR described here is moot for this project.
> **If** we ever want to give back to `rs-rtl`, the natural contribution given our hardware
> is **adding FC0013 tuner support** (a new tuner driver alongside the R82xx one) — a much
> bigger piece than the buffer tweak, and one we can test directly. The buffer/`allocate()`
> analysis below stands on its own if someone picks it up separately.

**Status (frozen):** `rs-rtl` 0.4.2 was vendored to `vendor/rs-rtl/`; the dependency has
since been removed from `Cargo.toml`.

Upstream: **https://github.com/xoolive/desperado** (author Xavier Olive), crate lives at
`crates/rs-rtl`, release 0.4.2 = commit `32c76e1`.

### Change sites in `vendor/rs-rtl/src/rtlsdr.rs`

Line numbers are **upstream** positions (0.4.2). The working copy has already been edited
— re-derive before starting.

| Line | Current | Change |
|---|---|---|
| 943 | `Buffer::new(transfer_size)` | → `ep_in.allocate(...)`, initial fill |
| 1037 | `Buffer::new(transfer_size)` | → recycle, error resubmit |
| 1045 | `completion.buffer[..].to_vec()` | → `ring.write()` (2c) / `to_vec()` (2b), then recycle |
| 1049 | `Buffer::new(transfer_size)` | → recycle, empty-transfer resubmit |
| 1067 | `Buffer::new(transfer_size)` | → recycle, normal resubmit |
| 725 | `mpsc::sync_channel::<Vec<u8>>` | → ring a (2c only); unchanged for 2b |

Also: move the resubmit **ahead of** the downstream handoff. Upstream sends first
(`rtlsdr.rs:1057-1067`), so during a backpressure stall the USB queue drains while
blocked. Refilling first keeps it full — free cushion.

No `set_requested_len()` on the recycle path — `clear()` preserves it (see §2).

### PR checklist

- [ ] Baseline commit of the pristine vendored copy first, so `git diff` == the PR
- [ ] PR **2b** (non-breaking). Keep 2c local, or **open an issue first** proposing
      `read(&mut [u8])` as an additive method. Same for the option-3 `Chunk` API.
- [ ] Diff must touch `src/` only — the vendored `Cargo.toml` is the crates.io-normalized
      form; upstream uses workspace inheritance (see `Cargo.toml.orig`). Carrying it over
      would look like tearing out their workspace setup.
- [ ] Base off `32c76e1` or current upstream `main`, in `crates/rs-rtl/`
- [ ] Match house style: `tracing` macros, crate `Result<()>`, `// ── Section ──` headers
- [ ] `cargo fmt`, `cargo clippy`
- [ ] Update readme example if the API changes (it won't for 2b)
- [ ] Bring numbers (see §6)

---

## 6. Measurement

This is deadline work: **the maximum matters, the mean doesn't.**

- Per-chunk service time around the completion handler — report p50 / p99 / **max**.
  Option 1's malloc looks great on p50; watch what 2b does to the tail.
- `dropped_chunks()` (`rtlsdr.rs:828`) at **3.2 MS/s**, where the cushion is only 38 ms.
  The only rate where any of this is likely to show.
- Allocation count — counting global allocator, or a counter on the fallback branch.
  Should flatline after warmup. That's the proof recycling works.
- **Run on a Raspberry Pi as well as the desktop.** At 2.048 MS/s on x86 this eliminates
  ~8 MB/s of memcpy — a fraction of a percent of memory bandwidth, invisible. It gets real
  on weak hardware and at 3.2 MS/s. "No measurable change on x86 desktop; dropped chunks
  N → 0 on a Pi 4 at 3.2 MS/s" is a far more credible PR than either number alone.

---

## 7. Build order

- [x] **0. Machine setup** — blacklist DVB modules, `librtlsdr-dev`, udev. See §0.
- [x] **1. Get sound out.** librtlsdr `read_async` → center → FM discriminator → boxcar
      decimate → ring b (placeholder `Mutex<VecDeque>`) → cpal. DSP runs in the read
      callback; cpal on its own thread. **Done — FM audio confirmed.**
- [x] **2. De-emphasis.** Most audible gap right now. Broadcast FM is pre-emphasized; add
      the 75 µs de-emphasis (one-pole IIR on the audio output) or it sounds hissy/bright.
      A few lines in the DSP callback. **Done — 50 µs, applied at 240 kS/s where it also
      serves as the stage-2 anti-alias filter (`source.rs`).**
- [x] **3. Real audio ring.** Replace `Mutex<VecDeque<f32>>` with a bounded SPSC ring
      (§3, ring b) so the audio callback never locks. Do this before adding anything else
      real-time-sensitive. **Done — `spmc.rs`, slot-based SPMC with per-consumer cursors.**
- [x] **4. UI thread**: `ratatui` + crossterm input; runtime tuning via the `rtlsdr_mt`
      `Controller` (it's `Send` — `set_center_freq` from a key handler on another thread
      while `read_async` runs). Volume/gain likewise. **Done — `tui/`, control signals over
      an `mpsc` to a dedicated controller thread. Volume is still hardcoded; see R1.7.**
- [x] **5. FFT waterfall** fed by ring c (§3, overwriting broadcast ring). Where the
      multi-cursor design earns its keep. Validate isolation: hammer the terminal (resize,
      spam input) and confirm no audio dropouts. **Done — own radix-2 FFT with twiddle
      table (`tui/fft.rs`), spectrum + waterfall (`tui/signal_view.rs`).**
- [ ] **6. Everything after that** — see **§8. Roadmap**, which supersedes what used to be
      a one-line "polish" bullet here.

Deferred / different project: adding FC0013 support to `rs-rtl` (§5) if we want the
pure-Rust path back.

---

## 8. Roadmap

Everything past step 5. Each item is sized to be taken on its own: what, why it matters,
where it lands in the tree, and what "done" looks like. **S** ≈ an evening, **M** ≈ a
weekend, **L** ≈ a project in its own right.

**Suggested order:** R2.1 first (it makes every DSP change below testable against a fixed
recording instead of against whatever is on the air right now). After that the list forks,
and the fork is a real choice:

- **Deeper on FM** — walk Tier 1 top to bottom. R1.9 → R1.10 (stereo, then RDS) is the
  highest-visibility pair in the whole list, and the WFM chain is now clean enough to
  deserve them.
- **Wider across the spectrum** — **R3.0** (mode abstraction), then R3.2 → R3.1 → R3.6.
  This is the better-value branch if the interest is in what is on the air rather than in
  FM fidelity: R3.0 is paid once and every mode after it is an evening.

Read **§8.0 first either way** — it rules out several otherwise-obvious targets on this
particular dongle.

### 8.0 What the hardware allows

The FC0013 (§ driver pivot) tunes roughly **22 – 1100 MHz** and tops out at **19.7 dB** of
tuner gain — the device reports 23 discrete values in three switched-LNA clusters around
−6, +7 and +19.7 dB, with ~11 dB gaps between them. That is not a detail: it decides which
of the obvious "what else is on the air" ideas are worth starting.

| band / idea | status on this dongle |
|---|---|
| GPS (1575 MHz) | **out of range** — not attemptable |
| GSM/LTE 1800 | **out of range** — 900 MHz downlink only |
| NDB (190 – 1750 kHz), HF | needs an upconverter |
| ADS-B (1090 MHz) | in range, but at the very top edge where this tuner is least sensitive, with half the gain an R820T offers — see R3.4 |
| 433 ISM / TPMS, 868 LoRa, PMR 446 | comfortably in range |
| **108 – 174 MHz** — airband, VOR, APT, AIS, pagers | **the tuner's sweet spot** |

The useful coincidence: the band this dongle handles best is also where the richest
decodable traffic sits. Tier 3 is ordered accordingly.

**The other limiting factor is the antenna, not the code.** The stock telescopic whip is
now the weakest link for everything in Tier 3 — a 137 MHz V-dipole or QFH is worth as much
to APT and AIS as any amount of DSP. And if decoding turns out to be the interesting part,
an R820T dongle (24 – 1766 MHz, 49.6 dB) re-opens ADS-B, GSM 1800 and comfortable 433/868
work for the price of an evening's coffee. Neither is a code change; both outrank one.

### Tier 1 — finish the receiver (P0)

Gaps in the signal path that exists today, roughly cheapest-first.

- [x] **R1.1 Window the FFT** — S — **done**
      **Why:** `Fft::push` fed a rectangular window into `dft_fwd`, so sidelobes were only
      −13 dB and rolled off at 6 dB/octave — a strong station smeared across the whole
      waterfall row. A Hann window is one multiply per sample.
      **What landed** (`tui/fft.rs`):
      - Periodic (DFT-even) Hann, `0.5·(1 − cos(2πk/N))` for `k = 0..N-1` — denominator
        `N`, not `N-1`. MATLAB's plain `hann(L)` is the *symmetric* window and is the wrong
        variant here; `hann(L,'periodic')` is this one.
      - Stored as `[f32; N]`, not `[ComplexF32; N]`: a window scales magnitude without
        rotating phase, so a complex table costs 4 mul + 2 add per sample to multiply by a
        zero imaginary part. Added `impl Mul<f32> for ComplexF32` for it.
      - Applied inside the bit-reversal copy at the top of `dft_fwd`, which already touched
        every sample — zero extra memory traffic, and `samples` keeps holding raw IQ (which
        matters if overlap is ever added).
      - `post_process` now normalises by `window_sum` (coherent gain) instead of `N`.
        Summed rather than hardcoded to `2/N`, so swapping in Hamming or Blackman-Harris
        needs no other change. Without this everything reads a flat 6 dB low.
      **Measured**, worst bin beyond ± *d* from a half-bin-offset tone:

      | beyond ± | rectangular | Hann |
      |---|---|---|
      | 4 | −23.0 dB | −48.7 dB |
      | 8 | −28.5 dB | −66.4 dB |
      | 100 | −49.9 dB | −69.0 dB ← 8-bit quantisation floor |
      | 300 | −58.2 dB | −69.0 dB |

      **Tests:** `energy_is_confined_to_the_tone` had encoded the rectangular premise; it now
      asserts the two Hann skirt bins sit at exactly −6.02 dB (the 0.25/0.5/0.25 main lobe)
      rather than merely skipping them. New `off_bin_tone_does_not_smear_across_the_display`
      is the one that actually fails without the window — every other test used a bin-centred
      tone, where a rectangular window is already near-perfect.
      **Not done — 50 % overlap, deliberately.** Overlap is not a smaller window; it is the
      same N-point window advanced by a hop of N/2. Its benefit is that a Hann window tapers
      block edges to zero, so hopping N/2 restores uniform weight to every input sample. That
      is unreachable here: `tui.rs` calls `seek_latest()` and drops ~10 blocks per frame by
      design (ring c is the overwriting broadcast ring), so coverage is already a few percent
      — preserving continuity across the 4 windows inside one block is meaningless when the
      gap *between* frames is two orders of magnitude larger. `signal_view::push` already
      takes an elementwise `max` across those 4 windows, which is peak-hold and covers the
      transient case better than overlap would. Revisit only for a path that processes every
      block contiguously (offline analysis of an R2.1 capture). Note that even for noise-floor
      smoothing, non-overlapping averaging is strictly better per CPU on a stream — overlap
      only reduces variance for a *fixed-length* record.

- [ ] **R1.2 DC blocker on I and Q** — S
      **Why:** no DC removal anywhere, so the tuner's DC offset is a permanent spike in the
      centre bin and an audible tone when tuned near it. One-pole high-pass on each of I
      and Q (`y[n] = x[n] − x[n−1] + a·y[n−1]`, a ≈ 0.999).
      **Where:** `source.rs`, right after the `u8 → f32` centring, before stage-1 decimation.
      **Done when:** the centre bin sits at the noise floor with no signal present.

- [ ] **R1.3 Surface ring health; get `eprintln!` out of the callback** — S
      **Why:** two problems, one fix. `spmc.rs` has drop/lap semantics but `source.rs`
      throws the result away (`let _ = producer_iq.write(..)`) or prints it — and
      `eprintln!` in the read callback takes the stdout lock and can block on a slow
      terminal, which is precisely the failure the three-thread split (§1) exists to
      prevent. §3's rule is "a full or empty ring is a defect to **measure**".
      **Where:** atomic counters on the producer/consumer, drained by the UI; new health row
      in `tui/stats_view.rs`.
      **Done when:** overruns, laps and cpal underruns are visible on screen, and no hot
      path formats a string.

- [ ] **R1.4 Real low-pass FIR + polyphase decimation** — M
      **Why:** `IQ_DECIM` and `POST_DECIM` are both boxcars. The first null lands where you
      want it but the stopband is only ~13 dB down, so adjacent-channel rejection is poor
      and stage 2 folds junk into the audio band. A designed FIR run polyphase (compute
      only the samples you keep) costs about the same CPU for a genuinely better radio.
      **Where:** new `dsp/` module — filter design offline, coefficients as a `const` table;
      wire into `source.rs` in place of the two accumulators.
      **Done when:** a strong neighbour 200 kHz away is inaudible, and R4.1's THD test
      improves against the boxcar baseline.

- [ ] **R1.5 Signal-quality metering (RSSI / SNR)** — S
      **Why:** `Field` shows Freq/Step/Gain/BW/PPM — every one an input, none a measurement.
      Without RSSI there is no squelch, no scanner, and no way to tell whether a gain change
      helped. Mean |IQ|² for RSSI; in-band vs out-of-band power from the FFT for SNR.
      **Where:** computed in the DSP callback, published as an atomic; rendered in
      `tui/stats_view.rs`.
      **Done when:** a dBFS bar tracks tuning across the band.

- [ ] **R1.6 Squelch** — S
      **Why:** mutes the hiss between stations; needs R1.5. Threshold with hysteresis and a
      short attack/release so it doesn't chatter on a marginal signal.
      **Where:** `source.rs` (gate the audio sample); threshold as a new `Field` + `CtrlSignal`.

- [ ] **R1.7 Volume and mute as controls** — S
      **Why:** `volume` is a hardcoded `0.3` inside the DSP closure.
      **Where:** new `CtrlSignal::Volume`, new `Field`; apply on the audio side.
      **Done when:** volume and mute are keyboard-adjustable and survive into R2.3's config.

- [ ] **R1.8 Clock-drift resampler** — M
      **Why:** the appendix already names it — the dongle's crystal and the sound card's
      crystal are independent, so ring b walks steadily toward permanent underrun or lapping.
      This is what turns "works for ten minutes" into "runs all day". Fractional resampler
      whose ratio is trimmed by a slow control loop watching ring fill level.
      **Where:** `source.rs` output stage; fill level read from the `spmc` consumer.
      **Done when:** R1.3's underrun counter still reads zero after an hour.

- [ ] **R1.9 Stereo (MPX decode)** — L
      **Why:** the biggest audible upgrade available, and the gateway to R1.10 since both
      subcarriers live in the same multiplex. Currently mono by construction: de-emphasis at
      240 kS/s deliberately buries the 38 kHz subcarrier before it can alias.
      **How:** keep a wideband path; PLL-lock the 19 kHz pilot; square it for a coherent
      38 kHz carrier; mix down L−R; matrix with L+R; de-emphasize each channel *after* the
      matrix. cpal is already handed a multi-channel config — `speaker.rs` just writes the
      same sample to every channel today.
      **Where:** `source.rs` restructure + new `dsp/pll.rs`; `speaker.rs` for true L/R.
      **Done when:** a stereo broadcast has real separation and the pilot indicator lights.

- [ ] **R1.10 RDS** — L
      **Why:** station name, radiotext, PI code, clock. The most satisfying payoff in the
      project — the TUI stops being a waterfall and starts naming what you're listening to.
      The 57 kHz subcarrier is 3× the pilot, so R1.9's PLL hands you the carrier for free.
      **How:** BPSK at 1187.5 bps → matched filter → timing recovery → differential decode →
      block sync via the offset words/syndromes → group decode (0A = PS, 2A = radiotext).
      **Where:** new `rds/` module; a display panel in `tui/`.
      **Done when:** the station name appears and stays stable. The block decoder is pure
      logic over a bit stream — test it against captured bits, no hardware needed.

### Tier 2 — make it a radio you'd actually use (P1)

- [ ] **R2.1 IQ record + file source** — M — *do this first*
      **Why:** double value. It's a feature, and it's the test harness for all of Tier 1 —
      develop DSP with no dongle, against reproducible input, including that one weak
      station that only misbehaves at night. Use **SigMF** (JSON sidecar + raw samples) so
      the captures interoperate with GNU Radio, `inspectrum`, etc.
      **Where:** trait-ify `Source` so `RtlSource` and `FileSource` are interchangeable;
      recording is just another ring-c consumer — exactly what the multi-cursor design in
      §3 was built for.
      **Done when:** `cargo run -- --replay capture.sigmf` produces audio with no hardware.

- [ ] **R2.2 CLI arguments** — S
      **Why:** frequency, sample rate and ring geometry are compile-time constants in
      `main.rs`; changing station means a rebuild. No arg parser in `Cargo.toml` yet — add
      `clap`.

- [ ] **R2.3 Config file + presets** — S
      **Why:** last frequency, gain, PPM, volume, and a named station list in a TOML under
      the XDG config dir. Presets on the number keys.

- [ ] **R2.4 Scan / seek** — M
      **Why:** you already compute the spectrum every frame. Sweep the band, threshold on
      R1.5's RSSI, collapse adjacent peaks into stations, populate a browsable list; plus
      seek-up/seek-down on a keypress. Another ring-c consumer.

- [ ] **R2.5 Audio recording and timeshift** — S
      **Why:** dump the demodulated audio to WAV; a pause/rewind buffer is the same ring with
      a lagging cursor.

### Tier 3 — new modes and decoders (P2)

Each is mostly a new demodulator behind the same pipeline. **R3.0 is the gate** — until it
exists every item below re-pays the same cost, and after it they are nearly free. Order
here is deliberate rather than by curiosity, because the hardware (§8.0) makes some of
these much better first projects than others.

**Do R2.1 before any of them.** A decoder debugged against whatever happens to be
transmitting is not debuggable; a decoder debugged against a saved capture is.

- [ ] **R3.0 Mode abstraction** — M — *the gate for everything below.*
      **Why:** R3.1 and R3.2 read as **S** because the DSP genuinely is trivial — AM is
      `sqrt(i² + q²)` plus a DC block, NBFM is the existing discriminator with a different
      deviation. But there is nowhere to put them. `DSPFlow` (`source/dsp.rs`) is welded to
      the WFM plan: const-generic buffer sizes for ÷4/÷2, a 4/25 resampler, demod pinned at
      300 kHz, a 164-sample output block, 100 kHz channel filters. `AUDIO_DECIM = 50` pins
      the device rate to the audio rate on top of that.
      **What:** give a mode ownership of its own chain — channel filter, IF rate,
      demodulator, audio rate — behind one trait, and let `TunerMode` in
      `tui/app_states.rs` actually select it. That enum already declares
      `WbFm/Nfm/Am/Usb/Lsb/Raw` and is entirely unused; this is the piece that was sketched
      and then skipped.
      **Note:** overlaps R1.4 substantially — the polyphase decimation work wants doing
      once, generically, not twice.
      **Done when:** WFM still sounds identical, and NBFM is a new impl rather than a new
      branch inside `DSPFlow`.

- [ ] **R3.2 AM / airband** — S *(after R3.0)* — 118–137 MHz, squarely in the tuner's best
      band. Envelope detection is simpler than anything already built here; the work is a
      proper AGC. **Best reward-per-line on the list** — it opens an entire band of live
      traffic (tower, approach) for a demodulator that is three lines of arithmetic.
- [ ] **R3.1 NBFM** — S *(after R3.0)* — 12.5/25 kHz channels: ham, PMR, marine voice.
      Reuses the existing discriminator with a different deviation and filter width.
- [ ] **R3.6 POCSAG / FLEX pagers** — M — **the first decoder to attempt.** POCSAG is FSK,
      which means it is the FM discriminator already in the tree plus a bit slicer, frame
      sync and CRC. Slow (512 / 1200 / 2400 baud), cleartext, thoroughly documented, and in
      the sweet-spot band. It reuses more of this codebase than anything else here.
      AIS (162 MHz) and ACARS (131 MHz, needs R3.2) follow the same shape.
- [ ] **R3.5 NOAA APT (137 MHz)** — L — **the trophy**: a real satellite image out of the
      air. In band, and the demod is NBFM plus a 2400 Hz subcarrier. The cost is outside the
      DSP — a pass predictor, Doppler correction, and an antenna that is not the stock whip.
- [ ] **R3.3 SSB / CW** — M — Weaver or Hilbert-transform demodulator plus a fine-tuning
      control. Opens up HF, but only with an upconverter (§8.0).
- [ ] **R3.4 ADS-B (1090 MHz)** — L — **deprioritised on this hardware.** Preamble
      correlator → PPM bit slicer → CRC → CPR position decode, and the most "wow" item on
      the list. But 1090 MHz sits at the top edge of the FC0013's range where it is least
      sensitive, with ~half an R820T's gain, and it wants its own antenna. The protocol is
      not the hard part here; the front end is. Revisit with an R820T — with one, this
      moves to the top of the tier.
- [ ] **R3.7 433 MHz ISM / TPMS / OOK** — M — car fobs, weather stations, doorbells, tyre
      pressure sensors. In range, mostly cleartext OOK/FSK, and a good generic bit-pattern
      viewer is reusable across all of them. Bursty, so it pairs naturally with R2.1's
      recorder and the R2.4 scanner.

### Tier 4 — engineering depth (P1, runs alongside everything)

- [ ] **R4.1 Golden tests for the DSP chain** — M
      **Why:** `tui/fft.rs` has real tests; the demodulator has none, and R1.4/R1.9 are large
      refactors of exactly that code. Synthesize an FM-modulated tone in Rust, run it through
      the chain, assert recovered frequency, amplitude and THD. Do this *before* R1.4, not
      after — it is what makes the filter swap safe.

- [ ] **R4.2 Faster FFT + per-stage benchmarks** — M
      **Why:** the FFT is a naive complex radix-2; the input is complex so a real-input
      optimisation doesn't apply directly, but split-radix, in-place stages and `f32x4` SIMD
      all do. You have `benches/ring_vs_mutex`; add per-stage DSP benches so the tail
      latency claims in §6 are measured rather than asserted.

- [ ] **R4.3 Run on a Raspberry Pi** — S
      **Why:** §6 already argues it. On x86 none of this is visible; at 3.2 MS/s on a Pi 4 the
      numbers get real, and "dropped chunks N → 0" is worth more than any p50.

- [ ] **R4.4 Network** — M
      **Why:** `rtl_tcp` **client** support means the dongle can live on a Pi by the window
      while the UI runs on the desktop — and it's a third `Source` implementation, so R2.1's
      trait pays off twice. Streaming the demodulated audio out over HTTP/Icecast is the
      other half.

---

## Appendix: things worth remembering

- **SDR races a clock; HFT races other people.** SDR/audio is deadline-driven — there's a
  threshold, and no prize for being early, so you optimize the *maximum* and will trade
  average throughput to shrink the tail. HFT is latency-competitive — no bell, just a
  ranking, so the *mean* is money. Shared toolbox (preallocate, no malloc in the hot path,
  lock-free SPSC, cache awareness, pinning), different objective function. The true
  siblings of this project are audio plugins (VST), robotics control loops, avionics.
- **The `sk_buff` analogy is exact.** The kernel preallocates and recycles packet
  buffers from page pools — no driver allocates per packet in the fast path. `read()`
  copies to userspace, and that copy is what decouples kernel and application buffer
  lifetimes. Kernel-bypass paths (AF_XDP, `PACKET_MMAP`, io_uring registered buffers,
  DPDK) drop the copy by mmapping a region once at setup and cycling ownership through
  explicit rings. **2b/2c are the `read()` model; option 3 is the kernel-bypass model.**
  Both are standard; the first is the safe default, the second is what you reach for when
  the copy becomes the bottleneck. Note 2c is *literally* `read(&mut [u8]) -> usize`.
- **Three independent clocks** in this system: the dongle's 28.8 MHz crystal (defines the
  true sample rate), the sound card's crystal (defines playback rate), and the CPU. None
  are exactly nominal, none are synchronized. The rings absorb the mismatch; a resampler
  eventually has to correct the drift.
