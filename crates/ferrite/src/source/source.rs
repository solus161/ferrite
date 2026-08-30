use std::{array, thread::{self, JoinHandle}};
use std::sync::mpsc::Receiver;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rtlsdr_mt::Reader;

use sdr_core::{exceptions::CustomError, spmc::RingProducer};
use sdr_core::control_signal::CtrlSignal;

use sdr_core::dsp::center_iq;

use super::dsp::DSPFlow;

/// RTL sample rate = audio sample rate × this. 50 keeps both 48k and 44.1k in
/// the RTL's valid range (2.4 MS/s and 2.205 MS/s). The DSP chain realises it
/// as 4 × 2 × 25/4 — see [`DSPFlow`].
pub const AUDIO_DECIM: u32 = 50;

pub const CPAL_BLOCK: usize = 164;  // ceil(8192 / 50) — the block is 163 or 164
pub const IQ_SLOTS: usize = 16;
pub const IQ_BLOCK: usize = 16384;

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
        mut producer_sp: RingProducer<f32, IQ_SLOTS, CPAL_BLOCK>,
        mut producer_fft: RingProducer<f32, IQ_SLOTS, IQ_BLOCK>,
    ) -> Result<(JoinHandle<()>, JoinHandle<()>), CustomError> {
        // ── DSP: runs inside librtlsdr's async read callback ────────────────────
        // read_async blocks this (main) thread and invokes the closure per USB
        // buffer, on this same thread. cpal's callback runs on its own thread, so
        // only finished audio crosses the ring.
        //
        // Audio goes into the ring one sample at a time via `push`, which
        // publishes once CPAL_BLOCK of them have accumulated. No staging array:
        // a block is 163 or 164 samples so it never aligns with a USB transfer,
        // and `push` keeps the fill position in the producer — which is exactly
        // what makes the misalignment harmless.

        // TODO: an IqDcBlocker here measured 57→62 dB SNR at a = 0.99999.
        // let mut highpass_filter = IqDcBlocker::<IQ_BLOCK>::new(self.sample_rate);

        // Multi phase decimator
        let mut dsp = DSPFlow::new_boxed();

        let mut buf: [f32; IQ_BLOCK] = array::from_fn(|_| 0.0f32);
        let mut buf_i: [f32; 8192] = array::from_fn(|_| 0.0f32);
        let mut buf_q: [f32; 8192] = array::from_fn(|_| 0.0f32);

        let builder = thread::Builder::new().stack_size(8 << 20)
            .name("thread-sdr".to_string());
        let handle = builder.spawn(move || {
            self.reader
                .read_async(15, IQ_BLOCK as u32, move |bytes| {
                    // bytes: interleaved u8 IQ [I0,Q0,I1,Q1,...] at rtl_rate
                    // DSP right here
                    for (idx, pair) in bytes.chunks_exact(2).enumerate() {
                        // u8 → centered f32 in ~[-1, 1]
                        let (i, q) = center_iq(pair[0], pair[1]);
                        (buf[2 * idx], buf[2 * idx + 1]) = (i, q);
                        buf_i[idx] = i;
                        buf_q[idx] = q;
                    };
                    // 2.4 MHz -> 300 kHz, demodulate, resample to 48 kHz.
                    // Returns 163 or 164 live samples.
                    let n = dsp.process(&buf_i, &buf_q);

                    // Feed speaker
                    dsp.out[..n].iter().for_each(|x| {
                        let _ = producer_sp.push(*x);
                    });

                    // Feed FFT
                    let _ = producer_fft.write(&buf);
                })
                .expect("rtlsdr: read_async failed");
            }).unwrap();

        Ok((handle, self.ctrl_handle))
    }
}
