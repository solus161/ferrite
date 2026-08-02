use core::array;
use core::ops::{Add, Mul};

/// Calculating the discrete Fourier Transform over a sliding window of N samples.
///
/// The input is not fed all at once, but in pieces of f32 which add up to N,
/// so `offset` tracks the insert point and decides when the window is full.
///
/// The transform is driven by a table of N twiddle factors rather than a full
/// N by N DFT matrix: because the matrix entry at (i, j) is only ever
/// `cis(-2*pi*((i*j) % N)/N)`, the matrix holds just N distinct values.
pub struct Fft<const N: usize> {
    samples: [f32; N],
    offset: usize,
    twiddles: [ComplexF32; N],
    pub output: [f32; N],
}

impl<const N: usize> Fft<N> {
    pub fn new() -> Self {
        let samples = array::from_fn(|_| 0f32);

        // Borrow this from Reducible's video at https://www.youtube.com/watch?v=h7apO7q16V0
        // Fill the twiddle factors, we only need N angle within full circle 2*pi
        // not full N by N matrix
        let twiddles: [ComplexF32; N] = array::from_fn(|k| {
            let angle = -2.0 * core::f32::consts::PI * (k as f32) / (N as f32);
            ComplexF32::cis(angle)
        });

        let output = array::from_fn(|_| 0f32);

        Self {
            samples,
            offset: 0,
            twiddles,
            output,
        }
    }

    /// Continuously copy array to fill samples
    /// return coeffs array when samples filled
    pub fn push(&mut self, src: &[f32]) -> Option<&[f32]> {
        let src_len = src.len();
        let max_index = N.min(self.offset + src_len);
        for i in self.offset..max_index {
            self.samples[i] = src[i - self.offset]
        }
        self.offset = max_index;
        if self.offset >= N {
            self.dft_fwd();
            self.offset = 0;
            Some(&self.output)
        } else {
            None
        }
    }

    /// Calculate coefficients using fowards DFT, radix-2 FFT
    /// The Cooley Tukey algo process a samples by splits it to even and odd-index values
    /// and process them recursively.
    ///
    /// For example a sample having indexes of
    ///  0  1	 2  3  4  5  6  7  8  9 10 11	12 13	14 15
    /// The splitted sequences of even-odd are
    ///  0  2  4  6  8 10 12 14| 1  3  5  7  9 11 13 15
    ///  0  4  8 12| 2  6 10 14| 1  5  9 13| 3  7 11 15
    ///  0  8| 4 12| 2 10| 6 14| 1  9| 5 13| 3 11| 7 15
    ///  0| 8| 4|12| 2|10| 6|14| 1| 9| 5|13| 3|11| 7|15
    ///
    ///  Call the fn combining a pair of even-odd f(e, o, k/n),
    ///  k/n is the power factor of w=-2*pi*i (this != the w in Reducible video)
    ///
    ///  The process starts from the deepest level,
    ///  [3]: x[0] = f( 0, 8, 1/2), x[1] = f( 0, 8, 2/2)
    ///  [3]: x[2] = f( 4,12, 1/2), x[3] = f( 4,12, 2/2)
    ///  ..
    ///  [2]: x[0] = f( 0, 4, 1/4), x[2] = f( 8,12, 2/4), x[3] = f( 0, 4, 3/4), x[4] = f( 8,12, 4/4)
    ///  ..
    ///  [0]: x[0] = f( 0, 1, 1/16), x[1] = f( 2, 3, 2/16), x[3] = f( 4, 5, 3/16), x[4] = f( 6, 7, 4/16)
    ///  
    ///  First step: bit-reversed to get the deepest level
    ///  https://brianmcfee.net/dstbook-site/content/ch08-fft/FFT.html
    pub fn dft_fwd(&mut self) {
        // self.output.iter_mut().enumerate()
        //     .for_each(|(i, o)| {
        //         let p = self.samples.iter().enumerate().fold(ComplexF32::default(), |acc, (j, p)| {
        //             let t = self.twiddles[(i*j)%N];
        //             acc + ComplexF32::new(t.real*p, t.img*p)
        //         });
        //         *o = p.norm();
        //     });
        let mut indexes: [usize; N] = array::from_fn(|i| i);

        // Bit-reversed
        indexes
            .iter_mut()
            .for_each(|x| *x = x.reverse_bits() >> usize::BITS - N.trailing_zeros());

        // Also reorder sample
        let mut output_cpx: [ComplexF32; N] = array::from_fn(|i| {
            let i = indexes[i];
            ComplexF32 {
                real: self.samples[i],
                img: 0.0,
            }
        });

        // A stage reads every slot of the previous stage, so it cannot write in
        // place: slot e_id is both the source of E[0] and the destination of
        // X[0], and X[n/2] still needs E[0] long after X[0] overwrote it.
        // Writing into scratch keeps the two roles in separate arrays, the way
        // the recursion gets a fresh output array at every level for free.
        let mut scratch: [ComplexF32; N] = array::from_fn(|_| ComplexF32::default());

        // Size of a sequence of both even and odd: 2, 4, 8, 16, 32, 64, 128, etc
        let mut n = 2usize;
        while n <= N {
            let mut e_id = 0usize;
            while e_id < N {
                for i in 0..n {
                    let i_alias = i % (n / 2);

                    // Example with N = 2048, n = 4
                    // twiddes indexes are 0, 512, 1024, 1536
                    let tw = self.twiddles[i * N / n];

                    // The even sequence starts at e_id
                    // The odd sequence starts at e_id + n/2
                    scratch[e_id + i] =
                        output_cpx[e_id + i_alias] + tw * output_cpx[e_id + n / 2 + i_alias];
                }
                // Move up sequence id
                e_id += n;
            }
            // This stage's results become the next stage's E and O
            output_cpx = scratch;
            n *= 2;
        }

        // Extract to output
        self.output
            .iter_mut()
            .zip(output_cpx)
            .for_each(|(o, c)| *o = c.norm());
    }
}

/// A struct for complex number, using f32 by default
#[derive(Copy, Clone, Default)]
#[repr(C)]
struct ComplexF32 {
    pub real: f32,
    pub img: f32,
}

impl ComplexF32 {
    pub const fn new(real: f32, img: f32) -> Self {
        Self { real, img }
    }

    pub fn cis(theta: f32) -> Self {
        let (img, real) = theta.sin_cos();
        Self { real, img }
    }

    pub fn norm(self) -> f32 {
        self.real.hypot(self.img)
    }
}

impl Add for ComplexF32 {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.real + rhs.real, self.img + rhs.img)
    }
}

impl Mul for ComplexF32 {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        Self::new(
            self.real * rhs.real - self.img * rhs.img,
            self.real * rhs.img + self.img * rhs.real,
        )
    }
}

/// Testing the FFT
#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 2048;

    /// Sample rate equal to N makes the window exactly 1 second, so bin index == Hz
    /// (resolution is SAMPLE_RATE / N = 1 Hz). Nyquist is 1024 Hz.
    const SAMPLE_RATE: f32 = N as f32;

    /// A real tone of amplitude 1 splits into a mirrored pair, each of magnitude N/2.
    const PEAK: f32 = N as f32 / 2.0;

    /// Fill `dst` with a unit-amplitude sine at `freq_hz`, continuing the waveform from
    /// absolute sample index `start` so chunk boundaries stay phase-continuous.
    fn fill_sine(dst: &mut [f32], freq_hz: f32, start: usize) {
        for (i, s) in dst.iter_mut().enumerate() {
            let t = (start + i) as f32 / SAMPLE_RATE;
            *s = (2.0 * core::f32::consts::PI * freq_hz * t).sin();
        }
    }

    /// Bin of the largest coefficient below Nyquist.
    fn peak_bin(coeffs: &[f32]) -> usize {
        coeffs[..coeffs.len() / 2]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(bin, _)| bin)
            .unwrap()
    }

    /// Feed a tone through the window in `chunk`-sized pushes and return the one spectrum.
    fn analyse(freq_hz: f32, chunk: usize) -> Vec<f32> {
        let mut fft = Fft::<N>::new();
        let mut buf = vec![0f32; chunk];
        let mut fired = None;
        for c in 0..(N / chunk) {
            fill_sine(&mut buf, freq_hz, c * chunk);
            if let Some(coeffs) = fft.push(&buf) {
                fired = Some(coeffs.to_vec());
            }
        }
        fired.expect("N/chunk pushes should fill the window exactly once")
    }

    #[test]
    fn test_bit_reversed() {
        let mut indexes: [usize; 16] = array::from_fn(|i| i);
        indexes
            .iter_mut()
            .for_each(|i| *i = i.reverse_bits() >> 8 * 7 + 4);
        println!("Bit-reversed: {:?}", &indexes);
        assert_eq!(
            indexes,
            [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15]
        );
    }

    #[test]
    fn tone_512hz_in_512_sample_chunks() {
        let coeffs = analyse(512.0, 512);
        assert_eq!(peak_bin(&coeffs), 512);
        assert!(
            (coeffs[512] - PEAK).abs() < 1.0,
            "bin 512 = {}",
            coeffs[512]
        );
        assert!(
            (coeffs[N - 512] - PEAK).abs() < 1.0,
            "mirror = {}",
            coeffs[N - 512]
        );
    }

    #[test]
    fn tone_768hz_in_1024_sample_chunks() {
        let coeffs = analyse(768.0, 1024);
        assert_eq!(peak_bin(&coeffs), 768);
        assert!(
            (coeffs[768] - PEAK).abs() < 1.0,
            "bin 768 = {}",
            coeffs[768]
        );
    }

    #[test]
    fn energy_is_confined_to_the_tone() {
        let coeffs = analyse(512.0, 512);
        for (bin, &m) in coeffs.iter().enumerate() {
            if bin == 512 || bin == N - 512 {
                continue;
            }
            assert!(m < 1.0, "bin {bin} should be silent, got {m}");
        }
    }

    #[test]
    fn window_only_fires_when_full() {
        let mut fft = Fft::<N>::new();
        let buf = [0f32; 512];
        assert!(fft.push(&buf).is_none());
        assert!(fft.push(&buf).is_none());
        assert!(fft.push(&buf).is_none());
        assert!(
            fft.push(&buf).is_some(),
            "4 x 512 should complete the 2048 window"
        );
    }

    #[test]
    fn nyquist_sine_is_silent() {
        // sin(2*pi*1024*j/2048) == sin(pi*j) == 0 for every integer sample, so a sine
        // exactly at Nyquist carries no energy at all. Documented here because the
        // obvious "test a 1024 Hz tone" analyses silence and passes for the wrong reason.
        let coeffs = analyse(1024.0, 1024);
        assert!(
            coeffs.iter().all(|&m| m < 1.0),
            "sin at Nyquist is all zeros"
        );
    }
}
