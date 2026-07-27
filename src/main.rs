
mod spmc;
mod buffer;
mod cpal;
mod exceptions;
mod utils;

use spmc::{RingProducer};

use crate::cpal::{Cpal, AUDIO_DECIM};

/// Ring geometry: RING_SLOTS buffers of RING_BLOCK f32 audio samples each.
/// RING_BLOCK is also the staging size in the DSP callback — `Buffer::write`
/// takes exactly M elements, so the two must not drift apart.
///
/// Sized to the *smallest* consumer, not the largest. A block only becomes
/// readable once it is full, so RING_BLOCK is a latency floor paid by every
/// consumer: 512 @ 48 kHz = ~10.7 ms per block, ~171 ms for the whole ring.
/// Consumers wanting a larger window (an FFT, say) accumulate blocks on their
/// own side — that is cheap, whereas splitting a large block down to cpal's
/// ~512-frame callbacks would mean holding a borrow into a ring slot across
/// dozens of real-time callbacks.
const RING_SLOTS: usize = 16;
const RING_BLOCK: usize = 512;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Device discovery ────────────────────────────────────────────────────
    println!("===RTL-SDR devices===");
    for (i, name) in rtlsdr_mt::devices().enumerate() {
        println!("#{i}: {}", name.to_string_lossy());
    }

    // Ring producer
    // Non-blocking: a slow DSP consumer must never stall the USB callback, so
    // the ring overwrites and drags the reader forward to the freshest data.
    let mut producer = RingProducer::<f32, RING_SLOTS, RING_BLOCK>::new(false);

    // A consumer for cpal
    let consumer_cpal = producer.subscribe();

    // cpal itself
    let cpal = Cpal::new(consumer_cpal);

    // ── SDR setup (librtlsdr via rtlsdr_mt) ─────────────────────────────────
    let (mut ctl, mut reader) =
        rtlsdr_mt::open(0).map_err(|_| "rtlsdr: failed to open device 0")?;
    ctl.set_center_freq(100_000_000)
        .map_err(|_| "rtlsdr: set_center_freq failed")?; // 100.0 MHz — set to a strong local station
    ctl.set_sample_rate(cpal.rtl_rate)
        .map_err(|_| "rtlsdr: set_sample_rate failed")?;
    ctl.enable_agc()
        .map_err(|_| "rtlsdr: enable_agc failed")?; // FC0013: auto gain for first sound
    ctl.reset_buffer()
        .map_err(|_| "rtlsdr: reset_buffer failed")?; // flush stale USB buffers before streaming

    cpal.play()?;

    // ── DSP: runs inside librtlsdr's async read callback ────────────────────
    // read_async blocks this (main) thread and invokes the closure per USB
    // buffer, on this same thread. cpal's callback runs on its own thread, so
    // only finished audio crosses the ring.
    let (mut prev_i, mut prev_q) = (0.0f32, 0.0f32); // FM discriminator state
    let mut acc = 0.0f32; // boxcar accumulator for decimate-by-AUDIO_DECIM
    let mut acc_n = 0u32;
    let volume = 0.3f32;

    // Ring slots are fixed size and `Buffer::write` rejects anything that is not
    // exactly RING_BLOCK long, so stage samples here and hand over a full block.
    // One USB buffer yields 16384/2/AUDIO_DECIM ≈ 164 samples, so a 512-sample
    // block fills every ~3.1 callbacks. The 164 is fractional, so block edges
    // never align with transfer edges — the decimation accumulator carries
    // across callbacks, which is what makes that harmless.
    let mut block = [0.0f32; RING_BLOCK];
    let mut block_n = 0usize;

    reader
        .read_async(15, 16384, move |bytes| {
            // bytes: interleaved u8 IQ [I0,Q0,I1,Q1,...] at rtl_rate
            for pair in bytes.chunks_exact(2) {
                // u8 → centered f32 in ~[-1, 1]
                let i = (pair[0] as f32 - 127.5) * (1.0 / 127.5);
                let q = (pair[1] as f32 - 127.5) * (1.0 / 127.5);

                // FM demod: angle of cur × conj(prev). Instantaneous frequency.
                let re = i * prev_i + q * prev_q;
                let im = q * prev_i - i * prev_q;
                let demod = im.atan2(re); // ∈ [-π, π]
                prev_i = i;
                prev_q = q;

                // Decimate rtl_rate → audio_rate by averaging AUDIO_DECIM samples.
                acc += demod;
                acc_n += 1;
                if acc_n == AUDIO_DECIM {
                    let sample =
                        (acc / AUDIO_DECIM as f32) * (1.0 / std::f32::consts::PI) * volume;
                    acc = 0.0;
                    acc_n = 0;

                    block[block_n] = sample;
                    block_n += 1;
                    if block_n == RING_BLOCK {
                        block_n = 0;
                        if let Err(e) = producer.write(&block) {
                            eprintln!("ring write failed: {e:?}");
                        }
                    }
                }
            }
        })
        .map_err(|_| "rtlsdr: read_async failed")?;

    Ok(())
}
