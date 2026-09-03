//! The Info panel: everything the radio *reports*.
//!
//! The mirror of [`ControlView`](super::control_view::ControlView) — nothing
//! here is settable, and every row is read out of
//! [`Health`](super::app_states::Health) or off a rate the device chose. That
//! split is the whole point: PLAN.md R1.5 notes that the old panel showed
//! "Freq/Step/Gain/BW/PPM — every one an input, none a measurement", which is
//! how a radio ends up with no way to tell whether a gain change helped.
//!
//! Rows reading `—` are not placeholders for layout. They are measurements
//! nothing writes yet ([`UNMEASURED`]), and they render as a dash rather than a
//! confident `0` so the panel never claims the radio is healthy on the strength
//! of an uninitialised counter. R1.3 fills in the drop/lap/underrun row, R1.5
//! the RSSI and SNR.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Widget};

use crate::source::source::OFFSET_TUNING_HZ;
use crate::tui::control_view::pane_border;

use super::tui_states::{Health, UNMEASURED};

/// Rows plus border. Kept beside the row list for the same reason
/// [`control_view::HEIGHT`](super::control_view::HEIGHT) is.
pub const HEIGHT: u16 = 7 + 2;

pub struct InfoView {
    center_freq: Rc<Cell<u32>>,
    sample_rate: Rc<Cell<u32>>,
    audio_rate: Rc<Cell<u32>>,
    health: Arc<Health>,
}

impl InfoView {
    pub fn new(
        center_freq: Rc<Cell<u32>>,
        sample_rate: Rc<Cell<u32>>,
        audio_rate: Rc<Cell<u32>>,
        health: Arc<Health>,
    ) -> Self {
        Self {
            center_freq,
            sample_rate,
            audio_rate,
            health,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .title("Info")
            .border_style(pane_border(false));
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.is_empty() {
            return;
        }

        // The LO is parked below the station (see `source::source`), so it is
        // never the tuned frequency and there is otherwise nothing on screen
        // that says where the dongle is actually looking.
        let lo = self.center_freq.get().saturating_sub(OFFSET_TUNING_HZ);

        let rows = [
            (
                "Rate",
                format!("{:.3} MS/s", self.sample_rate.get() as f64 / 1e6),
            ),
            ("Audio", format!("{} kHz", self.audio_rate.get() / 1000)),
            ("LO", format!("{:.3} MHz", lo as f64 / 1e6)),
            (
                "RSSI",
                measured(self.health.rssi_dbfs_x10.load(Relaxed), "dBFS"),
            ),
            ("SNR", measured(self.health.snr_db_x10.load(Relaxed), "dB")),
            (
                "Drop/lap",
                format!(
                    "{} / {}",
                    self.health.iq_drops.load(Relaxed),
                    self.health.iq_laps.load(Relaxed)
                ),
            ),
            (
                "Underrun",
                format!("{}", self.health.underruns.load(Relaxed)),
            ),
        ];

        // One terminal row per entry, leftover height left blank rather than
        // stretched — a readout reads as a list, not as evenly spread lines. In
        // a pane too short for all of them the trailing rows come back
        // zero-height and `Line::render` skips them on its own.
        let areas = Layout::vertical([Constraint::Length(1); 7]).split(inner);

        for (r, (label, value)) in areas.iter().zip(rows) {
            let [label_area, value_area] =
                Layout::horizontal([Constraint::Length(9), Constraint::Fill(1)]).areas(*r);

            Line::styled(label, Style::new().fg(Color::DarkGray)).render(label_area, buf);
            Line::from(value).right_aligned().render(value_area, buf);
        }
    }
}

/// A measurement in tenths, or a dash when nothing has written it yet.
fn measured(tenths: i32, unit: &str) -> String {
    if tenths == UNMEASURED {
        "\u{2014}".into()
    } else {
        format!("{:.1} {unit}", tenths as f32 / 10.0)
    }
}
