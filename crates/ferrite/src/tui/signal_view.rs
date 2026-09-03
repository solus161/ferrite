//! Scrolling spectrogram widget.
//!
//! Time runs down the screen, frequency across it, magnitude as colour. Rows
//! are stored at full bin resolution and folded onto columns at render time, so
//! a terminal resize re-folds the existing history instead of discarding it.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::{array, usize};

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Widget};

/// Upper-half block. The foreground paints the top half of the cell and the
/// background the bottom, so every terminal row carries two waterfall lines.
const HALF: char = '\u{2580}';
const EIGHT_BLOCK: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

/// Inferno-ish stops. Monotone in lightness, which is what keeps a weak signal
/// legible against the noise floor and survives a grayscale screenshot.
const STOPS: [[f32; 3]; 5] = [
    [0.0, 0.0, 4.0],
    [87.0, 16.0, 110.0],
    [188.0, 55.0, 84.0],
    [249.0, 142.0, 9.0],
    [252.0, 255.0, 164.0],
];

pub struct SignalView {
    bins: usize,
    /// Newest first. Each row is one fftshift'd, dB-scaled spectrum.
    rows: VecDeque<Box<[f32]>>,
    history: usize,
    /// Peak-hold accumulator. The FFT produces spectra far faster than we
    /// render, so several fold into the one row a frame is allowed to commit.
    pending: Box<[f32]>,
    has_pending: bool,

    /// A pair of (current, old), so rerender MHz only at changes.
    center_freq: (Rc<Cell<u32>>, u32),

    /// Sample rate is constant
    sample_rate: Rc<Cell<u32>>,

    /// Colour range. These two decide whether the display looks like anything.
    /// Shared with the Control panel, which is where they are edited — a copied
    /// `f32` would let the two drift.
    /// This `floor_db` and `ceil_db` are tuple of (updated value, old value)
    /// so rerender happens only at values changed
    floor_db: (Rc<Cell<f32>>, f32),
    ceil_db: (Rc<Cell<f32>>, f32),
    /// Read for the frequency axis only; the spectra themselves arrive through
    /// `push`.

    /// 4 segments, 5 values
    mhz_xaxis_labels: [f32; 6],

    /// Preallocated arrays labels for spectrum db axis
    /// 5 segments, 6 values
    spec_yaxis_labels: [f32; 6],

    /// Preallocated arrays labels for waterfall timelapse axis
    /// 5 segments, 6 values
    water_yaxis_labels: [f32; 6],
}

impl SignalView {
    pub fn new(
        bins: usize,
        history: usize,
        center_freq: Rc<Cell<u32>>,
        sample_rate: Rc<Cell<u32>>,
        floor_db: Rc<Cell<f32>>,
        ceil_db: Rc<Cell<f32>>,
    ) -> Self {
        let center_freq_old = center_freq.get();
        let floor_db_old = floor_db.get();
        let ceil_db_old = ceil_db.get();

        // Generate labels

        Self {
            bins,
            rows: VecDeque::with_capacity(history),
            history,
            pending: vec![f32::NEG_INFINITY; bins].into_boxed_slice(),
            has_pending: false,
            center_freq: (center_freq, center_freq_old),
            sample_rate,
            floor_db: (floor_db, floor_db_old),
            ceil_db: (ceil_db, ceil_db_old),
            mhz_xaxis_labels: array::from_fn(|_| 0.0f32),
            spec_yaxis_labels: array::from_fn(|_| 0.0f32),
            water_yaxis_labels: array::from_fn(|_| 0.0f32),
        }
    }

    /// Fold one spectrum into the pending row.
    ///
    /// Peak, not average: averaging a batch of spectra buries a narrow carrier
    /// back in the noise floor. Taking the max is also the only reduction that
    /// stays correct on values that are already in dB — `max` commutes with a
    /// monotonic function, `mean` does not. Averaging would have to happen on
    /// linear magnitudes, before `post_process` takes the log.
    pub fn push(&mut self, spectrum: &[f32]) {
        for (p, &s) in self.pending.iter_mut().zip(spectrum) {
            *p = p.max(s);
        }
        self.has_pending = true;
    }

    /// Promote the pending row to the newest line. Call once per rendered
    /// frame — that, not the FFT rate, is what sets the scroll speed.
    ///
    /// Does nothing when no spectrum arrived: a stalled DSP should freeze the
    /// display rather than scroll blank rows over real data.
    pub fn commit(&mut self) {
        if !self.has_pending {
            return;
        }

        // The evicted row's allocation becomes the next accumulator, so a
        // steady-state frame allocates nothing.
        let mut recycled = match self.rows.len() {
            n if n >= self.history => self.rows.pop_back().unwrap(),
            _ => vec![0.0; self.bins].into_boxed_slice(),
        };
        recycled.fill(f32::NEG_INFINITY);

        self.rows
            .push_front(std::mem::replace(&mut self.pending, recycled));
        self.has_pending = false;
    }

    /// dB to RGB across `[floor_db, ceil_db]`, clamped at both ends.
    fn color(&self, db: f32) -> Color {
        let (floor, ceil) = (self.floor_db.0.get(), self.ceil_db.0.get());
        let span = (ceil - floor).max(f32::EPSILON);
        let t = ((db - floor) / span).clamp(0.0, 1.0);

        let s = t * (STOPS.len() - 1) as f32;
        let i = (s as usize).min(STOPS.len() - 2);
        let f = s - i as f32;
        let (a, b) = (STOPS[i], STOPS[i + 1]);

        Color::Rgb(
            (a[0] + (b[0] - a[0]) * f) as u8,
            (a[1] + (b[1] - a[1]) * f) as u8,
            (a[2] + (b[2] - a[2]) * f) as u8,
        )
    }

    /// Peak dB over the bins folded onto column `x` of `width`.
    ///
    /// Peak rather than mean for the same reason as [`push`](Self::push): at
    /// 2048 bins across ~100 columns each column covers ~20 bins, and the mean
    /// of 19 noise bins and one carrier is noise.
    fn column(&self, row: &[f32], x: usize, width: usize) -> f32 {
        let lo = x * self.bins / width;
        let hi = ((x + 1) * self.bins / width).max(lo + 1);
        row[lo..hi]
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max)
    }

    /// This is called when app stats is updated in parent tui
    pub fn update_tui_stats(&mut self) {}

    /// Generate axis labels, given a center value.
    /// `range_half` is max distance from `center` to either sides.
    fn gen_axis_labels_center(center: f32, range_half: f32, out: &mut [f32]) {
        assert_eq!(out.len() % 2, 1);
        let mark_count = out.len();
        let bins_half = mark_count / 2;
        let bin_size: f32 = range_half / bins_half as f32;

        // Median labels
        out[bins_half + 1] = center;

        // Fill lower
        for i in 0..bins_half {
            let fr_center = bin_size * ((bins_half - i) as f32);
            out[i] = center - fr_center;
            out[mark_count - i - 1] = center + fr_center;
        }
    }

    /// Generate axis labels, given lower and upper bound
    fn gen_axis_labels_bound(lower: f32, upper: f32, out: &mut [f32]) {
        let mark_count = out.len();
        let bin_count = mark_count - 1;
        let bin_size: f32 = (upper - lower) / bin_count as f32;
        out[0] = lower;
        out[mark_count - 1] = upper;

        for i in 1..mark_count - 1 {
            out[i] = lower + bin_size * i as f32;
        }
    }

    /// Computer waterfall mhz axis marks
    /// `force = true` means recalculate marks always
    /// else check for change before calculation
    fn gen_mhz_xaxis_labels(&mut self, force: bool) {
        let current_freq = self.center_freq.0.get();
        if force || current_freq != self.center_freq.1 {
            Self::gen_axis_labels_center(
                self.center_freq.0.get() as f32,
                self.sample_rate.get() as f32 / 2.0,
                &mut self.mhz_xaxis_labels[..],
            );
        };
        self.center_freq.1 = current_freq;
    }

    /// Compute spectrum db axis mark
    pub fn gen_spec_yaxis_label(&mut self, force: bool) {
        let current_floor = self.floor_db.0.get();
        let current_ceil = self.ceil_db.0.get();
        if force || current_floor != self.floor_db.1 || current_ceil != self.ceil_db.1 {
            Self::gen_axis_labels_bound(
                current_floor,
                current_ceil,
                &mut self.spec_yaxis_labels[..],
            );
        };
        self.floor_db.1 = current_floor;
        self.ceil_db.1 = current_ceil;
    }

    /// Frequency axis, one row, shared by the spectrum above and the waterfall
    /// below.
    ///
    /// `post_process` rotates DC to the middle, so the display spans
    /// `center ± sample_rate/2`. Five labels centred in five equal columns
    /// rather than evenly spaced ticks: the column centres fall on ∓0.4, ∓0.2
    /// and 0 of that span, no manual padding is needed, and they cannot
    /// collide.
    ///
    /// Signed arithmetic throughout — the low end of the span goes negative on
    /// a wide sample rate at the bottom of the tuning range, and unsigned would
    /// wrap it into the gigahertz.
    fn render_mhz_axis(&self, area: Rect, buf: &mut Buffer) {
        let center_hz = self.center_freq.0.get() as i64;
        let fifth = self.sample_rate.get() as i64 / 5;

        let cols = Layout::horizontal([Constraint::Ratio(1, 5); 5]).split(area);

        for (i, col) in cols.iter().enumerate() {
            let hz = center_hz + (i as i64 - 2) * fifth;
            Line::styled(
                format!("{:.3}", hz as f64 / 1e6),
                Style::new().fg(Color::DarkGray),
            )
            .centered()
            .render(*col, buf);
        }
    }

    fn renter_spectrum_yaxis<const N: usize>(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        };

        let height = area.height as usize;
    }

    fn render_spectrum(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.bins == 0 {
            return;
        }
        let width = area.width as usize;
        let height = area.height as usize;

        let Some(row) = self.rows.front() else { return };

        let (floor, ceil) = (self.floor_db.0.get(), self.ceil_db.0.get());
        // Height is in whole cells but the blocks give eighths, so the bar is
        // measured in eighths throughout and split at the end. Doing it the
        // other way — cells, then a leftover in dB — needs a second division
        // and lands the partial block off by one.
        let eighths_per_db = (height * 8) as f32 / (ceil - floor).max(f32::EPSILON);

        for x in 0..width {
            let db = self.column(row, x, width);
            let eighths = ((db.clamp(floor, ceil) - floor) * eighths_per_db) as usize;

            let (whole, part) = (eighths / 8, eighths % 8);
            let color = self.color(db);
            let bottom = area.y + area.height - 1;

            for i in 0..whole.min(height) {
                if let Some(cell) = buf.cell_mut((area.x + x as u16, bottom - i as u16)) {
                    cell.set_char(EIGHT_BLOCK[7]).set_fg(color);
                }
            }

            // The partial block sits *above* the full stack, not on top of the
            // highest one, and `part` counts eighths from 1 while the table is
            // indexed from 0 — both off-by-ones cost the bar up to a full row.
            if part > 0
                && whole < height
                && let Some(cell) = buf.cell_mut((area.x + x as u16, bottom - whole as u16))
            {
                cell.set_char(EIGHT_BLOCK[part - 1]).set_fg(color);
            }
        }
    }

    fn render_waterfall(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.bins == 0 {
            return;
        }
        let width = area.width as usize;

        for y in 0..area.height {
            // Two waterfall rows per terminal row, newest at the top.
            let top = self.rows.get(2 * y as usize);
            let bottom = self.rows.get(2 * y as usize + 1);

            for x in 0..width {
                // Rows past the end of the history leave the cell at the
                // terminal's own background rather than painting floor_db —
                // a half-filled waterfall should look empty, not silent.
                let pick = |row: Option<&Box<[f32]>>| {
                    row.map_or(Color::Reset, |r| self.color(self.column(r, x, width)))
                };

                if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + y)) {
                    cell.set_char(HALF).set_fg(pick(top)).set_bg(pick(bottom));
                }
            }
        }
    }
}

impl Widget for &SignalView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered().title("Spectrum \u{b7} MHz");
        let inner = block.inner(area);
        block.render(area, buf);

        let y_axis_width: u16 = 5;

        // Axis on the seam rather than above everything: both panels share one
        // frequency scale, and putting it where they meet is what every SDR
        // display does, because each label then touches the thing it labels.
        let [area_spec, mhz_axis, area_water] = Layout::vertical([
            Constraint::Percentage(30),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);

        // The left col for db axis
        let [area_spec_axis, area_spec] =
            Layout::horizontal([Constraint::Length(y_axis_width), Constraint::Fill(1)])
                .areas(area_spec);

        // Same must be done tor mhz_axis
        let [_, mhz_axis] =
            Layout::horizontal([Constraint::Length(y_axis_width), Constraint::Fill(1)])
                .areas(mhz_axis);

        // Y axis of waterfall show timelapsed
        let [area_water_axis, area_water] =
            Layout::horizontal([Constraint::Length(y_axis_width), Constraint::Fill(1)])
                .areas(area_water);

        self.render_spectrum(area_spec, buf);
        self.render_mhz_axis(mhz_axis, buf);
        self.render_waterfall(area_water, buf);
    }
}
