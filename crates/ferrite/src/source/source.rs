use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::{
    array,
    thread::{self, JoinHandle},
};

use rtlsdr_mt::Reader;

use sdr_core::control_signal::CtrlSignal;
use sdr_core::{
    dsp::{IqDcBlocker, Xlator},
    exceptions::CustomError,
    spmc::RingProducer,
};

use sdr_core::dsp::center_iq;

use super::dsp::DSPFlow;

/// RTL sample rate = audio sample rate × this. 50 keeps both 48k and 44.1k in
/// the RTL's valid range (2.4 MS/s and 2.205 MS/s). The DSP chain realises it
/// as 4 × 2 × 25/4 — see [`DSPFlow`].
pub const AUDIO_DECIM: u32 = 50;

pub const CPAL_BLOCK: usize = 164; // ceil(8192 / 50) — the block is 163 or 164
pub const IQ_SLOTS: usize = 16;
pub const IQ_BLOCK: usize = 16384;

/// Offset tuning: how far *below* the wanted station the LO is parked.
///
/// The dongle's LO leakage and I/Q imbalance put a large spike at 0 Hz, which
/// is exactly on the carrier when tuned on-channel. Parking the LO low instead
/// puts the station at `+OFFSET_TUNING_HZ` and leaves the spike at DC; the
/// [`Xlator`] then shifts everything down so the station lands at 0 and the
/// spike is thrown out to `-OFFSET_TUNING_HZ`, where the decimation filters
/// bury it.
///
/// 350 kHz is not arbitrary. The spike has to survive two decimations without
/// folding back onto the channel, and where it lands depends on the offset:
///
/// | offset | worst-case rejection | note                        |
/// |--------|----------------------|-----------------------------|
/// | 250 kHz|      -127 dB         |                             |
/// | 350 kHz|      -152 dB         | chosen                      |
/// | 450 kHz|      -118 dB         |                             |
/// | 500 kHz|       -71 dB         | folds into stage 2 passband |
/// | 600 kHz|       -70 dB         | fs/4: folds exactly onto DC |
///
/// Anything near fs/4 is the worst possible choice — the spike aliases straight
/// back to 0 Hz on the stage-1 decimation, undoing the whole exercise.
pub const OFFSET_TUNING_HZ: u32 = 350_000;

/// The tuner's IF filter width for a given channel width.
///
/// The filter is centred on the LO, which offset tuning parks
/// [`OFFSET_TUNING_HZ`] *below* the station, so it has to reach past the offset
/// to the far edge of the channel rather than merely span the channel:
/// `2 × (350k + 150k)` = 1.0 MHz for a 300 kHz channel. Both the initial setup
/// and the retune path go through here, because applying a channel width raw
/// sets a filter narrower than the offset and mutes the radio.
fn tuner_bandwidth(channel_hz: u32) -> u32 {
    2 * (OFFSET_TUNING_HZ + channel_hz / 2)
}

/// Snap a request in librtlsdr's tenths of a dB to the nearest gain the tuner
/// actually supports.
///
/// The supported set is discrete and tuner-specific (an R820T offers 29 steps
/// from 0 to 49.6 dB, an FC0013 23 in three clusters with ~11 dB gaps), so an
/// arbitrary value is not generally settable. librtlsdr does not reject an
/// unsupported one — it quietly applies something else — so snapping here is
/// what makes the number the UI shows the number the hardware is using.
fn snap_gain_tenths(gains: &[i32], want: i32) -> i32 {
    gains
        .iter()
        .copied()
        .min_by_key(|g| (g - want).abs())
        .unwrap_or(want)
}

/// Whole dB, for the startup request that comes from a config rather than from
/// the table.
fn snap_gain(gains: &[i32], db: u32) -> i32 {
    snap_gain_tenths(gains, db as i32 * 10)
}

pub struct Source {
    /// rtl lib
    // ctl: Controller,
    reader: Reader,

    /// Handle for control signal threat
    ctrl_handle: JoinHandle<()>,

    sample_rate: u32,
    center_freq: Arc<AtomicU32>,
    tuned_freq: Arc<AtomicU32>,

    /// The gain the tuner actually accepted, in librtlsdr's tenths of a dB.
    /// Not necessarily what was asked for — see [`snap_gain`].
    pub applied_gain_tenths: i32,

    /// Every gain this tuner offers, tenths of a dB, ascending. Handed to the
    /// Control panel so its Gain row steps the real table instead of whole dB —
    /// on an FC0013 most whole-dB steps land in an ~11 dB dead zone and change
    /// nothing.
    pub gain_table: Vec<i32>,
}

impl Source {
    pub fn new(
        sample_rate: u32,
        center_freq: u32,
        bandwidth: u32,
        gain_db: u32,
        ctrl_rx: Receiver<CtrlSignal>,
    ) -> Self {
        // ── SDR setup (librtlsdr via rtlsdr_mt) ─────────────────────────────────
        let (mut ctl, reader) =
            rtlsdr_mt::open(0).expect(&CustomError::RtlOpenDevice(0).to_string());

        // Offset tuning: park the LO below the station, not on it.
        let lo = center_freq.saturating_sub(OFFSET_TUNING_HZ);
        ctl.set_center_freq(lo)
            .expect(&CustomError::RtlSetFreq(lo).to_string());

        let tuner_bw = tuner_bandwidth(bandwidth);
        ctl.set_bandwidth(tuner_bw)
            .expect(&CustomError::RtlSetBandwidth(tuner_bw).to_string());
        ctl.set_sample_rate(sample_rate)
            .expect(&CustomError::RtlSetSampleRate(sample_rate).to_string());

        // ── Gain: manual, not AGC ───────────────────────────────────────────────
        // The RTL2832's AGC is *digital*, downstream of the ADC. It cannot improve
        // the front end's noise figure, and it rides the level up and down against
        // FM's constant envelope, which is exactly the wrong thing for a mode whose
        // information is entirely in the phase.
        //
        // Audio SNR out of the discriminator tracks input CNR roughly 1:1 well
        // above threshold, so tuner gain is the single largest lever on how this
        // sounds. `disable_agc` turns the digital AGC off *and* puts the tuner in
        // manual gain mode; `set_tuner_gain` would only do the latter.
        ctl.disable_agc()
            .expect(&CustomError::RtlDisableAgc.to_string());

        let mut gain_buf: rtlsdr_mt::TunerGains = [0; 32];
        let mut gains: Vec<i32> = ctl.tuner_gains(&mut gain_buf).to_vec();
        // The Control panel steps this by index, so ascending order is load
        // bearing, not cosmetic. librtlsdr's tables happen to be sorted; a
        // tuner whose table is not would otherwise make the Gain row jump
        // around.
        gains.sort_unstable();
        let gains_for_ui = gains.clone();

        let gain = snap_gain(&gains, gain_db);
        ctl.set_tuner_gain(gain)
            .expect(&CustomError::RtlSetGain(gain).to_string());

        println!(
            "tuner gain: {:.1} dB (asked {} dB) | supported: {}",
            gain as f32 / 10.0,
            gain_db,
            gains
                .iter()
                .map(|g| format!("{:.1}", *g as f32 / 10.0))
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Flush stale USB buffers before streaming
        ctl.reset_buffer()
            .expect(&CustomError::RtlResetBuffer.to_string());

        let sample_rate = ctl.sample_rate();

        let tuned_freq = Arc::new(AtomicU32::new(center_freq));
        let center_freq = Arc::new(AtomicU32::new(center_freq));

        // Start a thread to receive control signal
        // better than checking within read_async
        let center_freq_clone = center_freq.clone();
        let ctrl_handle = thread::spawn(move || {
            // Every arm logs rather than printing: stdout is the TUI's once
            // `ratatui::init()` runs. This thread is not on a deadline path, so
            // it is allowed to (see `crate::log`).
            while let Ok(sig) = ctrl_rx.recv() {
                // TODO: handle error, should not expect()
                match sig {
                    CtrlSignal::CenterHz(freq) => {
                        // Same offset as `new` applied, or retuning would put
                        // the station where the Xlator is not looking.
                        let lo = freq.saturating_sub(OFFSET_TUNING_HZ);
                        ctl.set_center_freq(lo)
                            .expect(&CustomError::RtlSetFreq(lo).to_string());
                        center_freq_clone.store(freq, Ordering::Release);
                        // ctl.reset_buffer().expect(&CustomError::RtlResetBuffer.to_string());
                    }

                    // The UI sends a *channel* width; the tuner's IF filter is
                    // centred on the LO and has to reach past the offset to the
                    // far edge of the channel. Applying the channel width raw
                    // here — as this arm used to — sets a filter narrower than
                    // the offset itself and cuts the station out entirely.
                    CtrlSignal::Bandwidth(bw) => {
                        let tuner_bw = tuner_bandwidth(bw);
                        ctl.set_bandwidth(tuner_bw)
                            .expect(&CustomError::RtlSetBandwidth(tuner_bw).to_string());
                        log_info!(
                            "IF filter {:.0} kHz for a {:.0} kHz channel",
                            tuner_bw as f32 / 1e3,
                            bw as f32 / 1e3
                        );
                    }

                    // Snapped against the same table `new` used, so the TUI and
                    // the hardware never disagree about the current gain.
                    // `disable_agc` first because this signal means "manual, at
                    // this value" — see `CtrlSignal::GainTenths`.
                    CtrlSignal::GainTenths(tenths) => {
                        let g = snap_gain_tenths(&gains, tenths);
                        ctl.disable_agc()
                            .expect(&CustomError::RtlDisableAgc.to_string());
                        ctl.set_tuner_gain(g)
                            .expect(&CustomError::RtlSetGain(g).to_string());
                        if g != tenths {
                            log_warn!(
                                "tuner took {:.1} dB, not {:.1} dB",
                                g as f32 / 10.0,
                                tenths as f32 / 10.0
                            );
                        }
                    }

                    CtrlSignal::AgcOn => {
                        ctl.enable_agc()
                            .expect(&CustomError::RtlEnableAgc.to_string());
                    }

                    CtrlSignal::Ppm(ppm) => ctl
                        .set_ppm(ppm)
                        .expect(&CustomError::RtlSetPpm(ppm).to_string()),

                    CtrlSignal::Quit => {
                        ctl.cancel_async_read();
                        break;
                    }
                }
            }
        });

        // TODO: let skip tuned_freq for now
        Self {
            reader,
            ctrl_handle,
            sample_rate,
            center_freq,
            tuned_freq,
            applied_gain_tenths: gain,
            gain_table: gains_for_ui,
        }
    }

    /// What librtlsdr's divider actually rounded the requested rate to. Not
    /// generally the value passed to [`new`](Self::new).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
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

        // DCBlocker, before all
        let mut dc_blocker = IqDcBlocker::<IQ_BLOCK>::new(self.sample_rate);

        // Shift the station from +OFFSET_TUNING_HZ down to 0, which throws the
        // LO spike out to -OFFSET_TUNING_HZ. Positive offset = shift down; see
        // `Xlator::new`, whose delta carries the minus sign.
        let mut xlator = Xlator::new(OFFSET_TUNING_HZ as f32, self.sample_rate as f32);
        // Multi phase decimator
        let mut dsp = DSPFlow::new_boxed();

        let mut buf: [f32; IQ_BLOCK] = array::from_fn(|_| 0.0f32);
        let mut buf_i: [f32; 8192] = array::from_fn(|_| 0.0f32);
        let mut buf_q: [f32; 8192] = array::from_fn(|_| 0.0f32);

        let builder = thread::Builder::new()
            .stack_size(8 << 20)
            .name("thread-sdr".to_string());
        let handle = builder
            .spawn(move || {
                self.reader
                    .read_async(15, IQ_BLOCK as u32, move |bytes| {
                        // bytes: interleaved u8 IQ [I0,Q0,I1,Q1,...] at rtl_rate
                        // DSP right here
                        for (idx, pair) in bytes.chunks_exact(2).enumerate() {
                            // u8 → centered f32 in ~[-1, 1]
                            let (i, q) = center_iq(pair[0], pair[1]);
                            let (i, q) = dc_blocker.process(i, q);
                            // Ahead of the FFT tap deliberately: the waterfall then
                            // shows the tuned station centred, with the spike
                            // parked off to the left where you can see it behaving.
                            let (i, q) = xlator.process_sample(i, q);
                            (buf[2 * idx], buf[2 * idx + 1]) = (i, q);
                            buf_i[idx] = i;
                            buf_q[idx] = q;
                        }
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
            })
            .unwrap();

        Ok((handle, self.ctrl_handle))
    }
}

#[cfg(test)]
mod test {
    use super::snap_gain;

    /// The R820T/R820T2, by far the most common tuner.
    const R820T: [i32; 29] = [
        0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280, 297, 328, 338, 364,
        372, 386, 402, 421, 434, 439, 445, 480, 496,
    ];
    /// The FC0013 in this dongle, exactly as it reports itself. Note the shape:
    /// three switched-LNA clusters around -6, +7 and +19 dB with ~11 dB gaps,
    /// and a 19.7 dB ceiling. Any request above that clamps.
    const FC0013: [i32; 23] = [
        -99, -73, -65, -63, -60, -58, -54, 58, 61, 63, 65, 67, 68, 70, 71, 179, 181, 182, 184, 186,
        188, 191, 197,
    ];

    #[test]
    fn snaps_to_the_nearest_supported_value() {
        assert_eq!(snap_gain(&R820T, 40), 402, "40 dB is exact here");
        assert_eq!(snap_gain(&R820T, 41), 402, "41.0 -> 40.2, not 42.1");
        assert_eq!(snap_gain(&FC0013, 7), 70, "lands inside the middle cluster");
    }

    /// A request past either end must land on the end, never off the table.
    #[test]
    fn clamps_rather_than_extrapolating() {
        assert_eq!(snap_gain(&R820T, 0), 0);
        assert_eq!(snap_gain(&R820T, 99), 496, "above max -> max");
        assert_eq!(
            snap_gain(&FC0013, 40),
            197,
            "40 dB is above this tuner's ceiling"
        );
        assert_eq!(
            snap_gain(&FC0013, 0),
            -54,
            "below the lowest positive -> -5.4"
        );
    }

    /// A request landing in one of the ~11 dB dead zones must pick a real
    /// cluster rather than anything in between.
    #[test]
    fn a_request_inside_a_gap_picks_the_nearer_cluster() {
        assert_eq!(
            snap_gain(&FC0013, 1),
            58,
            "1 dB -> bottom of the middle cluster"
        );
        assert_eq!(
            snap_gain(&FC0013, 12),
            71,
            "12 dB -> top of the middle cluster"
        );
        assert_eq!(
            snap_gain(&FC0013, 14),
            179,
            "14 dB -> bottom of the high cluster"
        );
    }

    /// If the tuner reports nothing, fall through to the raw request rather
    /// than panicking on an empty `min_by_key`.
    #[test]
    fn empty_table_falls_back_to_the_request() {
        assert_eq!(snap_gain(&[], 40), 400);
    }
}
