//! The Info panel: everything the radio *reports*.
//!
//! The mirror of [`ControlView`](super::control_view::ControlView) — nothing
//! here is settable, and every row is read out of
//! [`Health`](super::app_states::Health) or off a rate the device chose. That
//! split is the whole point: PLAN.md R1.5 notes that the old panel showed
//! "Freq/Step/Gain/BW/PPM — every one an input, none a measurement", which is
//! how a radio ends up with no way to tell whether a gain change helped.
//!
//! ows reading `—` are not placeholders for layout. They are measurements
//! nothing writes yet ([`UNMEASURED`]), and they render as a dash rather than a
//! confident `0` so the panel never claims the radio is healthy on the strength
//! of an uninitialised counter. R1.3 fills in the drop/lap/underrun row, R1.5
//! the RSSI.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering::Relaxed;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Widget};

use crate::tui::colors::{self, CYAN_GLOW, ORANGE_BASE, ORANGE_DANGER, ORANGE_GLOW};

use super::tui_states::{Health, UNMEASURED};
use crate::source::source::IQ_SLOTS;

/// Rows plus border. Kept beside the row list for the same reason
/// [`control_view::HEIGHT`](super::control_view::HEIGHT) is.
pub const HEIGHT: u16 = 8 + 2;

/// Span of the RSSI bar, in dBFS. Fixed rather than the waterfall's
/// `[floor, ceil]`: RSSI is *channel* power, ~22 dB above the per-bin level
/// the waterfall paints, so a shared scale would pin the bar at full. With
/// 0 dBFS at the top, a full bar means the ADC is about to clip.
const RSSI_BAR_DB: (f32, f32) = (-60.0, 0.0);

pub struct InfoView {
    center_freq: Rc<Cell<u32>>,
    tuned_freq: Rc<Cell<u32>>,
    sample_rate: Rc<Cell<u32>>,
    audio_rate: Rc<Cell<u32>>,
    health: Health,
}

impl InfoView {
    pub fn new(
        center_freq: Rc<Cell<u32>>,
        tuned_freq: Rc<Cell<u32>>,
        sample_rate: Rc<Cell<u32>>,
        audio_rate: Rc<Cell<u32>>,
        health: Health,
    ) -> Self {
        Self {
            center_freq,
            tuned_freq,
            sample_rate,
            audio_rate,
            health,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .style(colors::pane_card())
            .title("Info")
            .title_style(colors::pane_title())
            .border_style(colors::pane_border(false));
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.is_empty() {
            return;
        }

        // Label column plus whatever is left for the value. The bars fill
        // that remainder, so the width is needed before the rows are built
        // rather than per row.
        const LABEL_W: u16 = 10;
        let value_w = inner.width.saturating_sub(LABEL_W) as usize;

        let plain = |s: String| Line::styled(s, Style::new().fg(colors::TEXT));
        let rssi = self.health.rssi_dbfs_x10.load(Relaxed);
        let audio_lag = self.health.ring_audio_lag.load(Relaxed);
        let fft_lag = self.health.ring_fft_lag.load(Relaxed);
        let rows = [
            (
                "Rate",
                plain(format!("{:.3} MS/s", self.sample_rate.get() as f64 / 1e6)),
            ),
            (
                "Audio",
                plain(format!("{} kHz", self.audio_rate.get() / 1000)),
            ),
            // Both, because they are independent now: the LO is where the
            // dongle is looking and the centre of the span, the tuned frequency
            // is the channel picked out of it.
            (
                "LO",
                plain(format!("{:.3} MHz", self.center_freq.get() as f64 / 1e6)),
            ),
            (
                "Tuned",
                plain(format!("{:.3} MHz", self.tuned_freq.get() as f64 / 1e6)),
            ),
            ("RSSI", rssi_line(rssi, value_w)),
            ("Audio Lag", lag_line(audio_lag, value_w)),
            ("FFT Lag", lag_line(fft_lag, value_w)),
            (
                "Underrun",
                plain(format!("{}", self.health.underruns.load(Relaxed))),
            ),
        ];

        // One terminal row per entry, leftover height left blank rather than
        // stretched — a readout reads as a list, not as evenly spread lines. In
        // a pane too short for all of them the trailing rows come back
        // zero-height and `Line::render` skips them on its own.
        let areas = Layout::vertical([Constraint::Length(1); 8]).split(inner);

        for (r, (label, value)) in areas.iter().zip(rows) {
            let [label_area, value_area] =
                Layout::horizontal([Constraint::Length(LABEL_W), Constraint::Fill(1)]).areas(*r);

            Line::styled(label, Style::new().fg(colors::LABEL)).render(label_area, buf);
            value.right_aligned().render(value_area, buf);
        }
    }
}

/// `███████▋░░░░░  -45.3` — a bar over [`RSSI_BAR_DB`] followed by the
/// reading, or a dash before the DSP has written one.
fn rssi_line(tenths: i32, width: usize) -> Line<'static> {
    if tenths == UNMEASURED {
        return Line::styled("\u{2014}", Style::new().fg(colors::TEXT));
    }
    let dbfs = tenths as f32 / 10.0;
    let (floor, ceil) = RSSI_BAR_DB;
    let t = (dbfs - floor) / (ceil - floor);
    bar_line(t, format!(" {dbfs:>6.1}"), rssi_color(dbfs), width)
}

/// Consumer lag in blocks over the ring's [`IQ_SLOTS`]: a full bar is one
/// write from being lapped. Both rings are `IQ_SLOTS` deep, which is what lets
/// one scale serve both rows.
fn lag_line(lag: isize, width: usize) -> Line<'static> {
    let t = lag as f32 / IQ_SLOTS as f32;
    bar_line(t, format!(" {lag:>3}"), lag_color(t), width)
}

/// A bar filling `width` less the reading, which is kept at a fixed width by
/// the caller so the bar's right edge does not jitter as the digits change.
fn bar_line(t: f32, reading: String, color: Color, width: usize) -> Line<'static> {
    let bar_w = width.saturating_sub(reading.len());
    Line::from(vec![
        Span::styled(hbar(t, bar_w), Style::new().fg(color)),
        Span::styled(reading, Style::new().fg(colors::TEXT)),
    ])
}

/// Horizontal bar of `width` cells filled from the left to `t` in `0..=1`,
/// at ⅛-cell resolution using the left-block glyphs. The unfilled remainder
/// is blank, padded to `width` so the reading after it stays put.
fn hbar(t: f32, width: usize) -> String {
    const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let cells = t.clamp(0.0, 1.0) * width as f32;
    let full = (cells as usize).min(width);
    let frac = ((cells - full as f32) * 8.0) as usize;

    let mut s = "█".repeat(full);
    if full < width {
        s.push_str(EIGHTHS[frac.min(7)]);
        s.push_str(&" ".repeat(width - full - 1));
    }
    s
}

/// Lag colour by fraction of the ring: calm while there is headroom, danger
/// once a lap is imminent.
fn lag_color(t: f32) -> Color {
    match t {
        x if x < 0.5 => CYAN_GLOW,
        x if x < 0.875 => ORANGE_GLOW,
        _ => ORANGE_DANGER,
    }
}

/// Text colors for RSSI
fn rssi_color(rssi: f32) -> Color {
    match rssi {
        x if x >= -10.0 => CYAN_GLOW,
        x if (-30.0..-10.0).contains(&x) => ORANGE_GLOW, 
        x if (-60.0..-30.0).contains(&x) => ORANGE_BASE,
        x if x <= -60.0 => ORANGE_DANGER,
        _ => ORANGE_DANGER
    }
}
