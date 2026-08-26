use std::array;

use std::cell::UnsafeCell;

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

/// 3-stage decimation for WFM
/// Input: [f32; 8192] ~ 2.4MHz
/// Stage 1: 27 taps, step size 4, output of 2 [f32; 2048] ~ 600kHz
/// Stage 2: 69 taps, step size 2, output of 2 [f32; 1024] ~ 300kHz
/// Stage 3: 152 taps, step size 1, channel LFP (lowpass filter)
pub struct DecimationWFM {
    stage_1: FIRFilter<8218, 27>,   // 26 + 8192, 27 taps, 2048 output
    stage_2: FIRFilter<2116, 69>,   // 68 + 2048, 69 taps, 1024 output
    stage_3: FIRFilter<1175, 152>,   // 151 + 1024, 152 taps, 1024 output
}

impl DecimationWFM {
    pub fn new() -> Self {
        let lfp_taps = Self::lowpass::<152>(75_000_f32, 7_500_f32, 300_000_f32);
        Self {
            stage_1: FIRFilter::<8218, 27>::new(&TAPS_27, 8192),
            stage_2: FIRFilter::<2116, 69>::new(&TAPS_69, 2048),
            stage_3: FIRFilter::<1175, 152>::new(&lfp_taps, 1024),
        }
    }

    pub fn process(
        &mut self,
        i: &[f32],
        q: &[f32],
        out_i: &mut [f32; 1024],
        out_q: &mut [f32; 1024],
    ) {
        assert_eq!(8192, i.len());
        assert_eq!(8192, q.len());
        // Returned number of sample processed
        self.stage_1.read_to_buf(i, q);

        // Write output directly to next stage buf
        let _sample_count_1 = self.stage_1.process(
            4, 2048,
            &mut self.stage_2.buf_i[68..],
            &mut self.stage_2.buf_q[68..]
            );
        let _sample_count_2 = self.stage_2.process(
            2, 1024,
            &mut self.stage_3.buf_i[151..],
            &mut self.stage_3.buf_q[151..]
            );
        let _sample_count_3 = self.stage_3.process(1, 1024, out_i, out_q);
    }

    fn sinc(x: f32) -> f32 {
        if x == 0.0 { 1.0 } else { x.sin() / x }
    }

    fn nuttall(n: f32, big_n: f32) -> f32 {
        use std::f32::consts::PI;
        0.355768
            - 0.487396 * (2.0 * PI * n / big_n).cos()
            + 0.144232 * (4.0 * PI * n / big_n).cos()
            - 0.012604 * (6.0 * PI * n / big_n).cos()
    }

    /// Create a lowpass taps
    fn lowpass<const T: usize>(cutoff: f32, trans: f32, sample_rate: f32) -> [f32; T] {
        let count = (3.8 * sample_rate / trans ) as usize;      // Nuttall main-lobe width
        let omega = 2.0 * std::f32::consts::PI * cutoff / sample_rate;
        let half  = count as f32 / 2.0;
        let corr  = omega / std::f32::consts::PI;     // DC-gain normalization

        array::from_fn(|i| {
            let t = i as f32 - half + 0.5;            // half-sample offset
            Self::sinc(t * omega) * Self::nuttall(t - half, count as f32) * corr
        })
    }
}

impl Default for DecimationWFM {
    fn default() -> Self {
        Self::new()
    }
}

/// A struct holding I and Q channel as separated array so vectorization is possible.
/// A buffer channel I/Q of hold history of past
/// For example:
/// - Input channel I having size of 8192, output having size of 2048
/// - Taps length is 27
/// - History part having size 26, new input part having size of 8192,
///   together they form an array of 26 + 8192
/// - New input is written into buffer from index 26
///
/// In the signature of struct:
/// - T: tap size, for example 27 26 + 8192
/// - N: input size
/// - B: T - 1 + input size
pub struct FIRFilter<const B: usize, const T: usize> {
    taps: [f32; T],
    buf_i: [f32; B],
    buf_q: [f32; B],
    input_size: usize,
    offset: usize,
}

impl<const B: usize, const T: usize> FIRFilter<B, T> {
    pub fn new(taps: &[f32; T], input_size: usize) -> Self {
        assert_eq!(B, T - 1 + input_size);
        Self {
            taps: *taps,
            buf_i: array::from_fn(|_| 0.0f32),
            buf_q: array::from_fn(|_| 0.0f32),
            input_size,
            offset: 0,
        }
    }

    /// Copy into buf from index T - 1
    pub fn read_to_buf(&mut self, i: &[f32], q: &[f32]) {
        // Copy inputs into buf first
        let hist = B - self.input_size;
        self.buf_i[hist..hist + self.input_size].copy_from_slice(i);
        self.buf_q[hist..hist + self.input_size].copy_from_slice(q);
    }

    /// Decimation includes multipliying a window with an array of taps,
    /// then sum up to extract a sample
    /// N is the input size, e.g. 8192, M is output size, e.g. 2048, T is taps size 27
    pub fn process(
        &mut self,
        step_size: usize,
        output_size: usize,
        // i: &[f32],
        // q: &[f32],
        out_i: &mut [f32],
        out_q: &mut [f32],
        ) -> usize
    {
        // assert_eq!(self.input_size, out_i.len());
        assert_eq!(output_size, out_i.len());
        assert_eq!(output_size, out_q.len());
        let hist = B - self.input_size;

        // An input of 8192 with step 4 produces 2048 samples
        let mut count = 0;
        while self.offset < self.input_size {
            let w = self.offset..self.offset + T;
            out_i[count] = Self::dot(&self.buf_i[w.clone()], &self.taps);
            out_q[count] = Self::dot(&self.buf_q[w.clone()], &self.taps);
            count += 1;
            self.offset += step_size;
        };

        // Carry leftover step into next block
        self.offset -= self.input_size;

        // Retain last T-1 samples for next process
        self.buf_i.copy_within(self.input_size..self.input_size+hist, 0);
        self.buf_q.copy_within(self.input_size..self.input_size+hist, 0);
        count
    }

    /// Written like this for vectorization with `target-cpu=native` build option
    fn dot(x: &[f32], y: &[f32]) -> f32 {
        assert!(x.len() == y.len());
        let mut acc_0 = 0.0f32;
        let mut acc_1 = 0.0f32;
        let mut acc_2 = 0.0f32;
        let mut acc_3 = 0.0f32;

        let chunk = x.len()/4;
        for i in 0..chunk {
            let base = i*4;
            acc_0 += x[base] * y[base];
            acc_1 += x[base + 1] * y[base + 1];
            acc_2 += x[base + 2] * y[base + 2];
            acc_3 += x[base + 3] * y[base + 3];
        };

        let mut acc_tail = 0.0f32;
        let remainder_start = chunk * 4;
        for i in remainder_start..x.len() {
            acc_tail += x[i] * y[i]
        };

        acc_0 + acc_1 + acc_2 + acc_3 + acc_tail

    }

    pub fn set_iq(&mut self, idx: usize, i: f32, q: f32) {
        (self.buf_i[idx], self.buf_q[idx]) = (i, q);
    }
}

pub struct Demodulation {
    prev_i: f32,
    prev_q: f32,
}

impl Demodulation {
    pub fn new() -> Self {
        Self { prev_i: 0.0f32, prev_q: 0.0f32 }
    }

    pub fn process<const N: usize>
        (&mut self, i: &[f32; N], q: &[f32; N], out: &mut [f32; N])
    {
        i.iter().zip(q.iter()).zip(out.iter_mut()).for_each(|((&x, &y), o)| {
            let re = x * self.prev_i + y * self.prev_q;
            let im = y * self.prev_i - x * self.prev_q;
            *o = im.atan2(re); // ∈ [-π, π]
            self.prev_i = x;
            self.prev_q = y;
        });
    }
}

impl Default for Demodulation {
    fn default() -> Self {
        Self::new()
    }
}


/// A sinc function to reconstruct continuous sample from discrete one
pub fn sinc(x: f32) -> f32 {
    if x.abs() < 1e-6 { 1.0 }
    else { (std::f32::consts::PI * x).sin() / (std::f32::consts::PI * x) }
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

    for n in 0..output_len {
        let pos = n as f32 * ratio;
        let mut sum = 0.0f32;

        for k in -(half_taps as i32)..=(half_taps as i32) {
            let i = pos as i32 + k;
            if i < 0 || i >= input.len() as i32 { continue; }

            let t = pos - i as f32;
            // Sinc × window — the low-pass interpolation kernel
            sum += input[i as usize] * sinc(t) * hann(t, half_taps as f32);
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

    #[test]
    fn test_wrapped() {
        let size: usize = 16;
        assert_eq!(next_wrapped(1, size), 2usize);
        assert_eq!(next_wrapped(15, size), 0);
    }
}
