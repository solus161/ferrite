//! State that never leaves the UI thread.
//!
//! The counterpart to [`AppStates`](super::app_states::AppStates): nothing here
//! is a radio setting, so nothing here needs an atomic. `Rc<Cell<_>>` because
//! two widgets read the same value — the colour range is edited in the Control
//! panel and consumed by [`SignalView`](super::signal_view::SignalView), and a
//! shared cell keeps them from drifting the way a copied `f32` would.
//!
//! The Control panel's cursor is deliberately *not* here: it indexes
//! `Field::ALL`, so it belongs with the row table it indexes.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use std::sync::atomic::{AtomicI32, AtomicU64};

use super::utils::get_attr_clone;

/// Sentinel for a measurement nothing has written yet, so the Info panel can
/// render "—" instead of a confident zero.
pub const UNMEASURED: i32 = i32::MIN;

/// Which pane the arrow keys are talking to.
///
/// The signal view is not focusable — it has nothing to select. `Tab` cycles
/// the two that do.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Control,
    Log,
}

impl Pane {
    pub fn next(self) -> Self {
        match self {
            Pane::Control => Pane::Log,
            Pane::Log => Pane::Control,
        }
    }
}

pub struct TuiStates {
    // ── RF ──────────────────────────────────────────────────────────────────
    pub sample_rate: Rc<Cell<u32>>,
    pub audio_rate: Rc<Cell<u32>>,
    pub center_freq: Rc<Cell<u32>>,

    /// The channel actually demodulated, which the `Xlator` brings down to DC.
    ///
    /// Distinct from `center_freq`: that one is the LO programmed into the
    /// dongle and the centre of the waterfall, while this one is a channel
    /// anywhere within `TUNE_SPAN_HZ` of it.
    ///
    /// Held at a fixed *offset* from the centre rather than at a fixed absolute
    /// frequency: moving `center_freq` moves this by the same delta, so the
    /// cursor keeps its place on the waterfall and the translator offset never
    /// changes. What is demodulated does move with the centre.
    pub tuned_freq: Rc<Cell<u32>>,

    pub step: Rc<Cell<u32>>,
    /// librtlsdr's units, and always a value from the tuner's own table — the
    /// UI steps that table rather than whole dB, so what is displayed is what
    /// the hardware has.
    pub gain_tenths: Rc<Cell<i32>>,
    pub agc: Rc<Cell<bool>>,
    /// Channel bandwidth. The tuner's IF filter is set wider than this to clear
    /// the offset-tuning gap; see `source::source`.
    pub bandwidth: Rc<Cell<u32>>,
    /// Signed: most dongles need a negative correction.
    pub ppm: Rc<Cell<i32>>,

    // ── Audio ───────────────────────────────────────────────────────────────
    /// 0..=100, applied in the cpal callback.
    pub volume: Rc<Cell<f32>>,
    pub muted: Rc<Cell<bool>>,
    /// De-emphasis time constant in µs: 50 outside the Americas and South
    /// Korea, 75 inside.
    pub deemph_us: Rc<Cell<u32>>,

    pub mode: Rc<Cell<TunerMode>>,

    // ── Measured ────────────────────────────────────────────────────────────
    pub health: Arc<Health>,
    pub focus: Rc<Cell<Pane>>,

    /// Bottom of the colour range, in dB. Shared with the signal view.
    pub floor_db: Rc<Cell<f32>>,
    /// Top of the colour range, in dB.
    pub ceil_db: Rc<Cell<f32>>,

    /// Lines the log panel is scrolled back from the newest. 0 follows the
    /// tail, which is what you want while the radio is running.
    pub log_scroll: Rc<Cell<usize>>,
}

impl TuiStates {
    pub fn new(
        sample_rate: u32,
        audio_rate: u32,
        center_freq: u32,
        tuned_freq: u32,
        step: u32,
        gain_tenths: i32,
        bandwidth: u32,
        ppm: i32,
        floor_db: f32,
        ceil_db: f32,
    ) -> Self {
        Self {
            sample_rate: Rc::new(Cell::new(sample_rate)),
            audio_rate: Rc::new(Cell::new(audio_rate)),
            center_freq: Rc::new(Cell::new(center_freq)),
            tuned_freq: Rc::new(Cell::new(tuned_freq)),
            step: Rc::new(Cell::new(step)),
            gain_tenths: Rc::new(Cell::new(gain_tenths)),
            agc: Rc::new(Cell::new(true)),
            bandwidth: Rc::new(Cell::new(bandwidth)),
            ppm: Rc::new(Cell::new(ppm)),
            volume: Rc::new(Cell::new(1.0)),
            muted: Rc::new(Cell::new(false)),
            deemph_us: Rc::new(Cell::new(50)),
            mode: Rc::new(Cell::new(TunerMode::WbFm)),
            health: Arc::new(Health::new()),
            focus: Rc::new(Cell::new(Pane::Control)),
            floor_db: Rc::new(Cell::new(floor_db)),
            ceil_db: Rc::new(Cell::new(ceil_db)),
            log_scroll: Rc::new(Cell::new(0)),
        }
    }

    get_attr_clone!(sample_rate, u32);
    get_attr_clone!(audio_rate, u32);
    get_attr_clone!(center_freq, u32);
    get_attr_clone!(tuned_freq, u32);
    get_attr_clone!(gain_tenths, i32);
    get_attr_clone!(floor_db, f32);
    get_attr_clone!(ceil_db, f32);

    pub fn set_mode(&self, mode: TunerMode) {
        self.mode.set(mode);
    }
}

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
    pub fn new() -> Self {
        Self {
            iq_drops: AtomicU64::new(0),
            iq_laps: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            rssi_dbfs_x10: AtomicI32::new(UNMEASURED),
            snr_db_x10: AtomicI32::new(UNMEASURED),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TunerMode {
    WbFm, // Wide-band FM
    Nfm,  // Narrow-band FM
    Am,   // Amplitude modulation
    Usb,  // Upper sideband
    Lsb,  // Lower sideband
    Raw,  // No demodulation
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
