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

// The gradient lives in `colors` with the rest of the palette. All cyan:
// orange is the accent, and a waterfall that borrowed it would paint a loud
// carrier the same shade as a warning. Monotonicity in lightness — what keeps
// a weak signal legible against the noise floor — is asserted by the tests
// over there.
use crate::tui::colors::{self, SPECTRUM_STOPS as STOPS};

/// Upper-half block. The foreground paints the top half of the cell and the
/// background the bottom, so every terminal row carries two waterfall lines.
const HALF: char = '\u{2580}';
const EIGHT_BLOCK: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

/// Braille dot bits, indexed `[row within the cell][column within the cell]`,
/// row 0 at the top.
///
/// The numbering is the trap. Dots 1–3 run down the left column and 4–6 down
/// the right, from the original six-dot cell; the fourth row was added later
/// for eight-dot braille and is dots 7 and 8, so it does not continue the
/// pattern. Deriving the bit arithmetically instead of tabulating it puts the
/// bottom row in the wrong column and shears the whole trace.
const BRAILLE_DOTS: [[u8; 2]; 4] = [
    [0x01, 0x08],
    [0x02, 0x10],
    [0x04, 0x20],
    [0x40, 0x80],
];

/// U+2800 BRAILLE PATTERN BLANK. The 256 patterns are laid out so that the code
/// point is the base plus the dot bits, which is why the table above can be
/// OR-ed straight onto it.
const BRAILLE_BASE: u32 = 0x2800;

/// How the spectrum panel draws its trace.
///
/// A real trade rather than a preference, which is why all three stay in the
/// tree and a keypress switches them:
///
/// - [`Blocks`](Self::Blocks) — one column per cell, eight vertical steps.
///   Coarsest horizontally, finest vertically, and every cell can take its own
///   colour.
/// - Braille — 2×4 dots per cell: **twice** the horizontal resolution for
///   **half** the vertical. Braille carries one foreground colour per cell, so
///   the two sub-columns inside a cell have to share one.
///
/// Which looks better depends on the terminal font as much as on the signal —
/// some fonts render braille dots small and faint, and there the blocks win.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpectrumStyle {
    Blocks,
    BrailleFill,
    BrailleTrace,
}

impl SpectrumStyle {
    pub fn next(self) -> Self {
        match self {
            SpectrumStyle::Blocks => SpectrumStyle::BrailleFill,
            SpectrumStyle::BrailleFill => SpectrumStyle::BrailleTrace,
            SpectrumStyle::BrailleTrace => SpectrumStyle::Blocks,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SpectrumStyle::Blocks => "blocks",
            SpectrumStyle::BrailleFill => "braille fill",
            SpectrumStyle::BrailleTrace => "braille trace",
        }
    }
}

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

    /// Rows the signal pane was last given, written by `tui_core::draw`.
    ///
    /// **Reconstructed, not recovered** — the field was lost with the
    /// uncommitted work and only its write site survived. Declared `pub` and
    /// zero-initialised to match how `draw` assigns it; the timelapse axis that
    /// presumably consumes it is not wired up here, so nothing reads it yet.
    pub waterfall_height: u16,

    /// Which renderer the spectrum panel uses. Not in `TuiStates` — no other
    /// widget reads it, so it stays with the panel that owns it, the same
    /// reasoning that keeps the Control cursor beside `Field::ALL`.
    style: SpectrumStyle,
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
            waterfall_height: 0,
            style: SpectrumStyle::BrailleFill,
        }
    }

    /// Cycle to the next renderer and report it, so the caller can say which
    /// one is now on screen.
    pub fn cycle_style(&mut self) -> SpectrumStyle {
        self.style = self.style.next();
        self.style
    }

    /// Recompute every axis' mark labels.
    ///
    /// **Reconstructed, not recovered** — `tui_core` called this and the
    /// definition was lost with the uncommitted work. It is the obvious reading
    /// of the two call sites: both `gen_*` helpers already skip the work when
    /// their inputs have not changed, so calling this on any state change is
    /// cheap and cannot leave an axis stale.
    pub fn gen_marked_labels(&mut self) {
        self.gen_mhz_xaxis_labels(false);
        self.gen_spec_yaxis_label(false);
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
    pub fn gen_mhz_xaxis_labels(&mut self, force: bool) {
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
                Style::new().fg(colors::LABEL),
            )
            .centered()
            .render(*col, buf);
        }
    }

    fn renter_spectrum_yaxis<const N: usize>(&self, area: Rect, _buf: &mut Buffer) {
        if area.is_empty() {
            return;
        };

        let _height = area.height as usize;
    }

    fn render_spectrum(&self, area: Rect, buf: &mut Buffer) {
        match self.style {
            SpectrumStyle::Blocks => self.render_spectrum_blocks(area, buf),
            SpectrumStyle::BrailleFill => self.render_spectrum_braille(area, buf, false),
            SpectrumStyle::BrailleTrace => self.render_spectrum_braille(area, buf, true),
        }
    }

    /// The spectrum as braille dots: 2×4 per cell, so twice the frequency
    /// resolution of the block renderer at half the vertical.
    ///
    /// `trace` draws the outline alone rather than filling to the floor. On a
    /// noise floor the two look nearly identical; on a strong carrier the fill
    /// reads as a solid tower and the trace as a curve, which is the whole
    /// reason both are here.
    fn render_spectrum_braille(&self, area: Rect, buf: &mut Buffer, trace: bool) {
        if area.is_empty() || self.bins == 0 {
            return;
        }
        let Some(row) = self.rows.front() else { return };

        let (width, height) = (area.width as usize, area.height as usize);
        let (sub_w, sub_h) = (width * 2, height * 4);

        let (floor, ceil) = (self.floor_db.0.get(), self.ceil_db.0.get());
        let span = (ceil - floor).max(f32::EPSILON);

        // Per sub-column: the peak dB, and the sub-row its bar reaches. Heights
        // are measured *from the top* because that is how the dot table is
        // indexed, and converting once here beats flipping the axis inside the
        // per-dot test.
        let mut tops = Vec::with_capacity(sub_w);
        let mut dbs = Vec::with_capacity(sub_w);
        for sx in 0..sub_w {
            let db = self.column(row, sx, sub_w);
            let filled = ((db.clamp(floor, ceil) - floor) / span * sub_h as f32).round() as usize;
            // Clamped to the last sub-row rather than past it: a bar sitting at
            // the floor then draws a one-dot baseline instead of nothing, which
            // is what makes an empty stretch of band read as a quiet noise
            // floor rather than as a dead display.
            tops.push((sub_h - filled.min(sub_h)).min(sub_h - 1));
            dbs.push(db);
        }

        for cx in 0..width {
            // Braille carries one foreground per cell, so the two sub-columns
            // inside it must agree on a colour. Peak, for the same reason
            // `column` folds by peak.
            let color = self.color(dbs[2 * cx].max(dbs[2 * cx + 1]));

            for cy in 0..height {
                let mut bits = 0u8;
                for (dy, dots) in BRAILLE_DOTS.iter().enumerate() {
                    let y = cy * 4 + dy;
                    for dx in 0..2 {
                        if self.dot_lit(&tops, 2 * cx + dx, y, trace) {
                            bits |= dots[dx];
                        }
                    }
                }

                // Leave untouched rather than writing U+2800: the blank pattern
                // is a glyph, and painting it would overwrite whatever the
                // waterfall or a future marker put there.
                if bits == 0 {
                    continue;
                }

                if let Some(cell) = buf.cell_mut((area.x + cx as u16, area.y + cy as u16)) {
                    // Infallible: BRAILLE_BASE + a u8 is inside the 256-code
                    // point braille block, which has no gaps or surrogates.
                    let glyph = char::from_u32(BRAILLE_BASE + bits as u32).unwrap();
                    cell.set_char(glyph).set_fg(color);
                }
            }
        }
    }

    /// Whether the dot at sub-column `sx`, sub-row `y` (from the top) is lit.
    fn dot_lit(&self, tops: &[usize], sx: usize, y: usize, trace: bool) -> bool {
        let cur = tops[sx];
        if !trace {
            return y >= cur;
        }

        // The tip, plus the vertical run back to the previous sub-column's tip.
        // Without that join a steep skirt renders as a stack of disconnected
        // dots — the difference between a curve and a dotted diagonal.
        let prev = if sx == 0 { cur } else { tops[sx - 1] };
        y >= cur.min(prev) && y <= cur.max(prev)
    }

    fn render_spectrum_blocks(&self, area: Rect, buf: &mut Buffer) {
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
                    row.map_or(colors::WATERFALL_EMPTY, |r| {
                        self.color(self.column(r, x, width))
                    })
                };

                if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + y)) {
                    cell.set_char(HALF).set_fg(pick(top)).set_bg(pick(bottom));
                }
            }
        }
    }
}

#[cfg(test)]
mod braille_tests {
    use super::*;

    const BINS: usize = 64;
    const FLOOR: f32 = -90.0;
    const CEIL: f32 = 0.0;

    /// A view holding one flat spectrum at `db`, ready to render.
    fn view_at(db: f32) -> SignalView {
        let mut v = SignalView::new(
            BINS,
            8,
            Rc::new(Cell::new(91_000_000)),
            Rc::new(Cell::new(2_400_000)),
            Rc::new(Cell::new(FLOOR)),
            Rc::new(Cell::new(CEIL)),
        );
        v.push(&vec![db; BINS]);
        v.commit();
        v
    }

    /// Render one terminal row and return the glyphs across it.
    fn glyphs(v: &SignalView, width: u16, trace: bool) -> Vec<char> {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        v.render_spectrum_braille(area, &mut buf, trace);
        (0..width)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().chars().next().unwrap())
            .collect()
    }

    /// The orientation test. `BRAILLE_DOTS` cannot be derived arithmetically —
    /// the bottom row is dots 7/8, not a continuation of 1-3 and 4-6 — so a
    /// plausible-looking table can still be upside down or column-swapped.
    ///
    /// Both glyphs here are asymmetric in exactly the way that would catch it:
    /// U+28C0 is the *bottom* line and U+2809 the *top*, so swapping the rows
    /// swaps the two assertions.
    #[test]
    fn dot_table_is_the_right_way_up() {
        // At the floor, only the baseline row is lit: dots 7 and 8.
        assert_eq!(glyphs(&view_at(FLOOR), 4, false), vec!['\u{28C0}'; 4]);

        // At the ceiling the cell is solid: all eight dots.
        assert_eq!(glyphs(&view_at(CEIL), 4, false), vec!['\u{28FF}'; 4]);

        // Half a cell's worth from the top down, in trace mode, is the top row
        // alone — dots 1 and 4. A row-flipped table returns U+28C0 here.
        assert_eq!(glyphs(&view_at(CEIL), 4, true), vec!['\u{2809}'; 4]);
    }

    /// A bar at the floor still draws its baseline rather than nothing, so an
    /// empty stretch of band reads as a quiet noise floor and not as a dead
    /// panel. This is the `.min(sub_h - 1)` clamp in `render_spectrum_braille`.
    #[test]
    fn the_floor_draws_a_baseline_not_a_blank() {
        for g in glyphs(&view_at(FLOOR - 20.0), 4, false) {
            assert_ne!(g, ' ', "a signal below the floor still needs a baseline");
            assert_ne!(g, '\u{2800}', "and it must not be the blank pattern");
        }
    }

    /// Braille buys its horizontal resolution from `column`, which folds bins
    /// onto `width * 2` sub-columns rather than `width`. If the fold ever went
    /// back to per-cell, a one-bin carrier would smear across twice the width.
    #[test]
    fn a_narrow_carrier_lands_in_one_half_of_a_cell() {
        let mut v = view_at(FLOOR);
        // One bin at the ceiling, in the left half of the second cell.
        // 8 sub-columns across 64 bins is 8 bins each; sub-column 2 is bins
        // 16..24.
        let mut row = vec![FLOOR; BINS];
        row[18] = CEIL;
        v.push(&row);
        v.commit();

        let g = glyphs(&v, 4, false);
        // Cell 1 holds sub-columns 2 and 3. The carrier is in sub-column 2 —
        // the left dot column — so the left dots are full and the right are
        // still at the baseline: dot 7 plus the whole left column.
        assert_eq!(g[1], '\u{28C7}', "carrier should fill only the left dots");
        assert_eq!(g[0], '\u{28C0}', "and leave its neighbours at the floor");
    }
}

impl Widget for &SignalView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // `style` paints the whole panel, inner included, so the spectrum and
        // the waterfall share one well rather than meeting at a seam where the
        // painted-but-empty spectrum ground would butt against the waterfall's
        // own. Set before the border and title styles, which layer on top.
        let block = Block::bordered()
            .style(Style::new().bg(colors::BG_WELL))
            .title("Spectrum \u{b7} MHz")
            .title_style(colors::pane_title())
            .border_style(colors::pane_border(false));
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
        let [_area_spec_axis, area_spec] =
            Layout::horizontal([Constraint::Length(y_axis_width), Constraint::Fill(1)])
                .areas(area_spec);

        // Same must be done tor mhz_axis
        let [_, mhz_axis] =
            Layout::horizontal([Constraint::Length(y_axis_width), Constraint::Fill(1)])
                .areas(mhz_axis);

        // Y axis of waterfall show timelapsed
        let [_area_water_axis, area_water] =
            Layout::horizontal([Constraint::Length(y_axis_width), Constraint::Fill(1)])
                .areas(area_water);

        self.render_spectrum(area_spec, buf);
        self.render_mhz_axis(mhz_axis, buf);
        self.render_waterfall(area_water, buf);
    }
}
