use std::{sync::Arc, thread::{self, JoinHandle}};
use std::sync::mpsc::{channel, Receiver};

use rtlsdr_mt::{Controller, Reader};

use super::utils::{IqDcBlocker, FmDiscriminator, AmEnvelope, Deemphasis};

use crate::{exceptions::CustomError, source::utils::Decim, spmc::RingProducer};
use crate::tui::control_signal::CtrlSignal;

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
const FM_DEVIATION: f32 = 75_000.0;
const RING_SLOTS: usize = 16;
const RING_BLOCK: usize = 512;
const IQ_SLOTS: usize = 16;
const IQ_BLOCK: usize = 16384;

pub struct Source {
    /// rtl lib
    reader: Reader,

    /// Handle for control signal threat
    ctrl_handle: JoinHandle<()>,

    sample_rate: u32,
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
        
        // Start a thread to receive control signal
        // better than checking within read_async 
        let ctrl_handle = thread::spawn(move || {
            while let Ok(sig) = ctrl_rx.recv() {
                // TODO: handle error, should not expect()
                match sig {
                    CtrlSignal::CenterHz(freq) => {
                        ctl.set_center_freq(freq).expect(&CustomError::RtlSetFreq(freq).to_string());
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
       
        Self { reader, ctrl_handle, sample_rate }

    }

    /// Start receiving from USB, send the self to another thread
    /// so this will not block UI
    pub fn start_receive(
        mut self,
        mut producer_sp: RingProducer<f32, RING_SLOTS, RING_BLOCK>,
        mut producer_iq: RingProducer<u8, IQ_SLOTS, IQ_BLOCK>,
    ) -> Result<(JoinHandle<()>, JoinHandle<()>), CustomError> {
        // ── DSP: runs inside librtlsdr's async read callback ────────────────────
        // read_async blocks this (main) thread and invokes the closure per USB
        // buffer, on this same thread. cpal's callback runs on its own thread, so
        // only finished audio crosses the ring.
        let volume = 0.3f32;

        // Ask the device for its rate rather than trusting the requested one:
        // librtlsdr snaps to what the 28.8 MHz crystal can actually divide down
        // to, so the two are close but not equal.
        let iq_rate = self.sample_rate as f32 / IQ_DECIM as f32;

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
        //
        // High-pass filter for this
        let mut highpass_filter = IqDcBlocker::new();

        // For decimation
        let mut decim = Decim::<IQ_DECIM, POST_DECIM>::new();
        
        // For Demphasis
        let mut deemp = Deemphasis::new(iq_rate, DEEMPHASIS_TAU);

        // FM and AM demod
        let mut fm_demod = FmDiscriminator::new(iq_rate, FM_DEVIATION);
        let mut am_demod = AmEnvelope::new(iq_rate, 20.0);

        // ── TEMPORARY INSTRUMENTATION — delete once the silence is diagnosed ──
        // Goes to a file, not stderr: ratatui owns the terminal. Watch with
        //     tail -f /tmp/ferrite-dsp.log
        let mut dbg_log = std::fs::File::create("/tmp/ferrite-dsp.log").ok();
        if let Some(f) = dbg_log.as_mut() {
            use std::io::Write;
            let _ = writeln!(
                f,
                "sample_rate={} iq_rate={} full_scale={} deemph_alpha={}",
                self.sample_rate,
                iq_rate,
                2.0 * std::f32::consts::PI * FM_DEVIATION / iq_rate,
                1.0 - (-1.0 / (iq_rate * DEEMPHASIS_TAU)).exp(),
            );
        }
        let mut dbg_n = 0u64;
        let mut dbg_peak = 0.0f32;
        let mut dbg_sumsq = 0.0f64;
        let mut dbg_err = 0u64;
        let mut dbg_at = std::time::Instant::now();

        let handle = thread::spawn(move || {
            self.reader
                .read_async(15, 16384, move |bytes| {
                    // bytes: interleaved u8 IQ [I0,Q0,I1,Q1,...] at rtl_rate
                    // Feed cpal first
                    for pair in bytes.chunks_exact(2) {
                        // u8 → centered f32 in ~[-1, 1]
                        let i = (pair[0] as f32 - 127.5) * (1.0 / 127.5);
                        let q = (pair[1] as f32 - 127.5) * (1.0 / 127.5);
                        
                        // Apply the high-pass filter
                        let (i, q) = highpass_filter.process(i, q);

                        // First phase of decimation
                        let Some((i, q)) = decim.iq_decim(i, q) else {
                            continue;
                        };

                        // FM demod at 240 kS/s: angle of cur × conj(prev).
                        // Instantaneous frequency.
                        let demod = fm_demod.process(i, q);
                        let audio = deemp.process(demod);

                        // Stage 2 — decimate 240 kS/s → audio_rate.
                        let Some(sample) = decim.post_decim(audio, volume) else {
                            continue;
                        };
                        if producer_sp.push(sample).is_err() {
                            dbg_err += 1;
                        }

                        // TEMPORARY
                        dbg_n += 1;
                        dbg_peak = dbg_peak.max(sample.abs());
                        dbg_sumsq += (sample as f64) * (sample as f64);
                    };

                    // ── TEMPORARY INSTRUMENTATION ────────────────────────────
                    if dbg_at.elapsed() >= std::time::Duration::from_secs(1) {
                        use std::io::Write;
                        let rms = (dbg_sumsq / dbg_n.max(1) as f64).sqrt();
                        if let Some(f) = dbg_log.as_mut() {
                            let _ = writeln!(
                                f,
                                "{:>6} samples/s   peak {:.5}   rms {:.6}   push_err {}",
                                dbg_n, dbg_peak, rms, dbg_err
                            );
                        }
                        dbg_n = 0;
                        dbg_peak = 0.0;
                        dbg_sumsq = 0.0;
                        dbg_err = 0;
                        dbg_at = std::time::Instant::now();
                    }

                    // Feed TUI the raw bytes: one USB buffer is exactly one
                    // IQ_BLOCK, and centering is the FFT's job — doing it here
                    // would only move work into this real-time callback.
                    // TODO: silenced error
                    let _ = producer_iq.write(bytes);
                })
                .expect("rtlsdr: read_async failed");
            });

        Ok((handle, self.ctrl_handle))
    }
}
