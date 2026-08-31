use std::cell::Cell;
use std::rc::Rc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Widget};

use sdr_core::control_signal::CtrlSignal;

pub struct StatsView {
    tuner_view: TunerView,
    info_view: InfoView,
    mode_view: ModeView,
    key_view: KeyView,
}

impl StatsView {
    pub fn new(
        center_freq: Rc<Cell<u32>>,
        step: Rc<Cell<u32>>,
        gain_db: Rc<Cell<u32>>,
        bw: Rc<Cell<u32>>,
        ppm: Rc<Cell<u32>>,
    ) -> Self {
        Self {
            tuner_view: TunerView::new(center_freq, step, gain_db, bw, ppm),
            info_view: InfoView {},
            mode_view: ModeView {},
            key_view: KeyView {},
        }
    }

    /// Move the tuner field cursor by one.
    pub fn select(&mut self, dir: i32) {
        self.tuner_view.select(dir);
    }

    /// Adjust the focused tuner field, handing back whatever the controller
    /// thread needs to hear about.
    pub fn adjust(&self, dir: i32) -> Option<CtrlSignal> {
        self.tuner_view.adjust(dir)
    }
}

impl Widget for &StatsView {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        // Overall layout include tuner, info, mode, and key
        let [area_tuner, area_info, area_mode, area_key] = Layout::vertical([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .areas(area);

        self.tuner_view.render(area_tuner, buf);
        self.info_view.render(area_info, buf);
        self.mode_view.render(area_mode, buf);
        self.key_view.render(area_key, buf);
    }
}

/// R820T/R828D tuning range. Clamping here rather than letting the device
/// reject the call keeps the display honest — an out-of-range value would
/// otherwise sit on screen while the tuner stayed where it actually was.
const FREQ_RANGE: (u32, u32) = (24_000_000, 1_766_000_000);
const GAIN_RANGE: (u32, u32) = (0, 50);
const PPM_RANGE: (u32, u32) = (0, 100);

/// Coarse enough to cross the FM band in a few presses, fine enough to land on
/// an NFM channel.
const STEP_LADDER: [u32; 6] = [1_000, 5_000, 10_000, 50_000, 100_000, 1_000_000];

/// Tuner filter widths worth offering; the hardware snaps to its nearest.
const BW_LADDER: [u32; 6] = [50_000, 100_000, 200_000, 300_000, 500_000, 1_000_000];

/// One adjustable row of [`TunerView`].
///
/// `ALL` is both the on-screen order and the order the cursor walks, so the
/// display and the key handling cannot drift apart — adding a field here is
/// the whole change.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Freq,
    Step,
    Gain,
    Bw,
    Ppm,
}

impl Field {
    const ALL: [Field; 5] = [Field::Freq, Field::Step, Field::Gain, Field::Bw, Field::Ppm];

    fn label(self) -> &'static str {
        match self {
            Field::Freq => "Freq",
            Field::Step => "Step",
            Field::Gain => "Gain",
            Field::Bw => "BW",
            Field::Ppm => "PPM",
        }
    }

    fn value(self, v: &TunerView) -> String {
        match self {
            Field::Freq => format!("{:.3} MHz", v.center_freq.get() as f64 / 1e6),
            Field::Step => fmt_hz(v.step.get()),
            Field::Gain => format!("{} dB", v.gain_db.get()),
            Field::Bw => fmt_hz(v.bw.get()),
            Field::Ppm => format!("{}", v.ppm.get()),
        }
    }

    /// Apply one `←`/`→` press; `dir` is -1 or +1.
    ///
    /// Writes straight into the shared cell, which is also what `value` reads,
    /// so the display follows on the next frame. Returns the signal the
    /// controller thread needs, or `None` for fields the device never hears
    /// about.
    fn adjust(self, v: &TunerView, dir: i32) -> Option<CtrlSignal> {
        match self {
            Field::Freq => {
                let hz = offset(
                    v.center_freq.get(),
                    v.step.get() as i64 * dir as i64,
                    FREQ_RANGE,
                );
                v.center_freq.set(hz);
                Some(CtrlSignal::CenterHz(hz))
            }
            // Purely how far `Freq` moves — no device round trip.
            Field::Step => {
                v.step.set(rung(&STEP_LADDER, v.step.get(), dir));
                None
            }
            Field::Gain => {
                let db = offset(v.gain_db.get(), dir as i64, GAIN_RANGE);
                v.gain_db.set(db);
                Some(CtrlSignal::Gain(db))
            }
            Field::Bw => {
                let bw = rung(&BW_LADDER, v.bw.get(), dir);
                v.bw.set(bw);
                Some(CtrlSignal::Bandwidth(bw))
            }
            Field::Ppm => {
                let ppm = offset(v.ppm.get(), dir as i64, PPM_RANGE);
                v.ppm.set(ppm);
                Some(CtrlSignal::Ppm(ppm))
            }
        }
    }
}

/// Shift and clamp. Widened to i64 so a downward step past zero saturates
/// instead of wrapping into the gigahertz.
fn offset(cur: u32, delta: i64, (lo, hi): (u32, u32)) -> u32 {
    (cur as i64 + delta).clamp(lo as i64, hi as i64) as u32
}

/// Move to the neighbouring rung of a ladder.
///
/// Starts from the *nearest* rung, so a value that came from somewhere else
/// (startup config, direct entry) still steps sensibly instead of jumping to
/// the bottom. Saturates rather than wraps: arrowing off the end of a short
/// list and reappearing at the start reads as a glitch.
fn rung(ladder: &[u32], cur: u32, dir: i32) -> u32 {
    let i = (0..ladder.len())
        .min_by_key(|&i| ladder[i].abs_diff(cur))
        .unwrap_or(0);

    let j = if dir > 0 {
        (i + 1).min(ladder.len() - 1)
    } else {
        i.saturating_sub(1)
    };
    ladder[j]
}

struct TunerView {
    center_freq: Rc<Cell<u32>>,
    step: Rc<Cell<u32>>,
    gain_db: Rc<Cell<u32>>,
    bw: Rc<Cell<u32>>,
    ppm: Rc<Cell<u32>>,
    /// Index into [`Field::ALL`].
    selected: usize,
}

impl TunerView {
    pub fn new(
        center_freq: Rc<Cell<u32>>,
        step: Rc<Cell<u32>>,
        gain_db: Rc<Cell<u32>>,
        bw: Rc<Cell<u32>>,
        ppm: Rc<Cell<u32>>,
    ) -> Self {
        Self {
            center_freq,
            step,
            gain_db,
            bw,
            ppm,
            selected: 0,
        }
    }

    /// Move the cursor. Wraps, unlike the value ladders: a cursor returning to
    /// the top of a five-item list is the expected behaviour, not a glitch.
    fn select(&mut self, dir: i32) {
        let n = Field::ALL.len();
        let delta = if dir > 0 { 1 } else { n - 1 };
        self.selected = (self.selected + delta) % n;
    }

    /// `&self`, not `&mut self` — every value lives behind a `Cell`, so the
    /// only thing that would need `&mut` is the cursor, which this never moves.
    fn adjust(&self, dir: i32) -> Option<CtrlSignal> {
        Field::ALL[self.selected].adjust(self, dir)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered().title("Tuner");
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.is_empty() {
            return;
        }

        // One terminal row per field, leftover height left blank rather than
        // stretched — a spec sheet reads as a list, not as evenly spread lines.
        // In a pane too short for all of them, the trailing rows come back
        // zero-height and `Line::render` skips them on its own.
        let rows = Layout::vertical([Constraint::Length(1); Field::ALL.len()]).split(inner);

        for (i, (row, field)) in rows.iter().zip(Field::ALL).enumerate() {
            let [marker_area, label_area, value_area] = Layout::horizontal([
                Constraint::Length(2),
                Constraint::Length(5),
                Constraint::Fill(1),
            ])
            .areas(*row);

            let focused = i == self.selected;

            // Unfocused: label dimmed so the eye lands on the values, which are
            // the part that changes while the radio runs. Focused: the whole
            // row goes accent-coloured, since the marker alone is easy to lose
            // against a waterfall running next to it.
            let (label_style, value_style) = if focused {
                (
                    Style::new().fg(Color::Yellow),
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )
            } else {
                (Style::new().fg(Color::DarkGray), Style::new())
            };

            if focused {
                Line::styled("▸", Style::new().fg(Color::Yellow)).render(marker_area, buf);
            }
            Line::styled(field.label(), label_style).render(label_area, buf);
            Line::styled(field.value(self), value_style)
                .right_aligned()
                .render(value_area, buf);
        }
    }
}

/// Hz in whichever unit keeps the number short. Step and bandwidth span 1 kHz
/// to a few MHz, so a fixed unit makes one end of that range unreadable.
fn fmt_hz(hz: u32) -> String {
    match hz {
        h if h >= 1_000_000 => format!("{:.3} MHz", h as f64 / 1e6),
        h if h >= 1_000 => format!("{:.1} kHz", h as f64 / 1e3),
        h => format!("{h} Hz"),
    }
}

struct InfoView {}

impl InfoView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered().title("Info");
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.is_empty() {
            return;
        }
    }
}

struct ModeView {}

impl ModeView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered().title("Mode");
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.is_empty() {
            return;
        }
    }
}

struct KeyView {}

impl KeyView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered().title("Shortcut");
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.is_empty() {
            return;
        }
    }
}
