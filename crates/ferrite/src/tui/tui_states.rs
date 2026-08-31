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
use std::time::{Duration, Instant};

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

/// How long a status message stays on the bottom bar before the key hints come
/// back. Long enough to read, short enough that the bar is not stale.
const STATUS_TTL: Duration = Duration::from_secs(3);

pub struct TuiStates {
    pub focus: Pane,

    /// Bottom of the colour range, in dB. Shared with the signal view.
    pub floor_db: Rc<Cell<f32>>,
    /// Top of the colour range, in dB.
    pub ceil_db: Rc<Cell<f32>>,

    /// Lines the log panel is scrolled back from the newest. 0 follows the
    /// tail, which is what you want while the radio is running.
    pub log_scroll: usize,

    status: Option<(String, Instant)>,
}

impl TuiStates {
    pub fn new(floor_db: f32, ceil_db: f32) -> Self {
        Self {
            focus: Pane::Control,
            floor_db: Rc::new(Cell::new(floor_db)),
            ceil_db: Rc::new(Cell::new(ceil_db)),
            log_scroll: 0,
            status: None,
        }
    }

    /// Post a transient message to the bottom bar. The log keeps the history;
    /// the bar only ever shows the latest.
    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Instant::now()));
    }

    pub fn status(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STATUS_TTL)
            .map(|(t, _)| t.as_str())
    }
}
