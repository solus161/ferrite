use rtlsdr_mt::{Controller, Reader};

use crate::{exceptions::CustomError, spmc::RingProducer};

/// RTL sample rate = audio sample rate × this. 50 keeps both 48k and 44.1k in
/// the RTL's valid range (2.4 MS/s and 2.205 MS/s). Also the boxcar decimation
/// factor — see the DSP callback.
pub const AUDIO_DECIM: u32 = 50;

pub struct Source<const RING_SLOT: usize, const RING_BLOCK: usize> {
    ctl: Controller,
    reader: Reader,
}

impl<const RING_SLOT: usize, const RING_BLOCK: usize> Source<RING_SLOT, RING_BLOCK> {
    pub fn new(sample_rate: u32, center_freq: u32) -> Self {
        // ── SDR setup (librtlsdr via rtlsdr_mt) ─────────────────────────────────
        let (mut ctl, reader) = rtlsdr_mt::open(0).expect("rtlsdr: failed to open device 0");
        ctl.set_center_freq(center_freq)
            .expect("rtlsdr: set_center_freq failed");
        ctl.set_sample_rate(sample_rate)
            .expect("rtlsdr: set_sample_rate failed");
        ctl.enable_agc().expect("rtlsdr: enable_agc failed"); // FC0013: auto gain for first sound
        ctl.reset_buffer().expect("rtlsdr: reset_buffer failed"); // flush stale USB buffers before streaming

        Self { ctl, reader }
    }

    pub fn receive(
        &mut self,
        mut producer: RingProducer<f32, RING_SLOT, RING_BLOCK>,
    ) -> Result<(), CustomError> {
        // ── DSP: runs inside librtlsdr's async read callback ────────────────────
        // read_async blocks this (main) thread and invokes the closure per USB
        // buffer, on this same thread. cpal's callback runs on its own thread, so
        // only finished audio crosses the ring.
        let (mut prev_i, mut prev_q) = (0.0f32, 0.0f32); // FM discriminator state
        let mut acc = 0.0f32; // boxcar accumulator for decimate-by-AUDIO_DECIM
        let mut acc_n = 0u32;
        let volume = 0.3f32;

        // Samples go straight into the open ring slot via `push`, which publishes
        // once RING_BLOCK of them have accumulated. No staging array: this producer
        // computes one sample at a time, so `write` would mean filling a local
        // buffer and then copying the whole thing in.
        //
        // One USB buffer yields 16384/2/AUDIO_DECIM ≈ 164 samples, so a block fills
        // every ~3.1 callbacks. The 164 is fractional, so block edges never align
        // with transfer edges — the fill position lives in the producer and the
        // decimation accumulator carries across callbacks, which is what makes that
        // harmless.
        self.reader
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

                        if let Err(e) = producer.push(sample) {
                            eprintln!("ring push failed: {e:?}");
                        }
                    }
                }
            })
            .expect("rtlsdr: read_async failed");

        Ok(())
    }
}
