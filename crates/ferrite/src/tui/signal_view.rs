//! Scrolling spectrogram widget.
//!
//! Time runs down the screen, frequency across it, magnitude as colour. Rows
//! are stored at full bin resolution and folded onto columns at render time, so
//! a terminal resize re-folds the existing history instead of discarding it.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Color;
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
    /// Colour range. These two decide whether the display looks like anything;
    /// they are the knobs worth binding to keys.
    pub floor_db: f32,
    pub ceil_db: f32,
    center_hz: Rc<Cell<u32>>,
    sample_rate: Rc<Cell<u32>>,
}

impl SignalView {
    pub fn new(
        bins: usize,
        history: usize,
        center_hz: Rc<Cell<u32>>,
        sample_rate: Rc<Cell<u32>>,
    ) -> Self {
        Self {
            bins,
            rows: VecDeque::with_capacity(history),
            history,
            pending: vec![f32::NEG_INFINITY; bins].into_boxed_slice(),
            has_pending: false,
            floor_db: -90.0,
            ceil_db: 0.0,
            center_hz,
            sample_rate,
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
        let span = (self.ceil_db - self.floor_db).max(f32::EPSILON);
        let t = ((db - self.floor_db) / span).clamp(0.0, 1.0);

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

    /// Render the MHz axis on top
    fn render_mhz_axis(&self, area: Rect, buf: &mut Buffer) {
        // post_process rotates DC to the middle, so the display spans
        // center ± sample_rate/2. Three labels aligned to thirds rather than
        // evenly spaced ticks: no manual padding, and they cannot collide.
        let sample_rate = self.sample_rate.get();
        let center_hz = self.center_hz.get();
        let witdh = area.width;

        // FFT size of 2048 is represented by area.with, for example 100 columns
        // and splitted into 10 bins
        for i in 0..10 {}

        let half = self.sample_rate.get() / 2;
        let mhz = |hz: u32| format!("{:.1}", hz as f32 / 1e6);

        // 5 bins total
        let cols = Layout::horizontal([Constraint::Ratio(1, 5); 5]).split(area);
        let bin_range = self.sample_rate.get() / 5;

        let freqs = [
            center_hz - 2 * bin_range,
            center_hz - bin_range,
            center_hz,
            center_hz + bin_range,
            center_hz + 2 * bin_range,
        ];

        cols.iter().zip(freqs).for_each(|(c, f)| {
            let line = Line::from(mhz(f)).centered();
            line.render(*c, buf);
        });

        // let line_left = Line::from(mhz(center_hz - half)).left_aligned();
        // line_left.render(cols[0], buf);
        //
        // let line_center = Line::from(mhz(center_hz)).centered();
        // line_center.render(cols[1], buf);
        //
        // let line_right = Line::from(mhz(center_hz + half)).right_aligned();
        // line_right.render(cols[2], buf);
    }

    fn render_spectrum(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.bins == 0 {
            return;
        }
        let width = area.width as usize;
        let height = area.height as usize;

        if let Some(row) = self.rows.get(0) {
            for x in 0..width {
                // Get the db, capped it by ceil, get value from floor, get abs
                let db = self.column(row, x, width);
                let db_abs = (db.clamp(self.floor_db, self.ceil_db) - self.floor_db).abs();

                // Generate the bar for drawing
                // TODO: this value needs be computed only when resizing
                let db_per_row = (self.ceil_db - self.floor_db) / height as f32;

                let rows_whole = (db_abs / db_per_row) as usize;
                let db_remained = db_abs - (rows_whole as f32 * db_per_row);
                let top_char_idx = (db_remained / (db_per_row / 8f32)) as usize;

                for i in 0..rows_whole {
                    if let Some(cell) =
                        buf.cell_mut((area.x + x as u16, area.y + height as u16 - 1 - i as u16))
                    {
                        cell.set_char(EIGHT_BLOCK[7]).set_fg(self.color(db));
                    };
                }

                if top_char_idx > 0 {
                    if let Some(cell) = buf.cell_mut((
                        area.x + x as u16,
                        area.y + height as u16 - 1 - (rows_whole as u16 - 1),
                    )) {
                        cell.set_char(EIGHT_BLOCK[top_char_idx])
                            .set_fg(self.color(db));
                    };
                };
            }
        };
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
        // Signal view, include spectrum and waterfall
        let block = Block::bordered().title("Spectrogram MHz");
        let inner = block.inner(area);
        block.render(area, buf);

        let [mhz_axis, area_spec, area_water] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Percentage(20),
            Constraint::Fill(1),
        ])
        .areas(inner);

        // MHz axis
        self.render_mhz_axis(mhz_axis, buf);

        // Spectrum
        self.render_spectrum(area_spec, buf);

        // Waterfall
        self.render_waterfall(area_water, buf);
    }
}
