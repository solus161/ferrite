/// A high-pass filter for leaked energy at the center freq
struct DcBlocker {
    x_prev: f32,
    y_prev: f32,
    a: f32
}

impl DcBlocker {
    fn new() -> Self {
        Self { x_prev: 0.0, y_prev: 0.0, a: 0.999 }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = x - self.x_prev + self.a*self.y_prev;
        self.x_prev = x;
        self.y_prev = y;
        y
    }
}

/// That high-pass filter for both I & Q channel
pub struct IqDcBlocker {
    i_blocker: DcBlocker,
    q_blocker: DcBlocker,
}

impl IqDcBlocker {
    pub fn new() -> Self {
        Self { i_blocker: DcBlocker::new(), q_blocker: DcBlocker::new() }
    }

    pub fn process(&mut self, i: f32, q: f32) -> (f32, f32) {
        (
            self.i_blocker.process(i),
            self.q_blocker.process(q),
        )
    }
}

/// Decimation
pub struct Decim<const IQ_DECIM: u32, const POST_DECIM: u32> {
    i_acc: f32,
    q_acc: f32,
    iq_n: u32,
    post_acc: f32,
    post_n: u32,
}

impl<const IQ_DECIM: u32, const POST_DECIM: u32> Decim<IQ_DECIM, POST_DECIM> {
    pub fn new() -> Self {
        Self { i_acc: 0.0, q_acc: 0.0, iq_n: 0, post_acc: 0.0, post_n: 0 }
    }

    /// Stage 1: Decimation of IQ stream, down sampled by `IQ_DECIM` times
    /// If `IQ_DECIM = 10`, SDR sampling rate of 2.4MS/s -> 240kS/s
    /// I and Q are averaged over `IQ_DECIM` samples before `atan2` in demod phase,
    /// so any adjacent station still present when
    /// it runs intermodulates and can never be unmixed again.
    pub fn iq_decim(&mut self, i: f32, q: f32) -> Option<(f32, f32)> {
        self.i_acc += i;
        self.q_acc += q;
        self.iq_n += 1;  

        if self.iq_n < IQ_DECIM {
            return None
        };

        let (i, q) = (self.i_acc / IQ_DECIM as f32, self.q_acc / IQ_DECIM as f32);
        self.i_acc = 0.0;
        self.q_acc = 0.0;
        self.iq_n = 0;
        Some((i, q))
    }

    /// Stage 2: further decimation to audio rate, 240kS/s -> 48KHz
    pub fn post_decim(&mut self, x: f32, volume: f32) -> Option<f32> {
        self.post_acc += x;
        self.post_n += 1;
        if self.post_n < POST_DECIM {
            return None
        };

        // Clamped last, and only last: nothing guarantees the
        // deviation stays within spec — noise on a weak signal
        // and over-modulated stations both push past it — but
        // clamping before the filters would distort what they
        // are about to shape.
        let sample = ((self.post_acc / POST_DECIM as f32) * volume).clamp(-1.0, 1.0);
        self.post_acc = 0.0;
        self.post_n = 0;
        Some(sample)
    }
}

pub struct Deemphasis {
    state: f32,
    alpha: f32,
}

impl Deemphasis {
    pub fn new(rate: f32, tau: f32) -> Self {
        Self { state: 0.0, alpha: 1.0 - (-1.0 / (rate * tau)).exp() }
    }

    pub fn process(&mut self, x: f32) -> f32 {
        self.state += self.alpha * (x - self.state);
        self.state
    }
}

/// FM discriminator: angle of cur × conj(prev), normalised so that peak
/// deviation maps to ±1.0. The ±π range of atan2 corresponds to ±fs/2,
/// which no broadcast signal comes anywhere near — normalising by π
/// instead would lose ~24 dB.
pub struct FmDiscriminator {
    prev_i: f32,
    prev_q: f32,
    // Phase step at full deviation, at the rate the discriminator now runs.
    // 2π·75k/240k = 1.96 rad — still under π, but that shrinking headroom is
    // why stage 1 cannot decimate much further than 240 kS/s. Normalising by
    // π instead would treat the deviation as ±fs/2 and lose ~24 dB.
    full_scale: f32,
}

impl FmDiscriminator {
  pub fn new(rate: f32, deviation: f32) -> Self {
      let full_scale = 2.0 * std::f32::consts::PI * deviation / rate;

      // Both ends of this are silent failures, which is why they are asserted
      // rather than commented. A zero/garbage `rate` makes `full_scale`
      // infinite, and `atan2(..) / inf` is exactly 0.0 — the demodulator emits
      // digital silence with no panic and no NaN to trace. Too small a `rate`
      // pushes full deviation past the ±π that `atan2` can represent, and every
      // modulation peak wraps to the opposite rail.
      assert!(
          full_scale.is_finite() && full_scale < 0.8 * std::f32::consts::PI,
          "discriminator rate {rate} Hz leaves no headroom for ±{deviation} Hz \
           deviation (full_scale {full_scale} rad); raise the rate or lower IQ_DECIM"
      );

      Self { prev_i: 0.0, prev_q: 0.0, full_scale }
  }

  pub fn process(&mut self, i: f32, q: f32) -> f32 {
      let re = i * self.prev_i + q * self.prev_q;
      let im = q * self.prev_i - i * self.prev_q;
      self.prev_i = i;
      self.prev_q = q;
      im.atan2(re) / self.full_scale
  }
}

/// AM envelope detector. The carrier appears as a constant term in the
/// envelope, so a slow one-pole tracks it and gets subtracted — that same
/// value is the natural full-scale, since envelope/carrier is exactly the
/// modulation index. Output is ±1.0 at 100 % modulation, no separate AGC.
pub struct AmEnvelope {
  carrier: f32,
  alpha: f32,
}
  
impl AmEnvelope {
  /// `cutoff_hz` should sit below the lowest audio you want to keep
  /// (~20 Hz) but above the fading rate. Same step-invariant form as the
  /// de-emphasis pole.
  pub fn new(rate: f32, cutoff_hz: f32) -> Self {
      let tau = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz);
      Self { carrier: 0.0, alpha: 1.0 - (-1.0 / (rate * tau)).exp() }
  }

  pub fn process(&mut self, i: f32, q: f32) -> f32 {
      let env = (i * i + q * q).sqrt();
      self.carrier += self.alpha * (env - self.carrier);
      (env - self.carrier) / self.carrier.max(1e-6)
  }
}
