//! Radio state shared across threads.
//!
//! Everything the *radio* has an opinion about lives here, behind one `Arc`
//! with plain atomics inside rather than a field-per-`Arc`. Two directions of
//! travel, and the split is what decides which panel renders a value:
//!
//! - **UI → radio.** The tuner and audio settings. The UI writes them and sends
//!   a [`CtrlSignal`](sdr_core::control_signal::CtrlSignal) when a device round
//!   trip is needed; settings the DSP applies itself (volume, mute,
//!   de-emphasis) are read straight off the atomic instead, so nothing on the
//!   audio path has to drain a channel. Rendered by the **Control** panel.
//! - **radio → UI.** [`Health`] — counters and measurements written by the DSP
//!   and USB threads. Rendered by the **Info** panel.
//!
//! Purely-visual state (cursor position, colour range, log scroll) is *not*
//! here — it never leaves the UI thread and lives in
//! [`TuiStates`](super::tui_states::TuiStates).
//!
//! All accesses are `Relaxed`. These are independent scalars read for display
//! or applied on the next block; none of them orders any other memory, and the
//! rings carry their own `Acquire`/`Release` pairs for the data that does.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};

/// Demodulation mode.
///
/// Only [`WbFm`](TunerMode::WbFm) is implemented — `DSPFlow` is welded to that
/// chain. The rest render dimmed and are selectable so the shape of the control
/// is right before PLAN.md R3.0 makes them real.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TunerMode {
    WbFm = 0, // Wide-band FM
    Nfm = 1,  // Narrow-band FM
    Am = 2,   // Amplitude modulation
    Usb = 3,  // Upper sideband
    Lsb = 4,  // Lower sideband
    Raw = 5,  // No demodulation
}

impl TunerMode {
    pub const ALL: [TunerMode; 6] = [
        TunerMode::WbFm,
        TunerMode::Nfm,
        TunerMode::Am,
        TunerMode::Usb,
        TunerMode::Lsb,
        TunerMode::Raw,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TunerMode::WbFm => "WFM",
            TunerMode::Nfm => "NFM",
            TunerMode::Am => "AM",
            TunerMode::Usb => "USB",
            TunerMode::Lsb => "LSB",
            TunerMode::Raw => "RAW",
        }
    }

    /// Whether the DSP can actually run this mode today.
    pub fn implemented(self) -> bool {
        matches!(self, TunerMode::WbFm)
    }

    fn from_u8(v: u8) -> Self {
        Self::ALL
            .get(v as usize)
            .copied()
            .unwrap_or(TunerMode::WbFm)
    }
}

/// Sentinel for a measurement nothing has written yet, so the Info panel can
/// render "—" instead of a confident zero.
pub const UNMEASURED: i32 = i32::MIN;

/// Written by the DSP and USB threads, read by the UI.
///
/// Counters are the *only* way the hot path is allowed to report anything —
/// see the logging rules in [`crate::log`].
pub struct Health {
    /// Blocks the IQ ring producer could not place (PLAN.md R1.3).
    pub iq_drops: AtomicU64,
    /// Times a consumer was lapped and had to jump forward.
    pub iq_laps: AtomicU64,
    /// cpal callbacks that found the audio ring empty and emitted silence.
    pub underruns: AtomicU64,
    /// Tenths of a dBFS, or [`UNMEASURED`]. PLAN.md R1.5.
    pub rssi_dbfs_x10: AtomicI32,
    /// Tenths of a dB, or [`UNMEASURED`]. PLAN.md R1.5.
    pub snr_db_x10: AtomicI32,
}

impl Health {
    fn new() -> Self {
        Self {
            iq_drops: AtomicU64::new(0),
            iq_laps: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            rssi_dbfs_x10: AtomicI32::new(UNMEASURED),
            snr_db_x10: AtomicI32::new(UNMEASURED),
        }
    }
}

pub struct AppStates {
    // ── RF ──────────────────────────────────────────────────────────────────
    pub sample_rate: AtomicU32,
    pub audio_rate: AtomicU32,
    pub center_freq: AtomicU32,
    /// librtlsdr's units, and always a value from the tuner's own table — the
    /// UI steps that table rather than whole dB, so what is displayed is what
    /// the hardware has.
    pub gain_tenths: AtomicI32,
    pub agc: AtomicBool,
    /// Channel bandwidth. The tuner's IF filter is set wider than this to clear
    /// the offset-tuning gap; see `source::source`.
    pub bandwidth: AtomicU32,
    /// Signed: most dongles need a negative correction.
    pub ppm: AtomicI32,

    // ── Audio ───────────────────────────────────────────────────────────────
    /// 0..=100, applied in the cpal callback.
    pub volume: AtomicU32,
    pub muted: AtomicBool,
    /// De-emphasis time constant in µs: 50 outside the Americas and South
    /// Korea, 75 inside.
    pub deemph_us: AtomicU32,

    pub mode: AtomicU8,

    // ── Measured ────────────────────────────────────────────────────────────
    pub health: Health,
}

impl AppStates {
    pub fn new(
        sample_rate: u32,
        audio_rate: u32,
        center_freq: u32,
        gain_tenths: i32,
        bandwidth: u32,
        ppm: i32,
    ) -> Self {
        Self {
            sample_rate: AtomicU32::new(sample_rate),
            audio_rate: AtomicU32::new(audio_rate),
            center_freq: AtomicU32::new(center_freq),
            gain_tenths: AtomicI32::new(gain_tenths),
            agc: AtomicBool::new(false),
            bandwidth: AtomicU32::new(bandwidth),
            ppm: AtomicI32::new(ppm),
            volume: AtomicU32::new(70),
            muted: AtomicBool::new(false),
            deemph_us: AtomicU32::new(50),
            mode: AtomicU8::new(TunerMode::WbFm as u8),
            health: Health::new(),
        }
    }

    pub fn mode(&self) -> TunerMode {
        TunerMode::from_u8(self.mode.load(Relaxed))
    }

    pub fn set_mode(&self, mode: TunerMode) {
        self.mode.store(mode as u8, Relaxed);
    }

    /// Linear scale for the cpal callback: volume as a fraction, or zero when
    /// muted. Squared so the control tracks loudness rather than amplitude —
    /// a linear fader spends most of its travel in the top few dB.
    pub fn audio_scale(&self) -> f32 {
        if self.muted.load(Relaxed) {
            return 0.0;
        }
        let v = self.volume.load(Relaxed) as f32 / 100.0;
        v * v
    }
}
