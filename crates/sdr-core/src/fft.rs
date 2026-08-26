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
    window_sum: f32,        // depends on type of window
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
            .for_each(|x| *x = x.reverse_bits() >> usize::BITS - N.trailing_zeros());

        // Precomputed Hann window
        // https://www.mathworks.com/help/signal/ref/hann.html
        //
        // Periodic (DFT-even) form: the denominator is N, and k stops at N-1.
        // MATLAB's `hann(L)` is the *symmetric* window — denominator L-1, L
        // points — which is right for filter design and wrong here: it repeats
        // its endpoint in the DFT's implied periodic extension and leaves a
        // residue in every bin. `hann(L,'periodic')` is this one.
        let hann_window: [f32; N] = array::from_fn(|k| {
            0.5*(1.0 - (2.0*core::f32::consts::PI*k as f32 / N as f32).cos())
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
        let mut buf_a: [ComplexF32; N] = array::from_fn(|i| 
            self.samples[self.indexes_rev[i]] * self.hann_window[self.indexes_rev[i]]
            );
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
            };

            core::mem::swap(&mut src, &mut dst);
            n *= 2;
        };
        
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
        self.output.rotate_left(N/2);

        // let scale = 1.0/(N as f32);
        let scale = 1.0/self.window_sum;
        self.output.iter_mut().for_each(|o| {
            *o = 20.0 * (*o*scale + 1e-12).log10();
        });
    }

    /// Update freq to match with center freq
    fn abs_freq(&mut self, center_freq: f32, sample_rate: f32) {
        self.freq.iter_mut().enumerate().for_each(|(i, f)| {
            let offset = i as f32 - (N/2) as f32;
            let freq = offset*(sample_rate/N as f32);
            *f = center_freq + freq
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

    const N: usize = 2048;

    /// Shifted index of a tone `bins` away from centre. Negative is to the
    /// left of DC — a distinction that only exists because the input is
    /// complex; real samples fold the two sides onto each other.
    fn shifted(bins: i32) -> usize {
        (N as i32 / 2 + bins) as usize
    }

    /// Fill `dst` with interleaved u8 IQ for a complex exponential of amplitude
    /// `amp` sitting exactly `bins` bins from centre.
    ///
    /// `start` is the absolute sample index, so chunked pushes stay phase
    /// continuous. The quantisation is the exact inverse of `push`'s decode, so
    /// a round trip recovers `amp` to within one 1/127.5 step.
    fn fill_tone(dst: &mut [u8], bins: f32, amp: f32, start: usize) {
        for (n, pair) in dst.chunks_exact_mut(2).enumerate() {
            let theta = 2.0 * core::f32::consts::PI * bins * (start + n) as f32 / N as f32;
            let q = |x: f32| (x * amp * 127.5 + 127.5).round().clamp(0.0, 255.0) as u8;
            pair[0] = q(theta.cos());
            pair[1] = q(theta.sin());
        }
    }

    /// Feed exactly one window in `chunk`-byte pushes and return the spectrum.
    fn analyse(bins: f32, amp: f32, chunk: usize) -> Vec<f32> {
        let mut fft = Fft::<N>::new();
        let mut buf = vec![0u8; chunk];
        let pairs = chunk / 2;

        let mut fired = None;
        for c in 0..(N / pairs) {
            fill_tone(&mut buf, bins, amp, c * pairs);
            if let Some(spectrum) = fft.push(&buf) {
                fired = Some(spectrum.to_vec());
            }
        }
        fired.expect("N/pairs pushes should fill the window exactly once")
    }

    fn peak_bin(spectrum: &[f32]) -> usize {
        spectrum
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(bin, _)| bin)
            .unwrap()
    }

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

    #[test]
    fn positive_and_negative_tones_land_on_opposite_sides() {
        // The whole payoff of complex input. Fed real samples these two tones
        // are the same bin and no cursor, scale or shift could separate them.
        assert_eq!(peak_bin(&analyse(256.0, 1.0, 4096)), shifted(256));
        assert_eq!(peak_bin(&analyse(-256.0, 1.0, 4096)), shifted(-256));
    }

    #[test]
    fn full_scale_tone_reads_zero_dbfs() {
        // A complex exponential puts all of its energy in one bin at |X| = A*N,
        // so the 1/N scale in post_process reads back A directly. No factor of
        // two: there is no conjugate twin to fold in.
        let peak = analyse(256.0, 1.0, 4096)[shifted(256)];
        assert!(peak.abs() < 0.5, "expected ~0 dBFS, got {peak}");
    }

    #[test]
    fn half_scale_tone_reads_minus_six_db() {
        // 20*log10(0.5) = -6.02. Proves 1/N is a real amplitude normalisation
        // and not a constant that merely happens to look right at full scale.
        let peak = analyse(256.0, 0.5, 4096)[shifted(256)];
        assert!((peak + 6.02).abs() < 0.5, "expected ~-6 dBFS, got {peak}");
    }

    #[test]
    fn energy_is_confined_to_the_tone() {
        // A bin-centred exponential is analytically zero in every other bin
        // under a *rectangular* window. The Hann window trades that for a
        // three-bin main lobe: its coefficients are 0.25 / 0.5 / 0.25, so the
        // two immediate neighbours sit at exactly 20*log10(0.5) = -6.02 dB and
        // everything beyond them is 8-bit quantisation noise spread over N bins
        // — around -83 dBFS. -40 leaves wide margin while still catching a
        // broken twiddle or a botched butterfly.
        //
        // Asserting the -6.02 rather than merely skipping the neighbours is
        // what pins the window down: a mis-parenthesised or symmetric-instead-
        // of-periodic Hann changes those two bins and nothing else visible.
        let spectrum = analyse(256.0, 1.0, 4096);
        let peak = shifted(256);
        for (bin, &db) in spectrum.iter().enumerate() {
            match bin as i32 - peak as i32 {
                0 => continue,
                -1 | 1 => assert!(
                    (db + 6.02).abs() < 0.1,
                    "bin {bin} is a Hann skirt, expected ~-6.02 dB, got {db}"
                ),
                _ => assert!(db < -40.0, "bin {bin} should be noise floor, got {db}"),
            }
        }
    }

    #[test]
    fn off_bin_tone_does_not_smear_across_the_display() {
        // The test the window exists for. Every other case here uses a
        // bin-centred tone, where a rectangular window is already near-perfect
        // — none of them would fail if the window were removed entirely.
        //
        // 256.5 bins is the worst case: exactly halfway between two bins, so
        // the analysis sees a truncated sinusoid whose ends do not match, and
        // the discontinuity is what radiates across the spectrum.
        //
        // Measured worst bin beyond a given distance from the peak:
        //
        //     beyond +-   rectangular    Hann
        //          4        -23.0 dB   -48.7 dB
        //          8        -28.5 dB   -66.4 dB
        //        100        -49.9 dB   -69.0 dB  <- quantisation floor
        //        300        -58.2 dB   -69.0 dB
        //
        // Rectangular rolls off at 6 dB/octave and is still 58 dB up three
        // hundred bins away, which is the whole width of the display; Hann
        // rolls off at 18 dB/octave and has reached the 8-bit noise floor by
        // +-12. The two thresholds below sit ~4 and ~6 dB under the Hann
        // numbers, and 22 and 32 dB above the rectangular ones — so this fails
        // loudly if the window is ever removed, mis-parenthesised, or built
        // symmetric instead of periodic.
        let spectrum = analyse(256.5, 1.0, 4096);
        let peak = shifted(256) as i32;
        for (bin, &db) in spectrum.iter().enumerate() {
            match (bin as i32 - peak).abs() {
                0..=4 => continue,
                5..=8 => assert!(db < -45.0, "near skirt: bin {bin} leaked to {db}"),
                _ => assert!(db < -60.0, "far skirt: bin {bin} leaked to {db}"),
            }
        }
    }

    #[test]
    fn nyquist_tone_is_the_leftmost_column() {
        // exp(i*pi*n) = (-1)^n lands wholly in bin N/2, which the rotate sends
        // to index 0 — the negative Nyquist edge. That bin is its own mirror,
        // so unlike every other tone it has no partner to share energy with.
        assert_eq!(peak_bin(&analyse(N as f32 / 2.0, 1.0, 4096)), 0);
    }

    #[test]
    fn chunked_pushes_reassemble_one_window() {
        // 4 x 512 pairs must agree with a single 2048-pair push: `offset` has
        // to carry the fill position across calls without dropping a sample.
        let whole = analyse(256.0, 1.0, 4096);
        let split = analyse(256.0, 1.0, 1024);
        assert_eq!(peak_bin(&whole), peak_bin(&split));
        assert!((whole[shifted(256)] - split[shifted(256)]).abs() < 0.1);
    }

    #[test]
    fn window_only_fires_when_full() {
        let mut fft = Fft::<N>::new();
        let buf = [127u8; 1024]; // 512 IQ pairs at mid-scale
        assert!(fft.push(&buf).is_none());
        assert!(fft.push(&buf).is_none());
        assert!(fft.push(&buf).is_none());
        assert!(
            fft.push(&buf).is_some(),
            "4 x 512 pairs should complete the 2048 window"
        );
    }
}
