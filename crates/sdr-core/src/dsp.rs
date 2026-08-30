use std::{array, usize};

use crate::complex::{ComplexF32};
use crate::fir_taps::{TAPS_27, TAPS_69};
use crate::spmc::RingProducer;

/// Next index + 1, but wrap around to 0 if reach max index
/// size must be power of 2
pub fn next_wrapped(i: usize, size: usize) -> usize {
    let mask = size - 1;
    (i + 1) & mask
}

/// Center 2 bytes IQ to 127.4, clamped to [-1, 1)
/// The offset of 127.4, not 127.5, is an empirical correction for the R820T's slight ADC bias. 
/// Using 127.5 leaves a small residual DC.
pub fn center_iq(i: u8, q: u8) -> (f32, f32) {
    (
        (i as f32 - 127.4) / 128.0,
        (q as f32 - 127.4) / 128.0,
    )
}

/// A high-pass filter for leaked energy at the center freq
// struct DcBlocker {
//     x_prev: f32,
//     y_prev: f32,
//     a: f32
// }
//
// impl DcBlocker {
//     fn new() -> Self {
//         Self { x_prev: 0.0, y_prev: 0.0, a: 0.999999 }
//     }
//
//     fn process(&mut self, x: f32) -> f32 {
//         let y = x - self.x_prev + self.a*self.y_prev;
//         self.x_prev = x;
//         self.y_prev = y;
//         y
//     }
// }

struct DcBlocker<const N: usize> {
    rate: f32,
    offset: f32,
}

impl<const N: usize> DcBlocker<N> {
    fn new(sample_rate: u32) -> Self { Self { rate: 50.0 / sample_rate as f32, offset: 0.0 }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = x - self.offset;
        self.offset = y * self.rate;
        y
    }
}

/// FIR (Finite Impulse Responsive) Filters are used filter out a range of frequency
/// by applying a convolution over time-domain window of signal, (instead of frequency domain).
/// A correctly apply filter will make signal, in the frequency domain, at both end of a window
/// settled to zero.
///
/// In frequency domain, filtering out a range of frequency is quite easy as it only required to
/// multiply the FFT array by a rectangular function/mask of, ideadly, 1 (for keep) or 0 (for remove).
/// The time-domain version of this mask is the sinc function `sinc(x) = sin(πx)/(πx)`.
/// However, this ideal mask cannot be applied to finite samples.
/// So windowed sinc is used instead: `h[k] = sinc(2·fc·(k - M)) · w[k]`
/// where:
///   k  = tap index, 0 to N-1
///   M  = (N-1)/2  — center of the filter (makes it symmetric)
///   fc = cutoff frequency normalized to 0..0.5  (fc = cutoff_hz / sample_rate)
///   w[k] = window function value at tap k
///
/// `w[k]` could be from different window funtions: Hann, Hamming, Blackman, Kaiser
pub enum Window {
    Hann,
    Hamming,
    Blackman,
    Kaiser,
}

pub enum FilterType {
    Lowpass,
    Highpass,
}

/// A sinc function to reconstruct continuous sample from discrete one
pub fn sinc(x: f32) -> f32 {
    if x.abs() < 1e-6 { 1.0 }
    else { (std::f32::consts::PI * x).sin() / (std::f32::consts::PI * x) }
}

/// Modified Bessel function I₀ — needed for Kaiser window
/// Series expansion, converges quickly for β < 20
fn bessel_i0(x: f32) -> f32 {
    let mut sum = 1.0f32;
    let mut term = 1.0f32;
    for k in 1..=20 {
        term *= (x / 2.0) / k as f32;
        term *= (x / 2.0) / k as f32;
        sum += term;
        if term < 1e-10 { break; }
    }
    sum
}

/// Create taps according to different window types
/// Feeded cutoff and sample rate are non-normalized
pub fn create_taps<const N: usize>(
    filter_type: FilterType,
    cutoff: f32,
    sample_rate: f32,
    window: Window,
    beta: Option<f32>) -> [f32; N]
{
    // Normalize
    let fc = cutoff/sample_rate;
    let alpha = ((N - 1)/2) as f32;
    let mut w = [0.0f32; N];
    let mut output = [0.0f32; N];
    let d = |k: f32| 2.0 * std::f32::consts::PI * k/(N as f32 - 1.0);
    match window {
        Window::Hann => {
            w.iter_mut().enumerate().for_each(|(k, x)| {
                *x = 0.5 * (1.0 - d(k as f32).cos());
            }); 
        }
        Window::Hamming => {
            w.iter_mut().enumerate().for_each(|(k, x)| {
                *x = 0.53 - 0.46 * d(k as f32).cos();
            });
        }
        Window::Blackman => {
            w.iter_mut().enumerate().for_each(|(k, x)| {
                *x = 0.42 - 0.5 * d(k as f32).cos() + 0.08 * (2.0 * d(k as f32)).cos();
            })
        }
        Window::Kaiser => {
            let beta = beta.unwrap_or(8.96); 
            let j = (N - 1) as f32;
            let i0_beta = bessel_i0(beta);
            w.iter_mut().enumerate().for_each(|(k, x)| {
                let ratio = (k as f32 - j / 2.0) / (j / 2.0);
                *x = bessel_i0(beta * (1.0 - ratio * ratio).max(0.0).sqrt()) / i0_beta;
            })
        }
    };

    output.iter_mut().enumerate().for_each(|(k, h)| {
        *h = sinc(2.0 * fc * (k as f32 - alpha))
    });

    // Highpass filter is lowpass filter inversed `h` 
    match filter_type {
        FilterType::Lowpass => {}
        FilterType::Highpass => {
            output.iter_mut().enumerate().for_each(|(k, h)| {
                if k as f32 != alpha {
                    *h = sinc(2.0 * fc * (k as f32 - alpha))
                };
            });
            output[alpha as usize] = 1.0 - output[alpha as usize]
        }
    }
    
    output.iter_mut().zip(w.iter_mut()).for_each(|(o, w)| {
        *o = *o * *w;
    });

    // Normalize dc gain = 1.0
    let gain: f32 = output.iter().sum();
    output.iter_mut().for_each(|o| *o /= gain);
    output
}

/// A FIR filter for channel I, Q, applying over a block of I, Q
/// This is a combination of downsampling and applying filter.
/// Given window size of `N`, taps size of `T`, the working buf has `T-1` padding at the beginning.
/// A normal FIR filter will convolute filter `T` over window `N`, with step size = 1,
/// resulting output of size `N`.
/// A decimating FIR will convolute with step size = `g`, `g` < `T`,
/// resulting output of size N % g
/// For example: a block size = 8192, T = 27, B = 8244
pub struct DecimFIR<const P: usize, const N: usize, const T: usize> {
    taps: [f32; T],
    pub buf_i: [f32; P],
    pub buf_q: [f32; P],
    offset: usize,
}

impl<const P: usize, const N: usize, const T: usize> DecimFIR<P, N, T> {
    pub fn new(taps: &[f32; T]) -> Self {
        assert_eq!(P, N + T - 1);
        Self {
            taps: *taps,
            buf_i: array::from_fn(|_| 0.0f32),
            buf_q: array::from_fn(|_| 0.0f32),
            offset: 0,
        }
    }

    /// Copy into buf from index T - 1, this is the first step before processing
    pub fn read_to_buf(&mut self, i: &[f32; N], q: &[f32; N]) {
        // Copy inputs into buf first
        // assert_eq!(N, i.len());
        // assert_eq!(N, q.len());

        // Copy from `T-1` as first `T` elements are padding at first
        self.buf_i[T - 1..T - 1 + N].copy_from_slice(i);
        self.buf_q[T - 1..T - 1 + N].copy_from_slice(q);
    }

    /// Apply filter over an input of predefined size.
    /// Output size must be the same as input size.
    /// No resampling here.
    pub fn process(
        &mut self,
        step_size: usize,
        out_i: &mut [f32],
        out_q: &mut [f32],
        )
    {
        let output_size = N / step_size;
        assert_eq!(out_i.len(), output_size);
        assert_eq!(out_q.len(), output_size);

        // An input of 8192 with step 4 produces 2048 samples
        let mut count = 0;

        // Stop when remaining window having size less than T
        // At this point, offset could have overpassed index N - 1 by k steps
        // In other words, the last taps window needs k steps to be full
        while self.offset < N {
            let w = self.offset..self.offset + T;
            out_i[count] = Self::convolve(&self.buf_i[w.clone()], &self.taps);
            out_q[count] = Self::convolve(&self.buf_q[w.clone()], &self.taps);
            count += 1;
            self.offset += step_size;
        };

        // Carry leftover step into next block,
        // offset step back N steps, because later we copy from N to last
        // that includes items already processed in the last taps windows
        self.offset -= N;

        // Retain last T-1 samples for next process
        self.buf_i.copy_within(N..N + T - 1, 0);
        self.buf_q.copy_within(N..N + T - 1, 0);
    }

    
    /// Apply convolution of `y` over `x`, given both having same lengthl
    /// Any padding must be apply before this.
    /// According to the definition of convolution, `y[k]` must be mapped to `x[N-k]`,
    /// or `y` at `k` mapped to sample `x` delayed by `k`,
    /// so `y` must be inversed first
    /// https://brianmcfee.net/dstbook-site/content/ch03-convolution/Convolution.html
    /// Written like this for vectorization with `target-cpu=native` build option
    fn convolve(x: &[f32], y: &[f32]) -> f32 {
        assert!(x.len() == y.len());
        
        let n = x.len();

        let mut acc_0 = 0.0f32;
        let mut acc_1 = 0.0f32;
        let mut acc_2 = 0.0f32;
        let mut acc_3 = 0.0f32;

        let chunk = x.len()/4;
        for i in 0..chunk {
            let base = i*4;
            acc_0 += x[base] * y[n - base - 1];
            acc_1 += x[base + 1] * y[n - base - 2];
            acc_2 += x[base + 2] * y[n - base - 3];
            acc_3 += x[base + 3] * y[n - base - 4];
        };

        let mut acc_tail = 0.0f32;
        let remainder_start = chunk * 4;
        for i in remainder_start..x.len() {
            acc_tail += x[i] * y[n - i - 1]
        };

        acc_0 + acc_1 + acc_2 + acc_3 + acc_tail
    }

    pub fn set_iq(&mut self, idx: usize, i: f32, q: f32) {
        (self.buf_i[idx], self.buf_q[idx]) = (i, q);
    }
}

/// Polyphase reampler
/// For example: from 300kHz -> 48kHz, interp = 4, decim = 25
pub struct PolyphaseResampler {
    branches:   Vec<Vec<f32>>,  // [interp][taps_per_branch]
    interp:     usize,
    decim:      usize,
    history:    Vec<f32>,       // last (taps_per_branch - 1) input samples
    phase:      usize,          // current branch index, 0..interp
    offset:     usize,          // carry from previous block
}

impl PolyphaseResampler {
    pub fn new(interp: usize, decim: usize, taps: &[f32]) -> Self {
        let taps_per_branch = (taps.len() + interp - 1) / interp;

        // Split taps into interp branches
        // tap k → branch (k % interp), position (k / interp)
        let mut branches = vec![vec![0.0f32; taps_per_branch]; interp];
        for (k, &tap) in taps.iter().enumerate() {
            branches[k % interp][k / interp] = tap;
        }

        Self {
            branches,
            interp,
            decim,
            history: vec![0.0; taps_per_branch - 1],
            phase: 0,
            offset: 0,
        }
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        assert!(output.len() >= (input.len() * self.interp).div_ceil(self.decim));
        let taps_per_branch = self.branches[0].len();
        let hist_len = taps_per_branch - 1;

        // Build working buffer: history + new input
        // history holds the last (taps_per_branch-1) samples from previous block
        let mut buf = Vec::with_capacity(hist_len + input.len());
        buf.extend_from_slice(&self.history);
        buf.extend_from_slice(input);

        let mut pos = self.offset;  // position in buf (not input)
        let mut pos_out: usize = 0;
    
        while pos + taps_per_branch <= buf.len() {
            // Dot product: branch[phase] against buf[pos..pos+taps_per_branch]
            let branch = &self.branches[self.phase];
            let window = &buf[pos..pos + taps_per_branch];
            let out: f32 = branch.iter()
                .zip(window.iter())
                .map(|(h, x)| h * x)
                .sum();
            // output.push(out);
            output[pos_out] = out;
            pos_out += 1;

            // Advance phase and position
            self.phase += self.decim;
            pos        += self.phase / self.interp;  // integer div = input samples to skip
            self.phase  = self.phase % self.interp;
        }

        // Save carry: how far into next input block we already are
        // pos is in buf coordinates (includes hist_len offset)
        // convert back to input coordinates
        // self.offset = pos.saturating_sub(hist_len + input.len());
        self.offset = pos - input.len();

        // Save history: last (taps_per_branch-1) samples of input
        let keep_from = input.len().saturating_sub(hist_len);
        self.history.clear();
        // pad with zeros if input was shorter than history
        for _ in input.len().min(hist_len)..hist_len {
            self.history.push(0.0);
        }
        self.history.extend_from_slice(&input[keep_from..]);
        pos_out
    }
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}


/// That high-pass filter for both I & Q channel
pub struct IqDcBlocker<const N: usize> {
    i_blocker: DcBlocker<N>,
    q_blocker: DcBlocker<N>,
}

impl<const N: usize> IqDcBlocker<N> {
    pub fn new(sample_rate: u32) -> Self {
        Self { 
            i_blocker: DcBlocker::<N>::new(sample_rate),
            q_blocker: DcBlocker::<N>::new(sample_rate),
        }
    }

    pub fn process(&mut self, i: f32, q: f32) -> (f32, f32) {
        (
            self.i_blocker.process(i),
            self.q_blocker.process(q),
        )
    }
}


/// Translate frequency
/// RTL-SDR holds center frequency
/// The channel you want hear is away from that, within sample rate
/// **Worked example.** Device centre 91.0 MHz at 2.4 MHz → you can see 89.8 – 92.2 MHz.
/// To hear a station at 91.5 MHz:
/// offset  = 91.5 MHz − 91.0 MHz = +500 kHz
/// ω       = 2π · 500000 / 2400000 = 1.3090 rad/sample     (hzToRads)
/// Δφ      = e^{−j·1.3090} = (0.2588, −0.9659)              (cos(−ω), sin(−ω))
pub struct Xlator {
    sample_rate: f32,
    offset: f32,
    phase: ComplexF32,
    delta: ComplexF32,
    n: u32
}

impl Xlator {
    pub fn new(offset: f32, sample_rate: f32) -> Self {
        let w = -std::f32::consts::TAU * offset / sample_rate;   // note the minus
        Self {
            sample_rate,
            offset,
            phase: ComplexF32::new(1.0, 0.0),
            delta: ComplexF32::new(w.cos(), w.sin()),
            n: 0
        }
    }

    pub fn set_offset(&mut self, offset: f32) {
        let w = -std::f32::consts::TAU * offset / self.sample_rate;
        self.delta = ComplexF32::new(w.cos(), w.sin());
    }

    pub fn process(&mut self, buf: &mut [ComplexF32]) {
        buf.iter_mut().for_each(|x| {
            *x = *x * self.phase;
            self.phase = self.phase * self.delta;
            self.n += 1;
            // TODO: what is 512 here
            if self.n >= 512 {
                // Normalize, as phase could drift after series of multiplication
                self.phase.scale(1.0 / self.phase.norm());
                self.n = 0;
            }
        });
    }
}

pub struct Demodulation {
    prev_i: f32,
    prev_q: f32,
    inv_deviation: f32,
}

impl Demodulation {
    pub fn new(deviation: f32, samples_rate: f32) -> Self {
        let rads = std::f32::consts::TAU * deviation / samples_rate;
        Self { prev_i: 0.0f32, prev_q: 0.0f32, inv_deviation: 1.0 / rads }
    }

    pub fn process<const N: usize>
        (&mut self, i: &[f32; N], q: &[f32; N], out: &mut [f32; N])
    {
        i.iter().zip(q.iter()).zip(out.iter_mut()).for_each(|((&x, &y), o)| {
            let re = x * self.prev_i + y * self.prev_q;
            let im = y * self.prev_i - x * self.prev_q;
            *o = im.atan2(re) * self.inv_deviation; // ∈ [-π, π]
            self.prev_i = x;
            self.prev_q = y;
        });
    }
}

pub struct Deemphasis {
    alpha: f32,
    last_out: f32,   // carries across blocks
}

impl Deemphasis {
    pub fn new(tau: f32, sample_rate: f32) -> Self {
      let dt = 1.0 / sample_rate;
      Self { alpha: dt / (tau + dt), last_out: 0.0 }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for x in buf.iter_mut() {
          *x = self.alpha * *x + (1.0 - self.alpha) *
        self.last_out;
          self.last_out = *x;
        }
    }
}

pub fn hann(x: f32, half_width: f32) -> f32 {
    if x.abs() > half_width { return 0.0; }
    0.5 * (1.0 + (std::f32::consts::PI * x / half_width).cos())
}

pub fn windowed_sinc_resample(
    input: &[f32],
    input_rate: f32,
    output_rate: f32,
    output: &mut [f32],
    half_taps: usize,   // typically 8-32
    )
{
    let ratio = input_rate / output_rate;
    let output_len = (input.len() as f32 / ratio) as usize;
    assert_eq!(output_len, output.len());

    let cutoff = 15_000.0_f32.min(output_rate / 2.0);
    let fc = 2.0 * cutoff / input_rate;
    let span = (half_taps as f32 / fc) as i32;

    for n in 0..output_len {
        let pos = n as f32 * ratio;
        let mut sum = 0.0f32;

        // The sweep must cover the *widened* kernel. Leaving it at ±half_taps
        // truncates a kernel that now needs ±span, which is a rectangular
        // window on top of the Hann one and puts the sidelobes back.
        for k in -span..=span {
            let i = pos as i32 + k;
            if i < 0 || i >= input.len() as i32 { continue; }

            let t = pos - i as f32;
            // `fc * sinc(fc * t)` is the lowpass kernel at cutoff `fc/2`
            // cycles/sample; it sums to exactly 1 at DC, so gain is unity.
            // `sinc(t)` alone would cut at the *input* Nyquist — no filtering.
            sum += input[i as usize] * fc * sinc(fc * t) * hann(t, span as f32);
        }
        output[n] = sum;
    }
}

/// A struct for partial filling output from DSP
/// For a DSP output of size 163 and a cpal frames count of 512 (both channels is 1024),
/// It takes more than 3 DSP output to fill one cpal frame
pub struct PartialWriter<const N: usize> {
    pub buf: [f32; N],
    cursor: usize,
    producer: RingProducer<f32, 16, N>
}

impl<const N: usize> PartialWriter<N> {
    pub fn new(ring_producer: RingProducer<f32, 16, N>) -> Self {
        Self { buf: [0.0f32; N], cursor: 0, producer: ring_producer }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn write<const M: usize>(&mut self, src: &[f32; M]) -> usize {
        let remained = N - self.cursor; 
        if M <= remained {
            // Having enough space in buf to write src
            self.buf[self.cursor..self.cursor + M].copy_from_slice(src);
            self.cursor += M;
            M
        } else {
            // Not enough space in buf for the whole src
            // Fill what it could
            self.buf[self.cursor..].copy_from_slice(&src[..remained]);
            
            // Flush to producer
            let _ = self.producer.write(&self.buf);

            // println!("write to audio");
            // println!("remained in buf: {}", M - remained);

            // Save the rest to buf
            self.buf[..M - remained].copy_from_slice(&src[remained..]);

            self.cursor = M - remained;
            remained
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_filter(taps: &[f32]) {
        // 1. DC gain should be 1.0 (sum of taps)
        let dc_gain: f32 = taps.iter().sum();
        println!("taps {:?}", taps);
        println!("dc gain {dc_gain}");
        assert!((dc_gain - 1.0).abs() < 1e-3);

        // 2. Filter should be symmetric (linear phase)
        let n = taps.len();
        let max_asymmetry = (0..n/2)
            .map(|k| (taps[k] - taps[n-1-k]).abs())
            .fold(0.0f32, f32::max);
        assert!(max_asymmetry.abs() < 1e-6);

        // 3. Nyquist gain should be near 0 for low-pass
        // Evaluate at Nyquist: alternating sum
        // let nyquist_gain: f32 = taps.iter()
        //     .enumerate()
        //     .map(|(k, &h)| if k % 2 == 0 { h } else { -h })
        //     .sum::<f32>()
        //     .abs();
        // println!("Nyquist gain: {:.6}  (should be ~0 for lowpass)", nyquist_gain);
    }

    #[test]
    fn test_wrapped() {
        let size: usize = 16;
        assert_eq!(next_wrapped(1, size), 2usize);
        assert_eq!(next_wrapped(15, size), 0);
    }

    #[test]
    fn test_filter() {
        let sample_rate = 300_000.0_f32;
        let filter_hann = create_taps::<51>(FilterType::Lowpass, 3000.0f32, sample_rate, Window::Hann, None);
        assert_filter(&filter_hann);
    }
}
