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
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Widget};

// The gradient lives in `colors` with the rest of the palette. All cyan:
// orange is the accent, and a waterfall that borrowed it would paint a loud
// carrier the same shade as a warning. Monotonicity in lightness — what keeps
// a weak signal legible against the noise floor — is asserted by the tests
// over there.
use crate::tui::colors::{self, SPECTRUM_STOPS as STOPS};
use super::tui_core::FRAME;

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

/// Marks per axis — the length every label array shares, and the most any axis
/// will draw. Fewer are drawn when the pane is too short to hold them all.
const MARKS: usize = 6;

/// Columns reserved for a vertical axis: four for the number, one for the tick.
/// Wide enough for `-100` and for a negative time in tenths.
const Y_AXIS_WIDTH: u16 = 5;

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

/// Where each piece of the panel goes.
///
/// Derived in one place because two callers need it and they must agree:
/// `Widget::render` draws with it, and `tui_core::draw` reads `water.height` a
/// step earlier to work out how many seconds of history the waterfall is
/// showing. Computing that height independently is how the time axis ends up
/// describing a pane that is not the one on screen — using the whole panel's
/// height overstates the span by the border, the spectrum and the frequency
/// row, which is most of a short pane.
pub struct SignalLayout {
    pub spec_axis: Rect,
    pub spec: Rect,
    pub freq_axis: Rect,
    pub water_axis: Rect,
    pub water: Rect,
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

    /// The demodulated channel, drawn as the moving cursor.
    ///
    /// A bare `Rc<Cell<_>>` rather than the `(current, old)` pair the axis
    /// labels use: nothing is cached off it, the cursor is recomputed from
    /// scratch every frame, so there is no stale copy to invalidate.
    tuned_freq: Rc<Cell<u32>>,

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
    water_xaxis_labels: [f32; 6],

    /// Preallocated arrays labels for waterfall timelapse axis
    /// 5 segments, 6 values
    water_yaxis_labels: [f32; 6],

    /// Preallocated arrays labels for spectrum db axis
    /// 5 segments, 6 values
    spec_yaxis_labels: [f32; 6],

    /// Rows the *waterfall* was last given — not the panel, which is taller by
    /// the border, the spectrum and the frequency row.
    ///
    /// Written by `tui_core::draw` from [`SignalView::layout`], and read by
    /// [`gen_water_yaxis_label`](Self::gen_water_yaxis_label) to scale the time
    /// axis. It has to come from outside because the height is not known until
    /// the pane is laid out, and `Widget::render` takes `&self` so it cannot
    /// write it back.
    pub waterfall_height: f32,

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
        tuned_freq: Rc<Cell<u32>>,
        sample_rate: Rc<Cell<u32>>,
        floor_db: Rc<Cell<f32>>,
        ceil_db: Rc<Cell<f32>>,
    ) -> Self {
        let center_freq_old = center_freq.get();
        let floor_db_old = floor_db.get();
        let ceil_db_old = ceil_db.get();

        let mut view = Self {
            bins,
            rows: VecDeque::with_capacity(history),
            history,
            pending: vec![f32::NEG_INFINITY; bins].into_boxed_slice(),
            has_pending: false,
            center_freq: (center_freq, center_freq_old),
            tuned_freq,
            sample_rate,
            floor_db: (floor_db, floor_db_old),
            ceil_db: (ceil_db, ceil_db_old),
            water_xaxis_labels: array::from_fn(|_| 0.0f32),
            water_yaxis_labels: array::from_fn(|_| 0.0f32),
            spec_yaxis_labels: array::from_fn(|_| 0.0f32), waterfall_height: 0.0,
            style: SpectrumStyle::BrailleFill,
        };

        // Forced, and this is the only place it can be.
        //
        // The `gen_*` helpers recompute only when their input differs from the
        // copy they cached last time, and the struct above seeds those copies
        // to the *current* values — so on the first frame nothing has changed
        // yet and the labels would stay at the zeros they were initialised
        // with. The axis would read `0` all the way down until the first time
        // the floor was nudged.
        //
        // The waterfall's time axis is not seeded here: it needs
        // `waterfall_height`, which nothing knows until `draw` has laid the
        // pane out.
        view.gen_water_xaxis_labels(true);
        view.gen_spec_yaxis_label(true);
        view.gen_water_xaxis_labels(true);
        view
    }

    /// Terminal column for an absolute frequency, or `None` if it is off-span.
    ///
    /// Deliberately the same mapping [`column`](Self::column) uses — `t * width`
    /// over the whole span — and *not* the one `render_mhz_axis` uses for label
    /// ticks, which is edge-to-edge over `width - 1`. The two differ by up to a
    /// column at the extremes, and a cursor has to sit over the data it points
    /// at, not over the label.
    fn freq_col(&self, hz: u32, width: u16) -> Option<u16> {
        if width == 0 {
            return None;
        }
        let fs = self.sample_rate.get() as f32;
        if fs <= 0.0 {
            return None;
        }
        let t = (hz as f32 - self.center_freq.0.get() as f32) / fs + 0.5;
        (t >= 0.0 && t < 1.0).then(|| ((t * width as f32) as u16).min(width - 1))
    }

    /// Draw one full-height vertical rule at column `x` of `area`.
    ///
    /// Writes a glyph rather than tinting with `set_style`: the waterfall packs
    /// two rows into every cell as foreground *and* background, so a background
    /// tint would erase half the data it covers, and a foreground-only tint
    /// would be invisible on the half-block's lower row. One overwritten column
    /// out of ~100 across 2.4 MHz is the cheaper trade.
    fn render_cursor(area: Rect, x: u16, color: Color, buf: &mut Buffer) {
        for dy in 0..area.height {
            if let Some(cell) = buf.cell_mut((area.x + x, area.y + dy)) {
                cell.set_char('\u{2502}').set_fg(color);
            }
        }
    }

    /// Split the panel. `area` is the whole thing, border included.
    ///
    /// The frequency axis sits on the *seam* rather than above everything: both
    /// plots share one frequency scale, and putting it where they meet is what
    /// every SDR display does, because each label then touches the thing it
    /// labels.
    pub fn layout(area: Rect) -> SignalLayout {
        let inner = Block::bordered().inner(area);

        let [spec, freq_axis, water] = Layout::vertical([
            Constraint::Percentage(30),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);

        let gutter = [Constraint::Length(Y_AXIS_WIDTH), Constraint::Fill(1)];
        let [spec_axis, spec] = Layout::horizontal(gutter).areas(spec);
        let [_, freq_axis] = Layout::horizontal(gutter).areas(freq_axis);
        let [water_axis, water] = Layout::horizontal(gutter).areas(water);

        SignalLayout {
            spec_axis,
            spec,
            freq_axis,
            water_axis,
            water,
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
    pub fn gen_marked_labels(&mut self, force: bool) {
        self.gen_spec_yaxis_label(force);
        self.gen_water_xaxis_labels(force);
        self.gen_water_yaxis_label(force);
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

    /// Generate axis labels, given a center value.
    /// `range_half` is max distance from `center` to either side.
    ///
    /// Delegates, because a centred axis *is* a bounded one over
    /// `center ± range_half`. The hand-rolled version carried two bugs that
    /// only showed once something actually called it:
    ///
    /// - it asserted an odd `out.len()`, and every caller passes `[f32; 6]`;
    /// - even at an odd length it wrote the centre to `bins_half + 1` and left
    ///   `bins_half` itself at whatever it held before, so the true middle
    ///   label was never filled in and one neighbour was written twice.
    ///
    /// An even count has no middle element to write, which is the deeper reason
    /// the bounded form is the right primitive here.
    fn gen_axis_labels_center(center: f32, range_half: f32, out: &mut [f32]) {
        Self::gen_axis_labels_bound(center - range_half, center + range_half, out);
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

    /// Computer waterfall mhz axis marks
    /// `force = true` means recalculate marks always
    /// else check for change before calculation
    pub fn gen_water_xaxis_labels(&mut self, force: bool) {
        let current_freq = self.center_freq.0.get();
        if force || current_freq != self.center_freq.1 {
            Self::gen_axis_labels_center(
                self.center_freq.0.get() as f32,
                self.sample_rate.get() as f32 / 2.0,
                &mut self.water_xaxis_labels[..],
            );
        };
        self.center_freq.1 = current_freq;
    }

    /// Compute waterfall timestamp axis
    pub fn gen_water_yaxis_label(&mut self, force: bool) {
        if force {
            Self::gen_axis_labels_bound(
                0.0,
                -1.0 * self.waterfall_height * 2.0 * FRAME.as_secs_f32(), // a row is actually filled in 2 ticks
                &mut self.water_yaxis_labels[..],                
                );
        }
    }

    /// Frequency axis, one row, shared by the spectrum above and the waterfall
    /// below.
    ///
    /// `post_process` rotates DC to the middle, so the display spans
    /// `center ± sample_rate/2`, and the labels are the *bounds* of that span:
    /// the first names the plot's left edge and the last its right edge.
    ///
    /// So they are placed at their true fractional position across the plot and
    /// **not** centred in equal columns. Equal columns put the first label at
    /// 1/12 of the width and the last at 11/12 — around 8 % in from each end,
    /// which on a waterfall reads as the whole scale being offset. The two ends
    /// are clamped rather than centred: the first is flush left, the last flush
    /// right, and only the interior ones are centred on their tick. That is
    /// what makes the axis agree with the pixels above it.
    fn render_mhz_axis(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        // `water_xaxis_labels` — the frequency array. Reading `water_yaxis_labels`
        // here put the waterfall's *time* range on the frequency scale, and
        // since that range is derived from `waterfall_height`, which is zero
        // until `draw` has laid the pane out, every label came out `0.000`.
        let labels = &self.water_xaxis_labels;
        let width = area.width as usize;

        let fmt = |hz: f32| format!("{:.3}", hz as f64 / 1e6);

        // The longest label decides how many fit. A fixed count overlaps as
        // soon as the pane narrows or the tuner passes 100 MHz and every label
        // gains a digit — and two frequencies printed over each other are worse
        // than one frequency printed alone.
        let widest = labels.iter().map(|hz| fmt(*hz).len()).max().unwrap_or(0);
        if widest == 0 || widest > width {
            return;
        }

        // How many fit, given that the two ends are clamped rather than centred.
        //
        // The clamp is what costs the extra room: the first label's tick is at
        // column 0 but its text runs to `widest`, so its neighbour — centred on
        // its own tick — has to start at least `1.5 × widest + 1` along. That,
        // not `widest + 1`, is the binding constraint; using the naive spacing
        // let the first two labels touch (`89.80090.400`) at exactly the widths
        // where the axis is most cramped. The same bound falls out at the right
        // end by symmetry, and the interior only needs `widest + 1`.
        let min_spacing = widest + widest / 2 + 1;
        let shown = MARKS.min((width - 1) / min_spacing + 1);
        if shown < 2 {
            return;
        }

        // Interpolated across the span, not indexed out of the array — the same
        // trap as the vertical axes: picking `j * (MARKS - 1) / (shown - 1)`
        // gives 0, 1, 3, 5 for four of six, which is one narrow gap and two
        // wide ones.
        let (lo, hi) = (labels[0], labels[MARKS - 1]);

        for j in 0..shown {
            let t = j as f32 / (shown - 1) as f32;
            let text = fmt(lo + (hi - lo) * t);
            let len = text.len();

            // The tick this label names, then the leftmost cell that centres
            // the text on it, clamped so neither end runs off the plot. The
            // clamp is what makes the first label flush left and the last
            // flush right.
            let tick = j * (width - 1) / (shown - 1);
            let x = tick.saturating_sub(len / 2).min(width - len);

            Line::styled(text, Style::new().fg(colors::LABEL)).render(
                Rect::new(area.x + x as u16, area.y, len as u16, 1),
                buf,
            );
        }
    }

    /// One vertical axis in a [`Y_AXIS_WIDTH`] gutter.
    ///
    /// `top` is the value on the first row and `bottom` the value on the last,
    /// so the **caller states the direction** rather than this deciding one for
    /// everybody. The two axes disagree about it: dB runs ceiling-down because
    /// the plot puts the floor at the bottom, while time runs now-down because
    /// `render_waterfall` puts the newest row at the top. Everything else — the
    /// thinning, the end placement, the styling — they share.
    ///
    /// **Ends.** The integer arithmetic lands `top` exactly on the first row and
    /// `bottom` exactly on the last, which is what makes the axis *bound* the
    /// plot instead of floating inside it. Spreading by `height / n` would leave
    /// both ends short by half a step and read as a systematic offset.
    fn render_vaxis(
        &self,
        area: Rect,
        buf: &mut Buffer,
        top: f32,
        bottom: f32,
        label: impl Fn(f32) -> String,
    ) {
        // Two rows is the minimum that can carry both ends; below that an axis
        // says nothing a reader can use. Two columns is one digit plus the tick.
        if area.is_empty() || area.width < 2 || area.height < 2 {
            return;
        }

        let height = area.height as usize;

        // At most one label per row. Six labels into four rows would write two
        // of them over their neighbours and leave whichever came last, so thin
        // the set instead.
        let shown = MARKS.min(height);

        // Values are *interpolated* across the range, not indexed out of the
        // caller's array. Indexing by `j * (n - 1) / (shown - 1)` looks even and
        // is not: six labels into five rows picks 0, 1, 2, 3, 5 — three 18 dB
        // gaps and then a 36 dB one, which reads as a mislabelled axis rather
        // than a thinned one. The arrays are linear ramps, so interpolating
        // their ends reproduces them exactly whenever every label fits.
        let num_w = area.width as usize - 1;

        for j in 0..shown {
            let t = j as f32 / (shown - 1) as f32;
            let value = top + (bottom - top) * t;
            let row = (j * (height - 1) / (shown - 1)) as u16;

            let text = format!("{:>num_w$}", label(value));

            let line = Line::from(vec![
                Span::styled(text, Style::new().fg(colors::LABEL)),
                // Dimmer than the number: the tick is there to point, not to be
                // read, and at the same weight it competes with the digits.
                Span::styled("\u{2524}", Style::new().fg(colors::BORDER)),
            ]);

            let row_area = Rect::new(area.x, area.y + row, area.width, 1);
            line.render(row_area, buf);
        }
    }

    /// The dB scale down the left edge of the spectrum.
    ///
    /// `spec_yaxis_labels` is stored floor-first and the plot puts the floor at
    /// the *bottom*, so the ends go in reversed.
    fn render_spectrum_yaxis(&self, area: Rect, buf: &mut Buffer) {
        let l = &self.spec_yaxis_labels;
        // `{:.0}`: floor and ceiling move in whole dB, and a trailing `.0` on
        // every row costs a column the number needs on a narrow gutter.
        self.render_vaxis(area, buf, l[MARKS - 1], l[0], |v| format!("{v:.0}"));
    }

    /// The elapsed-time scale down the left edge of the waterfall.
    ///
    /// Opposite direction to the dB axis: `water_yaxis_labels` is stored
    /// newest-first and `render_waterfall` puts the newest row at the top, so
    /// the ends go in as stored. Values are negative seconds — how long ago that
    /// row was captured.
    fn render_water_yaxis(&self, area: Rect, buf: &mut Buffer) {
        let l = &self.water_yaxis_labels;
        // One decimal: the whole visible history is often under a second, so
        // `{:.0}` would print `-0` for most of the column.
        self.render_vaxis(area, buf, l[0], l[MARKS - 1], |v| format!("{v:.1}"));
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
            Rc::new(Cell::new(91_350_000)),
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

    /// Read one row of the buffer back as a string.
    fn row_text(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf.cell((x, y)).unwrap().symbol())
            .collect()
    }

    /// The axis must run the same way the plot does: floor at the bottom,
    /// ceiling at the top. `spec_yaxis_labels` is stored floor-first, so the
    /// renderer walks it against the rows — get that backwards and every
    /// reading is inverted while still looking like a plausible axis.
    ///
    /// The ends are checked exactly, because "bounds the plot" is the property:
    /// labels spread by `height / n` instead would sit half a step in from both
    /// edges and read as a systematic offset.
    #[test]
    fn the_db_axis_runs_floor_at_the_bottom() {
        let v = view_at(FLOOR);
        let (w, h) = (5, 7);
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        v.render_spectrum_yaxis(area, &mut buf);

        assert_eq!(row_text(&buf, 0, w), "   0\u{2524}", "ceiling tops the axis");
        assert_eq!(
            row_text(&buf, h - 1, w),
            " -90\u{2524}",
            "floor sits on the last row"
        );
    }

    /// Six labels cannot go into four rows. Thinning has to keep the two ends —
    /// they are the ones the reader needs — and must never write two labels
    /// onto one row, which would leave whichever came last and silently
    /// mislabel that row.
    #[test]
    fn a_short_axis_thins_labels_instead_of_overwriting() {
        let v = view_at(FLOOR);
        let (w, h) = (5, 4);
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        v.render_spectrum_yaxis(area, &mut buf);

        assert_eq!(row_text(&buf, 0, w), "   0\u{2524}");
        assert_eq!(row_text(&buf, h - 1, w), " -90\u{2524}");

        // Every row carries exactly one label, none doubled up.
        for y in 0..h {
            let t = row_text(&buf, y, w);
            assert!(t.ends_with('\u{2524}'), "row {y} has no tick: {t:?}");
        }
    }

    /// The two vertical axes run in **opposite** directions, and sharing one
    /// renderer is exactly what makes that easy to get wrong: dB puts its
    /// largest value on the first row, time puts its largest — zero, the
    /// newest — there too, but its array is stored the other way round. Flip
    /// either call and the waterfall claims the oldest row is the newest.
    #[test]
    fn the_time_axis_runs_newest_at_the_top() {
        let mut v = view_at(FLOOR);
        v.waterfall_height = 12.0;
        v.gen_water_yaxis_label(true);

        let (w, h) = (5, 12);
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        v.render_water_yaxis(area, &mut buf);

        assert_eq!(
            row_text(&buf, 0, w),
            " 0.0\u{2524}",
            "the newest row is now, at the top"
        );

        // 12 rows x 2 spectra per row x one frame each.
        let oldest = -12.0 * 2.0 * FRAME.as_secs_f32();
        assert_eq!(
            row_text(&buf, h - 1, w),
            format!("{:>4}\u{2524}", format!("{oldest:.1}")),
            "the bottom row is the whole visible history ago"
        );
    }

    /// The height the time axis is scaled by must be the waterfall's, not the
    /// panel's — the panel is taller by the border, the spectrum and the
    /// frequency row, which on a short pane is most of it. Reading the wrong
    /// one overstates how much history is on screen.
    #[test]
    fn the_layout_reports_the_waterfall_not_the_panel() {
        let area = Rect::new(0, 0, 60, 20);
        let l = SignalView::layout(area);

        assert!(
            l.water.height < area.height,
            "waterfall {} should be shorter than the panel {}",
            l.water.height,
            area.height
        );
        // Border top and bottom, the spectrum, and the frequency row.
        assert_eq!(l.water.height, 12);
        assert_eq!(l.water_axis.height, l.water.height, "gutter tracks the plot");
        assert_eq!(l.water_axis.width, Y_AXIS_WIDTH);
    }

    /// The frequency labels are the *bounds* of the span, so the first names
    /// the plot's left edge and the last its right edge — they have to sit on
    /// those edges. Centring each in an equal column instead put the ends ~8 %
    /// of the width inboard, which reads as the whole scale being offset rather
    /// than as a rounding detail.
    ///
    /// Checked across several widths because the label count changes with them,
    /// and the count is what the placement arithmetic keys off.
    #[test]
    fn the_frequency_labels_sit_on_the_plot_edges() {
        for w in [60u16, 44, 34, 26] {
            let area = Rect::new(0, 0, w, 12);
            let v = SignalView::new(
                BINS,
                8,
                Rc::new(Cell::new(91_000_000)),
                Rc::new(Cell::new(91_350_000)),
                Rc::new(Cell::new(2_400_000)),
                Rc::new(Cell::new(FLOOR)),
                Rc::new(Cell::new(CEIL)),
            );

            let l = SignalView::layout(area);
            let mut buf = Buffer::empty(area);
            v.render_mhz_axis(l.freq_axis, &mut buf);

            let row = row_text(&buf, l.freq_axis.y, w);
            let first = row.find(|c: char| !c.is_whitespace()).unwrap();
            let last = row.rfind(|c: char| !c.is_whitespace()).unwrap();

            assert_eq!(
                first,
                l.freq_axis.x as usize,
                "w={w}: first label is not flush with the plot's left edge: {row:?}"
            );
            assert_eq!(
                last,
                (l.freq_axis.right() - 1) as usize,
                "w={w}: last label is not flush with the right edge: {row:?}"
            );

            // 89.800 and 92.200 are the span bounds for 91 MHz at 2.4 MS/s.
            assert!(row.contains("89.800") && row.contains("92.200"), "w={w}: {row:?}");
        }
    }

    /// Labels must never touch. The two ends are clamped rather than centred,
    /// which steals room from their neighbours — sizing the gap as `widest + 1`
    /// instead of `1.5 x widest + 1` let the first two run together as
    /// `89.80090.400` at exactly the widths where the axis is most cramped.
    #[test]
    fn frequency_labels_never_run_together() {
        for w in 20u16..80 {
            let area = Rect::new(0, 0, w, 12);
            let v = SignalView::new(
                BINS,
                8,
                Rc::new(Cell::new(91_000_000)),
                Rc::new(Cell::new(91_350_000)),
                Rc::new(Cell::new(2_400_000)),
                Rc::new(Cell::new(FLOOR)),
                Rc::new(Cell::new(CEIL)),
            );

            let l = SignalView::layout(area);
            let mut buf = Buffer::empty(area);
            v.render_mhz_axis(l.freq_axis, &mut buf);

            let row = row_text(&buf, l.freq_axis.y, w);
            for chunk in row.split_whitespace() {
                assert_eq!(
                    chunk.len(),
                    6,
                    "w={w}: two labels ran together as {chunk:?} in {row:?}"
                );
            }
        }
    }

    /// A pane too small for an axis must draw nothing rather than panic on the
    /// `shown - 1` divisor, which is zero when only one label fits.
    #[test]
    fn an_axis_too_small_to_read_draws_nothing() {
        let v = view_at(FLOOR);
        for (w, h) in [(5, 1), (5, 0), (1, 7), (0, 7)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(Rect::new(0, 0, w.max(1), h.max(1)));
            v.render_spectrum_yaxis(area, &mut buf);
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

    /// The two cursors are the only thing tying the waterfall to what is
    /// actually being heard, so their placement is the property worth pinning.
    ///
    /// Centre sits at mid-span by construction; the channel sits a fraction of
    /// the width to the right equal to its fraction of the sample rate. Both
    /// must land on the *same* column in the spectrum and the waterfall, which
    /// is what makes the display readable as one scale.
    #[test]
    fn the_cursors_mark_the_centre_and_the_channel() {
        const CENTER: u32 = 90_650_000;
        const TUNED: u32 = 91_000_000;
        const FS: u32 = 2_400_000;

        let mut v = SignalView::new(
            BINS,
            8,
            Rc::new(Cell::new(CENTER)),
            Rc::new(Cell::new(TUNED)),
            Rc::new(Cell::new(FS)),
            Rc::new(Cell::new(FLOOR)),
            Rc::new(Cell::new(CEIL)),
        );
        v.push(&vec![FLOOR; BINS]);
        v.commit();

        let area = Rect::new(0, 0, 60, 20);
        let l = SignalView::layout(area);
        let mut buf = Buffer::empty(area);
        Widget::render(&v, area, &mut buf);

        let w = l.spec.width as f32;
        let mid = (0.5 * w) as u16;
        let off = ((TUNED - CENTER) as f32 / FS as f32 + 0.5) * w;
        let chan = off as u16;
        assert!(chan > mid, "the channel is above the centre, so it draws right of it");

        for (x, colour, what) in [
            (mid, colors::CURSOR_CENTER, "centre"),
            (chan, colors::CURSOR_TUNED, "channel"),
        ] {
            for (pane, name) in [(l.spec, "spectrum"), (l.water, "waterfall")] {
                let cell = buf.cell((pane.x + x, pane.y)).unwrap();
                assert_eq!(
                    cell.symbol(),
                    "\u{2502}",
                    "{what} cursor missing from the {name} at column {x}"
                );
                assert_eq!(cell.fg, colour, "{what} cursor is the wrong colour in the {name}");
            }
        }
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
        block.render(area, buf);

        let l = SignalView::layout(area);

        self.render_spectrum_yaxis(l.spec_axis, buf);
        self.render_spectrum(l.spec, buf);
        self.render_mhz_axis(l.freq_axis, buf);
        self.render_water_yaxis(l.water_axis, buf);
        self.render_waterfall(l.water, buf);

        // Last, because both plot renderers write every cell in their own area
        // and would paint over anything drawn earlier.
        //
        // `l.spec` and `l.water` are cut from the same `inner` with the same
        // gutter constraint, so they share an `x` and a `width` and one column
        // index lines up across both. The axis row between them is skipped on
        // purpose: it holds text, and a glyph dropped into it would split a
        // frequency label in half.
        //
        // Tuned first, so that when the channel is parked on the centre the
        // cyan centre line is what you see rather than a half-hidden orange one.
        if let Some(x) = self.freq_col(self.tuned_freq.get(), l.spec.width) {
            SignalView::render_cursor(l.spec, x, colors::CURSOR_TUNED, buf);
            SignalView::render_cursor(l.water, x, colors::CURSOR_TUNED, buf);
        }
        if let Some(x) = self.freq_col(self.center_freq.0.get(), l.spec.width) {
            SignalView::render_cursor(l.spec, x, colors::CURSOR_CENTER, buf);
            SignalView::render_cursor(l.water, x, colors::CURSOR_CENTER, buf);
        }
    }
}



