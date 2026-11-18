use core::array;

use crate::complex::ComplexF32;

/// Calculating the discrete Fourier Transform over a sliding window of N samples.
///
/// The input is not fed all at once, but in pieces of f32 which add up to N,
/// so `offset` tracks the insert point and decides when the window is full.
///
/// The transform is driven by a table of N twiddle factors rather than a full
/// N by N DFT matrix: because the matrix entry at (i, j) is only ever
/// `cis(-2*pi*((i*j) % N)/N)`, the matrix holds just N distinct values.
pub struct Fft<const N: usize> {
    samples: [ComplexF32; N],
    offset: usize,
    twiddles: [ComplexF32; N],
    output: [f32; N],
    freq: [f32; N],
    indexes_rev: [usize; N],
    /// Real-valued: a window scales magnitude without rotating phase, so
    /// storing it as `ComplexF32` would pay a full complex product (4 mul,
    /// 2 add) per sample to multiply by a zero imaginary part.
    hann_window: [f32; N],
    window_sum: f32, // depends on type of window
}

impl<const N: usize> Fft<N> {
    pub fn new() -> Self {
        let samples = array::from_fn(|_| ComplexF32::default());

        // Borrow this from Reducible's video at https://www.youtube.com/watch?v=h7apO7q16V0
        // Fill the twiddle factors, we only need N angle within full circle 2*pi
        // not full N by N matrix
        let twiddles: [ComplexF32; N] = array::from_fn(|k| {
            let angle = -2.0 * core::f32::consts::PI * (k as f32) / (N as f32);
            ComplexF32::cis(angle)
        });

        let output = array::from_fn(|_| 0f32);
        let freq = array::from_fn(|_| 0f32);

        // Bit-reversed
        let mut indexes_rev: [usize; N] = array::from_fn(|i| i);
        indexes_rev
            .iter_mut()
            .for_each(|x| *x = x.reverse_bits() >> (usize::BITS - N.trailing_zeros()));

        // Precomputed Hann window
        // https://www.mathworks.com/help/signal/ref/hann.html
        //
        // FFT requires periodic signal, this one forces sample edges convert to zero.
        // That will prevent abrupt signal changes around edges. These abrupt changes 
        // requires extra frequencies/leakages to approximate the original signals.
        let hann_window: [f32; N] = array::from_fn(|k| {
            0.5 * (1.0 - (2.0 * core::f32::consts::PI * k as f32 / N as f32).cos())
        });

        // Coherent gain. Normalising by this instead of N is what keeps an
        // on-bin full-scale tone reading 0 dBFS — a window that averages 0.5
        // would otherwise cost a flat 6 dB. Summed rather than hardcoded to
        // 2/N so swapping in Hamming or Blackman-Harris needs no other change.
        let window_sum: f32 = hann_window.iter().sum();

        Self {
            samples,
            offset: 0,
            twiddles,
            output,
            freq,
            indexes_rev,
            hann_window,
            window_sum,
        }
    }

    /// Continuously copy array to fill samples, return coeffs array when samples filled
    /// `iq` is stream of IQ f32
    /// `iq` length must be even and all `iq` must make up the the samples size,
    /// Consumers may still work, but you'll lose data
    pub fn push(&mut self, iq: &[f32]) -> Option<&[f32]> {
        // let src_len = src.len();
        // let max_index = N.min(self.offset + src_len/2);
        // for i in self.offset..max_index {
        //     if i*2 + 1 > src_len { /* TODO: silenced error here */};
        //     let i_idx = (i - self.offset)*2;
        //     let q_idx = i_idx+1;
        //     self.samples[i] = ComplexF32 { real: src[i_idx] as f32, img: src[q_idx] as f32 };
        // }
        for pair in iq.chunks_exact(2) {
            self.samples[self.offset] = ComplexF32 {
                real: pair[0],
                img: pair[1],
            };
            self.offset += 1;
        }
        if self.offset == N {
            self.dft_fwd();
            self.post_process();
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
        // Keep sample intact
        // We need 2 bufs for this step
        // - one for reversed samples
        // - another for calculation, as inplace modification will break the calculation
        let mut buf_a: [ComplexF32; N] = array::from_fn(|i| {
            self.samples[self.indexes_rev[i]] * self.hann_window[self.indexes_rev[i]]
        });
        let mut buf_b: [ComplexF32; N] = array::from_fn(|_| ComplexF32::default());

        // This is for swapping two bufs when done
        let (mut src, mut dst) = (&mut buf_a, &mut buf_b);

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
                    dst[e_id + i] = src[e_id + i_alias] + tw * src[e_id + n / 2 + i_alias];
                }
                // Move up sequence id
                e_id += n;
            }

            core::mem::swap(&mut src, &mut dst);
            n *= 2;
        }

        self.output
            .iter_mut()
            .zip(src.iter())
            .for_each(|(o, c)| *o = c.norm());
    }

    /// Convert output to db
    /// Rotate the spectrum, sample rate 2.048MHz
    /// k =  0           1       2          N/2          N/2+1          N-1
    /// From 0Hz         1,000Hz 2,000Hz .. 1,024,000Hz -1,203,00Hz .. -1,000Hz
    /// To   -1,024,00Hz ..                 0Hz                         1,023,000Hz
    ///
    /// Then convert to decibel
    pub fn post_process(&mut self) {
        self.output.rotate_left(N / 2);

        let scale = 1.0 / self.window_sum;
        self.output.iter_mut().for_each(|o| {
            *o = 20.0 * (*o * scale + 1e-12).log10();
        });
    }
}

/// Testing the FFT
///
/// Everything here speaks the post-`push` contract: raw interleaved u8 IQ in,
/// a spectrum that is already fftshift'd and in dBFS out. Index `N / 2` is DC.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bit_reversed() {
        let mut indexes: [usize; 16] = array::from_fn(|i| i);
        indexes
            .iter_mut()
            .for_each(|i| *i = i.reverse_bits() >> 8 * 7 + 4);
        assert_eq!(
            indexes,
            [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15]
        );
    }
}
