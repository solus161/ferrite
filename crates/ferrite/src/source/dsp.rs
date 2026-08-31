use std::array;

use sdr_core::dsp::{
    DecimFIR, Deemphasis, Demodulation, FilterType, PolyphaseResampler, Window, create_taps,
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
/// Demod:    at 300 kHz, while the 200 kHz channel still fits
/// Resample: 4/25 -> 48 kHz, 163 or 164 samples (8192/50 = 163.84)
/// De-emph:  50 us one-pole at 48 kHz
///
/// Roughly 93 KB of buffers, so construct it with [`new_boxed`](Self::new_boxed):
/// this ends up inside two nested `move` closures on the SDR thread, and the
/// intermediate copies of an inline version overflow a default stack in debug.
pub struct DSPFlow {
    stage_1: DecimFIR<8218, 8192, 27>, // 26 + 8192, 27 taps, 2048 output
    stage_2: DecimFIR<2116, 2048, 69>, // 68 + 2048, 69 taps, 1024 output
    demod: Demodulation,
    resampler: PolyphaseResampler, // 1024, up 4 down 25
    deemph: Deemphasis,            // runs last, at 48 kHz
    out_i_decim: [f32; 1024],
    out_q_decim: [f32; 1024],
    out_demod: [f32; 1024],
    /// Only `[..n]` is live, where `n` is what [`process`](Self::process) returns.
    pub out: [f32; 164],
}

impl DSPFlow {
    pub fn new() -> Self {
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
            100_000.0,
            2_400_000.0,
            Window::Hann,
            None,
        );
        let lpf_taps_2 = create_taps::<69>(
            FilterType::Lowpass,
            100_000.0,
            600_000.0,
            Window::Hann,
            None,
        );
        let mut lpf_taps_resampler = create_taps::<1900>(
            FilterType::Lowpass,
            15_000.0,
            1_200_000.0,
            Window::Hann,
            None,
        );

        // Resampler has 4 branch, each holds 1/4 of the original normalized DC gain of 1.0
        // so need to add DC gain for each branch
        lpf_taps_resampler.iter_mut().for_each(|h| *h *= 4.0);
        Self {
            stage_1: DecimFIR::<8218, 8192, 27>::new(&lpf_taps_1),
            stage_2: DecimFIR::<2116, 2048, 69>::new(&lpf_taps_2),
            demod: Demodulation::new(75_000.0, 300_000.0),
            resampler: PolyphaseResampler::new(4, 25, &lpf_taps_resampler),
            deemph: Deemphasis::new(DEEMPHASIS_TAU, 48_000.0),
            out_i_decim: array::from_fn(|_| 0.0f32),
            out_q_decim: array::from_fn(|_| 0.0f32),
            out_demod: array::from_fn(|_| 0.0f32),
            out: array::from_fn(|_| 0.0f32),
        }
    }

    pub fn new_boxed() -> Box<Self> {
        Box::new(Self::new())
    }

    pub fn process(&mut self, i: &[f32; 8192], q: &[f32; 8192]) -> usize {
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

        // Demod must run before resampling
        self.demod
            .process(&self.out_i_decim, &self.out_q_decim, &mut self.out_demod);

        // Resampler
        let resampler_count = self.resampler.process(&self.out_demod, &mut self.out);

        // De-emphasis, at the audio rate and on the live samples only. The
        // one-pole state carries across calls, so the varying block length
        // (163 or 164) is harmless — what matters is that the stream stays
        // contiguous, which `push` on the ring guarantees.
        self.deemph.process(&mut self.out[..resampler_count]);

        resampler_count
    }
}
