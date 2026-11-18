use std::array;
use std::sync::{Arc, atomic::{AtomicU32, AtomicI32, Ordering}};

use sdr_core::dsp::{
    DecimFIR, Deemphasis, Demodulation, FilterType, PilotPll, PolyphaseResampler, Window, create_taps,
    DcBlocker, rssi_dbfs_x10, Squelch
};

/// De-emphasis time constant. 50 µs everywhere except the Americas and South
/// Korea, which use 75 µs. Broadcast FM pre-emphasises treble by up to ~14 dB
/// at 15 kHz; undoing it is what removes the hiss, because the discriminator's
/// own noise floor rises at 6 dB/octave and this cuts it back down.
const DEEMPHASIS_TAU: f32 = 50e-6;

/// The WFM chain, from one USB transfer to one block of audio.
///
/// Input: [f32; 8192] I and Q at 2.4 MHz
/// Stage 1:  27 taps, step 4, -> [f32; 2048] ~ 600 kHz
/// Stage 2:  69 taps, step 2, -> [f32; 1024] ~ 300 kHz
/// Demod:    at 300 kHz, while the 200 kHz channel still fits. The output is
///           the full stereo multiplex (MPX), up to 57 kHz.
/// Pilot:    PLL on the 19 kHz pilot, 2·MPX·sin 2φ -> L−R at baseband
/// Resample: 4/25 -> 48 kHz, twice (sum and diff), 163 or 164 samples
///           (8192/50 = 163.84). The 15 kHz prototype is both the mono
///           channel filter and the L−R baseband filter.
/// Matrix:   L = sum + diff, R = sum − diff
/// De-emph:  50 us one-pole at 48 kHz, per channel *after* the matrix —
///           pre-emphasis was applied to L and R, not to sum and diff
///
/// Roughly 93 KB of buffers, so construct it with [`new_boxed`](Self::new_boxed):
/// this ends up inside two nested `move` closures on the SDR thread, and the
/// intermediate copies of an inline version overflow a default stack in debug.
pub struct DSPFlow {
    stage_1: DecimFIR<8218, 8192, 27>,  // 26 + 8192, 27 taps, 2048 output
    stage_2: DecimFIR<2116, 2048, 69>,  // 68 + 2048, 69 taps, 1024 output
    squelch: Squelch,
    demod: Demodulation,
    pll: PilotPll,                      // 19 kHz pilot -> coherent 38 kHz
    resampler_sum: PolyphaseResampler,  // 1024, up 4 down 25, L+R
    resampler_diff: PolyphaseResampler, // same prototype, L−R
    deemph_l: Deemphasis,               // runs last, at 48 kHz
    deemph_r: Deemphasis,
    dc_blocker: DcBlocker,              // on the sum only; L−R has no DC
    out_i_decim: [f32; 1024],
    out_q_decim: [f32; 1024],
    out_squelch: [f32; 1024],
    out_demod: [f32; 1024],
    out_diff: [f32; 1024],
    /// Only `[..n]` is live, where `n` is what [`process`](Self::process) returns.
    pub out_l: [f32; 164],
    pub out_r: [f32; 164],
}

impl DSPFlow {
    pub fn new(squelch: Arc<AtomicI32>) -> Self {
        // Generate lowpass filters
        // TODO: This could be half-band filter
        // Half-band FIR — every even tap (except center) is exactly 0.0
        // You only store and compute the non-zero taps
        // For a 27-tap half-band, only 14 taps matter:
        // [h0, 0, h2, 0, h4, 0, ..., h12, 0, 0.5, 0, h12, ..., h0]
        //                                       ↑ center tap = 0.5
        // So only 14 multiplication for 27 taps
        let lpf_taps_1 = create_taps::<27>(
            FilterType::Lowpass,
            110_000.0,
            2_400_000.0,
            Window::Kaiser,
            None,
        );
        let lpf_taps_2 = create_taps::<69>(
            FilterType::Lowpass,
            85_000.0,
            600_000.0,
            Window::Kaiser,
            None,
        );
        let mut lpf_taps_resampler = create_taps::<1901>(
            FilterType::Lowpass,
            15_000.0,
            1_200_000.0,
            Window::Kaiser,
            None,
        );

        // Resampler has 4 branch, each holds 1/4 of the original normalized DC gain of 1.0
        // so need to add DC gain for each branch
        lpf_taps_resampler.iter_mut().for_each(|h| *h *= 4.0);
        Self {
            stage_1: DecimFIR::<8218, 8192, 27>::new(&lpf_taps_1),
            stage_2: DecimFIR::<2116, 2048, 69>::new(&lpf_taps_2),
            squelch: Squelch::new(squelch, 5.0, 0.01, 0.001),
            demod: Demodulation::new(75_000.0, 300_000.0),
            // 50 Hz loop noise bandwidth is 6000:1 against the MPX rate, so
            // programme and RDS average out of the detector. ±100 Hz pull-in
            // covers dongle ppm error (50 ppm is ~1 Hz at 19 kHz) many times.
            pll: PilotPll::new(300_000.0, 50.0, 100.0),
            resampler_sum: PolyphaseResampler::new(4, 25, &lpf_taps_resampler),
            resampler_diff: PolyphaseResampler::new(4, 25, &lpf_taps_resampler),
            deemph_l: Deemphasis::new(DEEMPHASIS_TAU, 48_000.0),
            deemph_r: Deemphasis::new(DEEMPHASIS_TAU, 48_000.0),
            dc_blocker: DcBlocker::new(48_000),
            out_i_decim: array::from_fn(|_| 0.0f32),
            out_q_decim: array::from_fn(|_| 0.0f32),
            out_squelch: array::from_fn(|_| 0.0f32),
            out_demod: array::from_fn(|_| 0.0f32),
            out_diff: array::from_fn(|_| 0.0f32),
            out_l: array::from_fn(|_| 0.0f32),
            out_r: array::from_fn(|_| 0.0f32),
        }
    }

    pub fn new_boxed(squelch: Arc<AtomicI32>) -> Box<Self> {
        Box::new(Self::new(squelch))
    }

    pub fn process(
        &mut self,
        i: &[f32; 8192],
        q: &[f32; 8192],
        ) -> (usize, i32) {
        assert_eq!(8192, i.len());
        assert_eq!(8192, q.len());
        // Returned number of sample processed
        self.stage_1.read_to_buf(i, q);

        // Write output directly to next stage buf
        self.stage_1.process(
            4,
            &mut self.stage_2.buf_i[68..],
            &mut self.stage_2.buf_q[68..],
        );
        self.stage_2
            .process(2, &mut self.out_i_decim, &mut self.out_q_decim);

        // Squelch must be before demod
        self.squelch.process(&self.out_i_decim, &self.out_q_decim, &mut self.out_squelch);

        // Demod must run before resampling
        self.demod
            .process(&self.out_i_decim, &self.out_q_decim, &mut self.out_demod);

        // Demod output at 300kHz, afterwards, the stereo decoder requires different bands:
        // - 0-15kHz, L+R, "sum", for mono compatibility
        // - 19kHz, stereo pilot tone, triggers the stereo indicator and acts as a phase reference
        // - 23-53kHz, L-R, "diff"
        // - 57kHz, RDS/RBDS data, station names, text, etc. at 1187.5bps

        // Apply squelch filter. Before the PLL, so a closed squelch also
        // starves the lock detector and the indicator drops with the audio.
        self.out_demod.iter_mut().zip(self.out_squelch.iter()).for_each(|(o, f)| {
            *o *= *f
        });

        // Update RSSI at bandwidth 300kHz
        // let mut acc = 0.0f32;
        // for (i, q) in self.out_i_decim.iter().zip(self.out_q_decim.iter()) {
        //     acc += i * i + q * q;
        // };
        // let mean_pow = acc / self.out_q_decim.len() as f32;
        // let dbfs = 10.0 * mean_pow.max(1e-12).log10();
        // let rssi_dbfs_x10 = (dbfs * 10.0).round() as i32;
        let rssi = rssi_dbfs_x10(&self.out_i_decim, &self.out_q_decim);

        // L−R down from 38 kHz to baseband, still at 300 kHz. Silent when
        // there is no pilot, which makes everything below plain mono.
        self.pll.process(&self.out_demod, &mut self.out_diff);

        // Twin resamplers. They start from the same state and see the same
        // block length, so they produce the same count on every call and the
        // sum and diff stay sample-aligned — the matrix depends on that.
        let n = self.resampler_sum.process(&self.out_demod, &mut self.out_l);
        let n_diff = self.resampler_diff.process(&self.out_diff, &mut self.out_r);
        debug_assert_eq!(n, n_diff);

        // DC block the sum, then matrix in place: out_l holds sum, out_r diff.
        for (l, r) in self.out_l[..n].iter_mut().zip(self.out_r[..n].iter_mut()) {
            let sum = self.dc_blocker.process(*l);
            let diff = *r;
            *l = sum + diff;
            *r = sum - diff;
        }

        // De-emphasis, at the audio rate and on the live samples only. The
        // one-pole state carries across calls, so the varying block length
        // (163 or 164) is harmless — what matters is that the stream stays
        // contiguous, which `push` on the ring guarantees.
        self.deemph_l.process(&mut self.out_l[..n]);
        self.deemph_r.process(&mut self.out_r[..n]);

        (n, rssi)
    }

    /// Stereo indicator state, as of the end of the last block.
    pub fn is_stereo(&self) -> bool {
        self.pll.is_stereo()
    }

    /// Pilot injection as a fraction of full deviation.
    pub fn pilot_level(&self) -> f32 {
        self.pll.pilot_level()
    }
}
