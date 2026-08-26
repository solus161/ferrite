use std::{array, thread::{self, JoinHandle}, usize};
use std::sync::mpsc::{channel, Receiver};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rtlsdr_mt::{Controller, Reader};

use sdr_core::{exceptions::CustomError, spmc::RingProducer};
use sdr_core::control_signal::CtrlSignal;

use sdr_core::dsp::{center_iq, IqDcBlocker};

/// RTL sample rate = audio sample rate × this. 50 keeps both 48k and 44.1k in
/// the RTL's valid range (2.4 MS/s and 2.205 MS/s). Also the boxcar decimation
/// factor — see the DSP callback.
pub const AUDIO_DECIM: u32 = 50;

/// Decimation stage 1, 2.4 MS/s -> 240 kS/s. Runs on I and Q *before* the
/// discriminator: 240 kHz is the lowest rate that still holds a 200 kHz FM
/// channel, and the boxcar's first null lands exactly there.
const IQ_DECIM: u32 = 10;

/// Decimation stage 2, 240 kS/s -> 48 kHz, on the demodulated signal.
const POST_DECIM: u32 = AUDIO_DECIM/IQ_DECIM;

/// De-emphasis time constant. 50 µs everywhere except the Americas and South
/// Korea, which use 75 µs. Broadcast FM pre-emphasises treble by up to ~14 dB
/// at 15 kHz; undoing it is what removes the hiss, because the discriminator's
/// own noise floor rises at 6 dB/octave and this cuts it back down.
const DEEMPHASIS_TAU: f32 = 50e-6;

/// Peak frequency deviation of broadcast FM, in Hz. This is what "fully
/// modulated" means for the discriminator, and therefore what maps to ±1.0 at
/// the output — the discriminator's own ±π range corresponds to ±fs/2, which no
/// broadcast signal comes anywhere near.
const FM_DEVIATION: f32 = 75_000.0; const RING_SLOTS: usize = 16;
const RING_BLOCK: usize = 512;
pub const IQ_SLOTS: usize = 16;
pub const IQ_BLOCK: usize = 16384;
pub const COMPLEX_BLOCK: usize = IQ_BLOCK/2;

pub struct Source {
    /// rtl lib
    // ctl: Controller,
    reader: Reader,

    /// Handle for control signal threat
    ctrl_handle: JoinHandle<()>,

    sample_rate: u32,
    center_freq: Arc<AtomicU32>,
    tuned_freq: Arc<AtomicU32>,
}

impl Source {
    pub fn new(sample_rate: u32, center_freq: u32, bandwidth: u32, ctrl_rx: Receiver<CtrlSignal>) -> Self {
        // ── SDR setup (librtlsdr via rtlsdr_mt) ─────────────────────────────────
        let (mut ctl, reader) = rtlsdr_mt::open(0)
            .expect(&CustomError::RtlOpenDevice(0).to_string());
        ctl.set_center_freq(center_freq)
            .expect(&CustomError::RtlSetFreq(center_freq).to_string());

        // This is for listening mode only
        ctl.set_bandwidth(bandwidth)
            .expect(&CustomError::RtlSetBandwidth(bandwidth).to_string());
        ctl.set_sample_rate(sample_rate)
            .expect(&CustomError::RtlSetSampleRate(sample_rate).to_string());
        // FC0013: auto gain for first sound
        ctl.enable_agc().expect(&CustomError::RtlEnableAgc.to_string());
        
        // Flush stale USB buffers before streaming
        ctl.reset_buffer().expect(&CustomError::RtlResetBuffer.to_string());

        let sample_rate = ctl.sample_rate();

        let tuned_freq = Arc::new(AtomicU32::new(center_freq));
        let center_freq = Arc::new(AtomicU32::new(center_freq));
        
        
        // Start a thread to receive control signal
        // better than checking within read_async 
        let center_freq_clone = center_freq.clone();
        let ctrl_handle = thread::spawn(move || {
            while let Ok(sig) = ctrl_rx.recv() {
                // TODO: handle error, should not expect()
                match sig {
                    CtrlSignal::CenterHz(freq) => {
                        ctl.set_center_freq(freq).expect(&CustomError::RtlSetFreq(freq).to_string());
                        center_freq_clone.store(freq, Ordering::Release);
                        // ctl.reset_buffer().expect(&CustomError::RtlResetBuffer.to_string());
                    },
                    CtrlSignal::Bandwidth(bw) => ctl.set_bandwidth(bw)
                        .expect(&CustomError::RtlSetBandwidth(bw).to_string()),
                    // librtlsdr takes tenths of a dB, and only the discrete
                    // values `tuner_gains()` reports are legal — it snaps to the
                    // nearest rather than failing. Also silently disables AGC.
                    CtrlSignal::Gain(db) => ctl.set_tuner_gain(db as i32 * 10)
                        .expect(&CustomError::RtlSetGain(db).to_string()),
                    CtrlSignal::Ppm(ppm) => ctl.set_ppm(ppm as i32)
                        .expect(&CustomError::RtlSetPpm(ppm).to_string()),
                    CtrlSignal::Quit => {
                        ctl.cancel_async_read();
                        break
                    },
                }
            }
        });
       
        // TODO: let skip tuned_freq for now
        Self { reader, ctrl_handle, sample_rate, center_freq, tuned_freq }

    }

    /// Start receiving from USB, send the self to another thread
    /// so this will not block UI
    pub fn start_receive(
        mut self,
        mut producer: RingProducer<f32, IQ_SLOTS, IQ_BLOCK>,
    ) -> Result<(JoinHandle<()>, JoinHandle<()>), CustomError> {
        // ── DSP: runs inside librtlsdr's async read callback ────────────────────
        // read_async blocks this (main) thread and invokes the closure per USB
        // buffer, on this same thread. cpal's callback runs on its own thread, so
        // only finished audio crosses the ring.
        let (mut iacc, mut qacc) = (0.0f32, 0.0f32); // stage 1 boxcar, on the IQ
        let mut iq_n = 0u32;
        let (mut prev_i, mut prev_q) = (0.0f32, 0.0f32); // FM discriminator state
        let mut acc = 0.0f32; // stage 2 boxcar, on the demodulated signal
        let mut acc_n = 0u32;
        let mut deemph = 0.0f32; // one-pole de-emphasis state
        let volume = 0.3f32;

        // Ask the device for its rate rather than trusting the requested one:
        // librtlsdr snaps to what the 28.8 MHz crystal can actually divide down
        // to, so the two are close but not equal.
        let iq_rate = self.sample_rate as f32 / IQ_DECIM as f32;

        // Phase step at full deviation, at the rate the discriminator now runs.
        // 2π·75k/240k = 1.96 rad — still under π, but that shrinking headroom is
        // why stage 1 cannot decimate much further than 240 kS/s. Normalising by
        // π instead would treat the deviation as ±fs/2 and lose ~24 dB.
        let full_scale = 2.0 * std::f32::consts::PI * FM_DEVIATION / iq_rate;

        // One-pole RC lowpass; alpha = 1 - exp(-T/tau) is the step-invariant
        // discretisation of the analogue de-emphasis network.
        let deemph_a = 1.0 - (-1.0 / (iq_rate * DEEMPHASIS_TAU)).exp();

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

        // High-pass filter
        let mut highpass_filter = IqDcBlocker::<IQ_BLOCK>::new(self.sample_rate);

        let mut buf: [f32; IQ_BLOCK] = array::from_fn(|_| 0.0f32 );
        let builder = thread::Builder::new().name("thread-sdr".to_string());
        let handle = builder.spawn(move || {
            self.reader
                .read_async(15, IQ_BLOCK as u32, move |bytes| {
                    // bytes: interleaved u8 IQ [I0,Q0,I1,Q1,...] at rtl_rate
                    // Feed cpal first
                    for (idx, pair) in bytes.chunks_exact(2).enumerate() {
                        // u8 → centered f32 in ~[-1, 1]
                        let (i, q) = center_iq(pair[0], pair[1]);

                        // Apply high-pass filter
                        let (i, q) = highpass_filter.process(i, q);
                        
                        (buf[idx], buf[idx + 1]) = (i, q);
                    };
                    let _ = producer.write(&buf);
                    producer.wake();
                })
                .expect("rtlsdr: read_async failed");
            }).unwrap();

        Ok((handle, self.ctrl_handle))
    }
}
