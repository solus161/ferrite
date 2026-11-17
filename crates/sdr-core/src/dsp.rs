use std::sync::atomic::Ordering;
use std::{array, usize};
use std::sync::{Arc, atomic::AtomicI32};

use crate::complex::ComplexF32;

/// Next index + 1, but wrap around to 0 if reach max index
/// size must be power of 2
#[inline]
pub fn next_wrapped(i: usize, size: usize) -> usize {
    let mask = size - 1;
    (i + 1) & mask
}

/// Center 2 bytes IQ to 127.4, clamped to [-1, 1)
/// The offset of 127.4, not 127.5, is an empirical correction for the R820T's slight ADC bias.
/// Using 127.5 leaves a small residual DC.
#[inline]
pub fn center_iq(i: u8, q: u8) -> (f32, f32) {
    ((i as f32 - 127.4) / 128.0, (q as f32 - 127.4) / 128.0)
}

pub struct DcBlocker {
    rate: f32,
    offset: f32,
}

impl DcBlocker {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            rate: 50.0 / sample_rate as f32,
            offset: 0.0,
        }
    }
    
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = x - self.offset;
        self.offset += y * self.rate;
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
    if x.abs() < 1e-6 {
        1.0
    } else {
        (std::f32::consts::PI * x).sin() / (std::f32::consts::PI * x)
    }
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
        if term < 1e-10 {
            break;
        }
    }
    sum
}

/// Create taps according to different window types
/// Feeded cutoff and sample rate are non-normalized
/// Taps are created reversed so can be used with convolve fn rightaway
pub fn create_taps<const N: usize>(
    filter_type: FilterType,
    cutoff: f32,
    sample_rate: f32,
    window: Window,
    beta: Option<f32>,
) -> [f32; N] {
    // Normalize
    let fc = cutoff / sample_rate;
    let alpha = ((N - 1) / 2) as f32;
    let mut w = [0.0f32; N];
    let mut output = [0.0f32; N];
    let d = |k: f32| 2.0 * std::f32::consts::PI * k / (N as f32 - 1.0);
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
        Window::Blackman => w.iter_mut().enumerate().for_each(|(k, x)| {
            *x = 0.42 - 0.5 * d(k as f32).cos() + 0.08 * (2.0 * d(k as f32)).cos();
        }),
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

    output
        .iter_mut()
        .enumerate()
        .for_each(|(k, h)| *h = sinc(2.0 * fc * (k as f32 - alpha)));

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

    // Reverse taps to used with convolve fn
    output.reverse();
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
    pub fn process(&mut self, step_size: usize, out_i: &mut [f32], out_q: &mut [f32]) {
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
        }

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
    /// taps must be reversed
    /// https://brianmcfee.net/dstbook-site/content/ch03-convolution/Convolution.html
    /// Written like this for vectorization with `target-cpu=native` and `target-feature=+avx2` build option
    fn convolve(x: &[f32], y: &[f32]) -> f32 {
        assert!(x.len() == y.len());

        // let n = x.len();
        let mut acc = [0.0f32; 8];
        let chunk = x.len() / 8;
        for i in 0..chunk {
            let base = i * 8;
            for k in 0..8 {
                acc[k] += x[base + k] * y[base + k]
            }
        }

        let mut acc_tail = 0.0f32;
        let remainder_start = chunk * 8;
        for i in remainder_start..x.len() {
            acc_tail += x[i] * y[i]
        }

        acc.iter().sum::<f32>() + acc_tail
    }

    pub fn set_iq(&mut self, idx: usize, i: f32, q: f32) {
        (self.buf_i[idx], self.buf_q[idx]) = (i, q);
    }
}

/// Polyphase reampler
/// For example: from 300kHz -> 48kHz, interp = 4, decim = 25
pub struct PolyphaseResampler {
    branches: Vec<Vec<f32>>, // [interp][taps_per_branch]
    interp: usize,
    decim: usize,
    history: Vec<f32>, // last (taps_per_branch - 1) input samples
    phase: usize,      // current branch index, 0..interp
    offset: usize,     // carry from previous block
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

        let mut pos = self.offset; // position in buf (not input)
        let mut pos_out: usize = 0;

        while pos + taps_per_branch <= buf.len() {
            // Dot product: branch[phase] against buf[pos..pos+taps_per_branch]
            let branch = &self.branches[self.phase];
            let window = &buf[pos..pos + taps_per_branch];
            let out: f32 = branch.iter().zip(window.iter()).map(|(h, x)| h * x).sum();
            // output.push(out);
            output[pos_out] = out;
            pos_out += 1;

            // Advance phase and position
            self.phase += self.decim;
            pos += self.phase / self.interp; // integer div = input samples to skip
            self.phase %= self.interp;
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
pub struct IqDcBlocker {
    i_blocker: DcBlocker,
    q_blocker: DcBlocker,
}

impl IqDcBlocker {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            i_blocker: DcBlocker::new(sample_rate),
            q_blocker: DcBlocker::new(sample_rate),
        }
    }
    
    #[inline]
    pub fn process(&mut self, i: f32, q: f32) -> (f32, f32) {
        (self.i_blocker.process(i), self.q_blocker.process(q))
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
    n: u32,
}

impl Xlator {
    pub fn new(offset: f32, sample_rate: f32) -> Self {
        let w = -std::f32::consts::TAU * offset / sample_rate; // note the minus
        Self {
            sample_rate,
            offset,
            phase: ComplexF32::new(1.0, 0.0),
            delta: ComplexF32::new(w.cos(), w.sin()),
            n: 0,
        }
    }

    /// Retune without touching `phase`, so the phasor stays continuous across
    /// the change and a retune produces no click in the demodulated audio.
    pub fn set_offset(&mut self, offset: f32) {
        let w = -std::f32::consts::TAU * offset / self.sample_rate;
        self.offset = offset;
        self.delta = ComplexF32::new(w.cos(), w.sin());
    }

    pub fn process(&mut self, buf: &mut [ComplexF32]) {
        buf.iter_mut().for_each(|x| {
            *x = *x * self.phase;
            self.advance();
        });
    }

    /// One sample at a time, for callers holding I and Q as separate arrays.
    ///
    /// The SDR callback centres and DC-blocks sample by sample already, so
    /// going through a `ComplexF32` buffer would mean an extra pass over
    /// 8192 samples purely to change container.
    #[inline]
    pub fn process_sample(&mut self, i: f32, q: f32) -> (f32, f32) {
        // (i + jq)(pr + jpi)
        let (pr, pi) = (self.phase.real, self.phase.img);
        let out = (i * pr - q * pi, i * pi + q * pr);
        self.advance();
        out
    }

    /// Step the phasor, renormalising periodically.
    ///
    /// Repeated complex multiplication accumulates rounding error, and in f32
    /// the magnitude drifts off 1.0 within a few thousand samples — which would
    /// show up as slow amplitude modulation. 512 is simply often enough that
    /// the drift stays far below the noise floor while the divide is amortised
    /// to nothing.
    #[inline]
    fn advance(&mut self) {
        self.phase = self.phase * self.delta;
        self.n += 1;
        if self.n >= 512 {
            self.phase.scale(1.0 / self.phase.norm());
            self.n = 0;
        }
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
        Self {
            prev_i: 0.0f32,
            prev_q: 0.0f32,
            inv_deviation: 1.0 / rads,
        }
    }

    pub fn process<const N: usize>(&mut self, i: &[f32; N], q: &[f32; N], out: &mut [f32; N]) {
        i.iter()
            .zip(q.iter())
            .zip(out.iter_mut())
            .for_each(|((&x, &y), o)| {
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
    last_out: f32, // carries across blocks
}

impl Deemphasis {
    pub fn new(tau: f32, sample_rate: f32) -> Self {
        let dt = 1.0 / sample_rate;
        Self {
            alpha: dt / (tau + dt),
            last_out: 0.0,
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for x in buf.iter_mut() {
            *x = self.alpha * *x + (1.0 - self.alpha) * self.last_out;
            self.last_out = *x;
        }
    }
}

pub fn hann(x: f32, half_width: f32) -> f32 {
    if x.abs() > half_width {
        return 0.0;
    }
    0.5 * (1.0 + (std::f32::consts::PI * x / half_width).cos())
}

pub fn windowed_sinc_resample(
    input: &[f32],
    input_rate: f32,
    output_rate: f32,
    output: &mut [f32],
    half_taps: usize, // typically 8-32
) {
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
            if i < 0 || i >= input.len() as i32 {
                continue;
            }

            let t = pos - i as f32;
            // `fc * sinc(fc * t)` is the lowpass kernel at cutoff `fc/2`
            // cycles/sample; it sums to exactly 1 at DC, so gain is unity.
            // `sinc(t)` alone would cut at the *input* Nyquist — no filtering.
            sum += input[i as usize] * fc * sinc(fc * t) * hann(t, span as f32);
        }
        output[n] = sum;
    }
}

/// Compute Reiceived Signal Strength Indicator
/// The reference is full scale (0 db)
/// Rounded to integer for ease of processing
pub fn rssi_dbfs_x10(buf_i: &[f32], buf_q: &[f32]) -> i32 {
    assert_eq!(buf_i.len(), buf_q.len());
    let mut acc = 0.0f32;
    for (i, q) in buf_i.iter().zip(buf_q.iter()) {
        acc += i * i + q * q;
    };
    let mean_pow = acc / buf_i.len() as f32;

    // Convert to db
    let dbfs = 10.0 * mean_pow.max(1e-12).log10();

    // x10 to encode as deicmal
    (dbfs * 10.0).round() as i32
}

/// Squelch with hysteresis and attack/decay
pub struct Squelch {
    /// Threshold at wich squelch opens when signal rising
    open: Arc<AtomicI32>,
    /// Lower band, squelch close
    close_band: f32,
    is_open: bool,
    envelop: f32,
    /// Attack speed
    attack: f32,
    /// Decay speed
    decay: f32,
}

impl Squelch {
    pub fn new(open: Arc<AtomicI32>, close_band: f32, attack: f32, decay: f32) -> Self{
        assert!(close_band > 0.0);
        Self { open, close_band, is_open: false, envelop: 0.0, attack, decay }
    }

    pub fn process(&mut self, buf_i: &[f32], buf_q: &[f32], output: &mut [f32]) {
        assert_eq!(buf_i.len(), buf_q.len());
        assert_eq!(buf_i.len(), output.len());

        let mut magnitude: f32 = 0.0;
        let open = self.open.load(Ordering::Relaxed) as f32;
        buf_i.iter().zip(buf_q.iter()).zip(output.iter_mut()).for_each(|((i, q), o)| {
            magnitude = (i*i + q*q).sqrt();

            // Envelop is smoothed out based on attack/decay rate
            // These two may not be the same
            if magnitude > self.envelop {
                self.envelop += self.attack * (magnitude - self.envelop);
            } else {
                self.envelop += self.decay * (magnitude - self.envelop)
            }

            // db for volts, amperes, sound pressure
            let db = (20.0 * self.envelop.log10()).max(-90.0);
            
            if !self.is_open && db > open {
                self.is_open = true;
            } else if self.is_open && db < open - self.close_band {
                self.is_open = false;
            };

            *o = if self.is_open { 1.0 } else { 0.0 }
        });
    }
}

/// FM Stereo L-R lives at 23-53kHz of a 300kHz bandwidth.
/// This one emits a coherent 38kHz reference for the L-R downmix.
///
/// The multiplex, with `θ` the pilot phase (FCC 73.322: the 38 kHz subcarrier
/// crosses zero with positive slope together with the pilot):
///
/// ```text
/// mpx = (L+R)/2  +  p·sin θ  +  (L−R)/2 · sin 2θ  (+ RDS at 57 kHz)
/// ```
///
/// The phase detector is quadrature, not a bare multiply: `mpx` is mixed with
/// `sin φ` and `cos φ`, both products are low-passed, and the error is
/// `atan2(Q, I) = θ − φ`. A bare `mpx · cos φ` detector has a gain proportional
/// to the pilot amplitude `p`, which is 8–10 % of deviation and varies by
/// station — the loop bandwidth would be ~20× narrower than asked for and
/// station-dependent. `atan2` makes it exactly 1 rad/rad, and the same `I` is
/// the pilot-presence measurement for free.
pub struct PilotPll {
    /// NCO phase in rad, wrapped to [0, 2π)
    phase: f32,
    /// NCO freq rad/sample
    freq: f32,
    w_min: f32,
    w_max: f32,
    /// PI loop filter
    alpha: f32,
    beta: f32,
    /// Two cascaded one-poles per arm. One pole leaves L+R content near 15 kHz
    /// (pre-emphasised, so not small) beating against the NCO at ~4 kHz only
    /// 10× down — comparable to the pilot itself. Two give ~250×, and still
    /// put only ~14° of lag at the loop's crossover.
    i_lp: [f32; 2],
    q_lp: [f32; 2],
    iq_alpha: f32,
    /// Smoothed in-phase pilot amplitude. Zero-mean when unlocked, `p/2` when
    /// locked.
    lock: f32,
    lock_alpha: f32,
    stereo: bool,
    /// 0 = mono, 1 = full stereo. Ramped, so a lock change fades rather than
    /// switching the L−R channel in or out mid-waveform.
    blend: f32,
    blend_alpha: f32,
}

impl PilotPll {
    const PILOT_HZ: f32 = 19_000.0;
    const DAMPING: f32 = 0.707;
    /// I/Q arm corner. ~15× the loop's natural frequency at 50 Hz noise
    /// bandwidth, so the filter barely touches loop dynamics.
    const IQ_CORNER_HZ: f32 = 250.0;
    /// Pilot injection, as a fraction of full deviation, at which stereo
    /// switches on and back off. The spec puts the pilot at 8–10 %; the gap is
    /// hysteresis so a fade near threshold does not flap.
    const LOCK_ON: f32 = 0.04;
    const LOCK_OFF: f32 = 0.025;

    /// `loop_bw_hz` is the one-sided noise bandwidth — how fast the loop
    /// tracks, and how much of the MPX's own energy it lets through. 50 Hz at
    /// 300 kHz is a 6000:1 ratio, narrow enough that L+R and RDS average to
    /// nothing in the detector.
    ///
    /// `pull_in_hz` only has to cover the dongle's sample-rate error: at 50 ppm
    /// a true 19 kHz pilot lands 0.95 Hz off, and the broadcast spec holds the
    /// pilot itself to ±2 Hz. 100 Hz is generous.
    pub fn new(sample_rate: f32, loop_bw_hz: f32, pull_in_hz: f32) -> Self {
        let w0 = std::f32::consts::TAU * Self::PILOT_HZ / sample_rate;
        let pull = std::f32::consts::TAU * pull_in_hz / sample_rate;

        // Analog prototype, normalised to rad/sample. For a second-order loop
        // B_n = (ω_n / 2)·(ζ + 1/(4ζ)) with B_n in Hz and ω_n in rad/s, so
        // ω_n = 2·B_n / (ζ + 1/(4ζ)) — a factor of 2, not 2π. The 1.0607 is
        // ζ + 1/(4ζ) at ζ = 0.707.
        let wn = 2.0 * loop_bw_hz
            / (sample_rate * (Self::DAMPING + 1.0 / (4.0 * Self::DAMPING)));

        let one_pole = |hz: f32| 1.0 - (-std::f32::consts::TAU * hz / sample_rate).exp();

        Self {
            phase: 0.0,
            freq: w0,
            w_min: w0 - pull,
            w_max: w0 + pull,
            alpha: 2.0 * Self::DAMPING * wn,
            beta: wn * wn,
            i_lp: [0.0; 2],
            q_lp: [0.0; 2],
            iq_alpha: one_pole(Self::IQ_CORNER_HZ),
            lock: 0.0,
            // ~16 Hz one-pole: fast enough to drop to mono on a station change,
            // slow enough to ride out a fade.
            lock_alpha: one_pole(16.0),
            stereo: false,
            blend: 0.0,
            // ~16 ms time constant.
            blend_alpha: one_pole(10.0),
        }
    }

    /// Track the pilot through `mpx` and write the recovered L−R into `diff`,
    /// still at the MPX rate and not yet low-passed.
    ///
    /// `diff[k] = 2 · mpx[k] · sin 2φ · blend`. At lock `φ = θ`, and
    /// `(L−R)/2 · sin²2θ` averages to `(L−R)/4`, hence the 2: what comes out of
    /// the caller's 15 kHz low-pass is `(L−R)/2`, the same scale the `(L+R)/2`
    /// mono channel is already at, so `L = sum + diff` and `R = sum − diff`
    /// need no further gain. Every other product (L+R at 23–53 kHz, pilot at
    /// 19 and 57 kHz, RDS at 19 and 95 kHz) lands outside that low-pass.
    ///
    /// Unlocked, `blend` is 0 and `diff` is silent, so the matrix degrades to
    /// mono at exactly the old level.
    pub fn process(&mut self, mpx: &[f32], diff: &mut [f32]) {
        assert_eq!(mpx.len(), diff.len());
        let tau = std::f32::consts::TAU;

        for (&x, d) in mpx.iter().zip(diff.iter_mut()) {
            let (s, c) = self.phase.sin_cos();

            // x = p·sin θ  →  x·sin φ → (p/2)·cos(θ−φ),  x·cos φ → (p/2)·sin(θ−φ)
            let a = self.iq_alpha;
            self.i_lp[0] += a * (x * s - self.i_lp[0]);
            self.i_lp[1] += a * (self.i_lp[0] - self.i_lp[1]);
            self.q_lp[0] += a * (x * c - self.q_lp[0]);
            self.q_lp[1] += a * (self.q_lp[0] - self.q_lp[1]);
            let (i, q) = (self.i_lp[1], self.q_lp[1]);

            // sin 2φ, from the sin/cos already in hand.
            *d = 2.0 * x * (2.0 * s * c) * self.blend;

            // θ − φ. Positive means the NCO lags, so step faster.
            let err = q.atan2(i);
            // Clamping the integrator is also the anti-windup: off-station it
            // would otherwise wander without bound on noise.
            self.freq = (self.freq + self.beta * err).clamp(self.w_min, self.w_max);
            self.phase += self.freq + self.alpha * err;
            if self.phase >= tau {
                self.phase -= tau;
            } else if self.phase < 0.0 {
                self.phase += tau;
            }

            // Lock detector, on the in-phase arm: averages to zero unless the
            // loop is actually sitting on a pilot.
            self.lock += self.lock_alpha * (i - self.lock);
            let level = self.pilot_level();
            if !self.stereo && level > Self::LOCK_ON {
                self.stereo = true;
            } else if self.stereo && level < Self::LOCK_OFF {
                self.stereo = false;
            }

            let target = if self.stereo { 1.0 } else { 0.0 };
            self.blend += self.blend_alpha * (target - self.blend);
        }
    }

    /// Whether the decoder is (switching to) stereo. Drives the indicator.
    pub fn is_stereo(&self) -> bool {
        self.stereo
    }

    /// Coherent pilot amplitude as a fraction of full deviation — the stereo
    /// injection level, ~0.08–0.10 on a well-behaved station.
    pub fn pilot_level(&self) -> f32 {
        2.0 * self.lock
    }
}

#[cfg(test)]
mod xlator_tests {
    use super::*;

    const FS: f32 = 2_400_000.0;
    const OFF: f32 = 350_000.0;

    /// Generate `n` samples of a complex tone at `hz`, run them through an
    /// Xlator set to `off`, and report the mean vector. A tone shifted to DC
    /// has a mean of magnitude ~1; a tone left anywhere else averages to ~0.
    fn shifted_mean(hz: f32, off: f32, n: usize) -> f32 {
        let mut x = Xlator::new(off, FS);
        let (mut sr, mut si) = (0.0f32, 0.0f32);
        for k in 0..n {
            let p = std::f32::consts::TAU * hz * k as f32 / FS;
            let (i, q) = x.process_sample(p.cos(), p.sin());
            sr += i;
            si += q;
        }
        ((sr / n as f32).powi(2) + (si / n as f32).powi(2)).sqrt()
    }

    /// The sign convention: a POSITIVE offset shifts DOWN. Offset tuning parks
    /// the LO low so the station sits at +OFF, and this is what brings it to 0.
    /// Getting this backwards moves the station to -2*OFF and you hear nothing.
    #[test]
    fn positive_offset_shifts_down() {
        assert!(
            shifted_mean(OFF, OFF, 24000) > 0.99,
            "a tone at +offset must land on DC"
        );
        assert!(
            shifted_mean(-OFF, OFF, 24000) < 0.01,
            "a tone at -offset must NOT land on DC — that would be the wrong sign"
        );
    }

    /// The LO spike sits at DC before the shift; afterwards it must be well
    /// away from the channel, not merely somewhere else.
    #[test]
    fn dc_spike_is_moved_clear_of_the_channel() {
        assert!(
            shifted_mean(0.0, OFF, 24000) < 0.01,
            "DC must not stay at DC"
        );
        // and it lands at exactly -OFF: undoing the shift brings it back.
        assert!(shifted_mean(0.0, 0.0, 24000) > 0.99);
    }

    /// The phasor is stepped by repeated complex multiplication, which drifts
    /// in f32. `advance` renormalises every 512 samples; over a full USB
    /// transfer the magnitude must stay put or the audio gets slow AM.
    #[test]
    fn phasor_magnitude_does_not_drift() {
        let mut x = Xlator::new(OFF, FS);
        for _ in 0..8192 {
            x.process_sample(1.0, 0.0);
        }
        let m = (x.phase.real.powi(2) + x.phase.img.powi(2)).sqrt();
        assert!((m - 1.0).abs() < 1e-4, "phasor magnitude drifted to {m}");
    }
}

#[cfg(test)]
mod stereo_tests {
    use super::*;

    const FS: f32 = 300_000.0;
    const TONE_HZ: f32 = 1_000.0;

    /// Build a stereo MPX with a 1 kHz tone on `left`/`right` at the given
    /// amplitudes, `pilot` injection, and a pilot slightly off nominal and
    /// starting at an arbitrary phase — what a real dongle with ppm error hands
    /// the loop. Phase is accumulated in f64 so the test signal itself has no
    /// float drift over a second of samples.
    fn mpx(n: usize, left: f32, right: f32, pilot: f32) -> Vec<f32> {
        let (mut th, mut ph) = (1.3f64, 0.0f64);
        let dth = std::f64::consts::TAU * 19_000.7 / FS as f64;
        let dph = std::f64::consts::TAU * TONE_HZ as f64 / FS as f64;
        (0..n)
            .map(|_| {
                let a = ph.sin() as f32;
                let (l, r) = (left * a, right * a);
                let x = 0.5 * (l + r)
                    + pilot * (th.sin() as f32)
                    + 0.5 * (l - r) * ((2.0 * th).sin() as f32);
                th += dth;
                ph += dph;
                x
            })
            .collect()
    }

    /// Run `x` through the same chain `DSPFlow` uses — PLL, then twin 4/25
    /// resamplers — and return (L, R) at 48 kHz.
    fn decode(x: &[f32], pll: &mut PilotPll) -> (Vec<f32>, Vec<f32>) {
        let mut taps = create_taps::<1901>(
            FilterType::Lowpass,
            15_000.0,
            1_200_000.0,
            Window::Kaiser,
            None,
        );
        taps.iter_mut().for_each(|h| *h *= 4.0);
        let mut rs_sum = PolyphaseResampler::new(4, 25, &taps);
        let mut rs_diff = PolyphaseResampler::new(4, 25, &taps);

        let (mut l, mut r) = (Vec::new(), Vec::new());
        let mut diff = [0.0f32; 1024];
        let (mut s48, mut d48) = ([0.0f32; 164], [0.0f32; 164]);
        for block in x.chunks_exact(1024) {
            pll.process(block, &mut diff);
            let n = rs_sum.process(block, &mut s48);
            assert_eq!(n, rs_diff.process(&diff, &mut d48));
            for k in 0..n {
                l.push(s48[k] + d48[k]);
                r.push(s48[k] - d48[k]);
            }
        }
        (l, r)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn locks_and_separates_left_only() {
        let mut pll = PilotPll::new(FS, 50.0, 100.0);
        let x = mpx(300 * 1024, 0.8, 0.0, 0.09); // ~1 s
        let (l, r) = decode(&x, &mut pll);

        assert!(pll.is_stereo(), "a 9 % pilot must light the indicator");
        let level = pll.pilot_level();
        assert!((level - 0.09).abs() < 0.01, "pilot level read {level}");

        // Last quarter second, well after lock and the blend ramp.
        let tail = l.len() - 12_000;
        let (lr, rr) = (rms(&l[tail..]), rms(&r[tail..]));
        let sep_db = 20.0 * (lr / rr).log10();
        println!("separation {sep_db:.1} dB, pilot {level:.4}");
        // 0.8 peak → 0.566 RMS; the left channel must come out at full level.
        assert!((lr - 0.566).abs() < 0.03, "left RMS {lr}");
        assert!(sep_db > 30.0, "separation only {sep_db:.1} dB");
    }

    #[test]
    fn right_only_lands_on_the_right() {
        let mut pll = PilotPll::new(FS, 50.0, 100.0);
        let x = mpx(300 * 1024, 0.0, 0.8, 0.09);
        let (l, r) = decode(&x, &mut pll);
        let tail = l.len() - 12_000;
        assert!(
            rms(&r[tail..]) > 10.0 * rms(&l[tail..]),
            "matrix sign is backwards"
        );
    }

    /// No pilot: stay mono, emit a silent L−R, and so leave the old mono
    /// output untouched.
    #[test]
    fn no_pilot_stays_mono() {
        let mut pll = PilotPll::new(FS, 50.0, 100.0);
        let x = mpx(300 * 1024, 0.8, 0.0, 0.0);
        let (l, r) = decode(&x, &mut pll);
        assert!(!pll.is_stereo());
        let tail = l.len() - 12_000;
        let d: Vec<f32> = l[tail..].iter().zip(&r[tail..]).map(|(a, b)| a - b).collect();
        assert!(rms(&d) < 1e-4, "L and R must be identical in mono");
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
        let max_asymmetry = (0..n / 2)
            .map(|k| (taps[k] - taps[n - 1 - k]).abs())
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
        let filter_hann = create_taps::<51>(
            FilterType::Lowpass,
            3000.0f32,
            sample_rate,
            Window::Hann,
            None,
        );
        assert_filter(&filter_hann);
    }
}
