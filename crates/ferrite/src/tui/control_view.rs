//! The Control panel: everything you *set*.
//!
//! One flat, sectioned list. [`Field::ALL`] is both the on-screen order and the
//! order the cursor walks, so the display and the key handling cannot drift
//! apart — adding a control is one entry there plus its three `match` arms.
//! Section headers are derived from [`Field::section`] rather than stored
//! separately, which keeps that single source of truth intact.
//!
//! Values it changes go into [`AppStates`] immediately, so the next frame
//! renders them; whether the *device* also needs telling is what
//! [`Field::adjust`]'s return value says. `None` covers two cases — a control
//! the UI owns outright (Step, the colour range) and one the DSP reads straight
//! off an atomic (volume, mute, de-emphasis). Neither wants a channel.

use std::cell::Cell;
use std::rc::Rc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Widget};

use sdr_core::control_signal::CtrlSignal;

use crate::tui::tui_states::{TuiStates, TunerMode};
/// Widest tuning range across the tuners librtlsdr supports, not this device's.
/// The FC0013 in hand covers roughly 22–1100 MHz (PLAN.md §8.0) and silently
/// clamps beyond that; clamping here only stops the UI showing a frequency no
/// tuner could ever reach. Narrowing it to the real device range wants the
/// tuner type, which `rtlsdr_mt` does not expose.
const FREQ_RANGE: (u32, u32) = (24_000_000, 1_766_000_000);

/// ±100 covers any crystal worth using. Signed, because a dongle needing a
/// *negative* correction is the common case, not the exception.
const PPM_RANGE: (i32, i32) = (-100, 100);

/// Coarse enough to cross the FM band in a few presses, fine enough to land on
/// an NFM channel.
const STEP_LADDER: [u32; 6] = [1_000, 5_000, 10_000, 50_000, 100_000, 1_000_000];

/// Channel widths worth offering. This is the *channel*, not the tuner's IF
/// filter — `source::source` widens it to clear the offset-tuning gap.
const BW_LADDER: [u32; 6] = [50_000, 100_000, 200_000, 300_000, 500_000, 1_000_000];

/// Keeps the colour range from collapsing to a single step, which renders as a
/// flat wall of one colour and looks like a crash.
const MIN_DB_SPAN: f32 = 10.0;

/// Section headers, counted for [`HEIGHT`].
const SECTIONS: usize = 3;

/// Rows this panel needs, border included. `tui::draw` sizes the pane from it,
/// so adding a field cannot silently clip the list.
pub const HEIGHT: u16 = (Field::ALL.len() + SECTIONS + 2) as u16;

/// One adjustable row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Mode,
    Freq,
    Step,
    Gain,
    Agc,
    Bw,
    Ppm,
    Volume,
    Mute,
    Deemph,
    Floor,
    Ceil,
}

impl Field {
    const ALL: [Field; 12] = [
        Field::Mode,
        Field::Freq,
        Field::Step,
        Field::Gain,
        Field::Agc,
        Field::Bw,
        Field::Ppm,
        Field::Volume,
        Field::Mute,
        Field::Deemph,
        Field::Floor,
        Field::Ceil,
    ];

    /// Header drawn *above* this field, when it opens a section.
    fn section(self) -> Option<&'static str> {
        match self {
            Field::Mode => Some("RADIO"),
            Field::Volume => Some("AUDIO"),
            Field::Floor => Some("DISPLAY"),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Field::Mode => "Mode",
            Field::Freq => "Freq",
            Field::Step => "Step",
            Field::Gain => "Gain",
            Field::Agc => "AGC",
            Field::Bw => "BW",
            Field::Ppm => "PPM",
            Field::Volume => "Volume",
            Field::Mute => "Mute",
            Field::Deemph => "De-emph",
            Field::Floor => "Floor",
            Field::Ceil => "Ceiling",
        }
    }

    fn value(self, v: &ControlView) -> String {
        let states = &v.states;
        match self {
            Field::Mode => states.mode.get().label().to_string(),
            Field::Freq => format!("{:.3} MHz", states.center_freq.get() as f64 / 1e6),
            Field::Step => fmt_hz(states.step.get()),
            Field::Gain => format!("{:.1} dB", states.gain_tenths.get() as f32 / 10.0),
            Field::Agc => on_off(states.agc.get()),
            Field::Bw => fmt_hz(states.bandwidth.get()),
            Field::Ppm => format!("{:+}", states.ppm.get()),
            Field::Volume => format!("{}", states.volume.get()),
            Field::Mute => on_off(states.muted.get()),
            Field::Deemph => format!("{} \u{b5}s", states.deemph_us.get()),
            Field::Floor => format!("{:.0} dB", states.floor_db.get()),
            Field::Ceil => format!("{:.0} dB", states.ceil_db.get()),
        }
    }

    /// Whether the value is currently inert — set, but not doing anything.
    ///
    /// Rendered struck-through-dim rather than hidden: a control that exists
    /// but has no effect yet is more honest than one that vanishes, and it is
    /// how you find out that Gain stopped mattering when AGC came on.
    fn inert(self, v: &ControlView) -> bool {
        match self {
            Field::Mode => !v.states.mode.get().implemented(),
            Field::Gain => v.states.agc.get(),
            Field::Volume => v.states.muted.get(),
            _ => false,
        }
    }

    /// Apply one `←`/`→` press; `dir` is -1 or +1.
    ///
    /// Writes straight into the shared state, which is also what [`value`] and
    /// the DSP read, so the display follows on the next frame whether or not a
    /// signal goes out. Returns what the controller thread needs to hear, or
    /// `None` for anything the device is not involved in.
    ///
    /// [`value`]: Field::value
    fn adjust(self, v: &ControlView, dir: i32) -> Option<CtrlSignal> {
        let states = &v.states;
        match self {
            Field::Mode => {
                let i = TunerMode::ALL
                    .iter()
                    .position(|&m| m == states.mode.get())
                    .unwrap_or(0);
                let n = TunerMode::ALL.len();
                let next = TunerMode::ALL[(i + if dir > 0 { 1 } else { n - 1 }) % n];
                states.set_mode(next);
                if next.implemented() {
                    log_info!("mode {}", next.label());
                } else {
                    log_warn!(
                        "mode {} selected but not implemented (PLAN R3.0)",
                        next.label()
                    );
                }
                None
            }

            Field::Freq => {
                let hz = offset(
                    states.center_freq.get(),
                    states.step.get() as i64 * dir as i64,
                    FREQ_RANGE,
                );
                states.center_freq.set(hz);
                Some(CtrlSignal::CenterHz(hz))
            }

            // Purely how far `Freq` moves — no device round trip.
            Field::Step => {
                states.step.set(rung(&STEP_LADDER, states.step.get(), dir));
                None
            }

            // Steps the tuner's own table rather than whole dB. The table is
            // discrete and clustered — the FC0013 reports 23 values in three
            // groups with ~11 dB gaps — so a ±1 dB control would no-op on most
            // presses and display a gain the hardware never had.
            Field::Gain => {
                let tenths = v.gain_step(dir);
                states.gain_tenths.set(tenths);
                log_info!("gain {:.1} dB", tenths as f32 / 10.0);
                // Reaching for the gain means you want it by hand — every radio
                // carrying both controls behaves this way. `GainTenths` *is*
                // "manual, this value", so one signal covers both halves; a
                // separate AGC-off would leave the tuner in auto until the next
                // gain press.
                states.agc.swap(&Cell::new(false));
                if states.agc.get() {
                    log_info!("AGC off (manual gain)");
                }
                Some(CtrlSignal::GainTenths(tenths))
            }

            Field::Agc => {
                let on = !states.agc.get();
                states.agc.set(on);
                log_info!("AGC {}", on_off(on));
                // Leaving AGC restores the gain sitting in the panel. Sending a
                // bare "AGC off" would drop the tuner into manual mode at
                // whatever the AGC last happened to leave it at, so the number
                // on screen and the hardware would disagree.
                Some(match on {
                    true => CtrlSignal::AgcOn,
                    false => CtrlSignal::GainTenths(states.gain_tenths.get()),
                })
            }

            Field::Bw => {
                let bw = rung(&BW_LADDER, states.bandwidth.get(), dir);
                states.bandwidth.set(bw);
                log_info!("channel bandwidth {}", fmt_hz(bw));
                Some(CtrlSignal::Bandwidth(bw))
            }

            Field::Ppm => {
                let ppm = (states.ppm.get() + dir).clamp(PPM_RANGE.0, PPM_RANGE.1);
                states.ppm.set(ppm);
                Some(CtrlSignal::Ppm(ppm))
            }

            Field::Volume => {
                let vol = (states.volume.get() as i64 + 5 * dir as i64).clamp(0, 100) as u32;
                states.volume.set(vol as f32);
                None
            }

            Field::Mute => {
                let muted = !states.muted.get();
                states.muted.set(muted);
                None
            }

            Field::Deemph => {
                // Two values, so direction is irrelevant — either arrow toggles.
                let us = if states.deemph_us.get() == 50 { 75 } else { 50 };
                states.deemph_us.set(us);
                log_info!("de-emphasis {us} \u{b5}s");
                None
            }

            // Both ends push the other rather than stopping, so you can drag
            // the whole window up and down without alternating fields.
            Field::Floor => {
                let floor = states.floor_db.get() + 2.0 * dir as f32;
                states.floor_db.set(floor);
                states
                    .ceil_db
                    .set(states.ceil_db.get().max(floor + MIN_DB_SPAN));
                None
            }
            Field::Ceil => {
                let ceil = states.ceil_db.get() + 2.0 * dir as f32;
                states.ceil_db.set(ceil);
                states
                    .floor_db
                    .set(states.floor_db.get().min(ceil - MIN_DB_SPAN));
                None
            }
        }
    }
}

pub struct ControlView {
    states: Rc<TuiStates>,
    /// The tuner's own gain table, tenths of a dB, ascending. Empty if the
    /// device reported none, in which case Gain becomes a no-op rather than
    /// inventing values.
    gain_table: Vec<i32>,
    selected: usize,
}

impl ControlView {
    pub fn new(states: Rc<TuiStates>, gain_table: Vec<i32>) -> Self {
        Self {
            states,
            gain_table,
            selected: 0,
        }
    }

    /// Move the cursor. Wraps, unlike the value ladders: a cursor returning to
    /// the top of the list is expected behaviour, not a glitch.
    pub fn select(&mut self, dir: i32) {
        let n = Field::ALL.len();
        let delta = if dir > 0 { 1 } else { n - 1 };
        self.selected = (self.selected + delta) % n;
    }

    /// `&self`, not `&mut self` — every value lives behind a `Cell` or an
    /// atomic, so the only thing that would need `&mut` is the cursor, which
    /// this never moves.
    pub fn adjust(&self, dir: i32) -> Option<CtrlSignal> {
        Field::ALL[self.selected].adjust(self, dir)
    }

    /// The focused field's label and new value, for the status bar.
    pub fn focused(&self) -> (&'static str, String) {
        let f = Field::ALL[self.selected];
        (f.label(), f.value(self))
    }

    /// Neighbouring rung of the tuner's gain table, starting from whichever
    /// entry is nearest the current value.
    fn gain_step(&self, dir: i32) -> i32 {
        let cur = self.states.gain_tenths.get();
        if self.gain_table.is_empty() {
            return cur;
        }
        let i = (0..self.gain_table.len())
            .min_by_key(|&i| self.gain_table[i].abs_diff(cur))
            .unwrap_or(0);
        let j = if dir > 0 {
            (i + 1).min(self.gain_table.len() - 1)
        } else {
            i.saturating_sub(1)
        };
        self.gain_table[j]
    }

    fn render_row(&self, area: Rect, buf: &mut Buffer, field: Field, focused: bool) {
        let [marker, label, value] = Layout::horizontal([
            Constraint::Length(2),
            Constraint::Length(8),
            Constraint::Fill(1),
        ])
        .areas(area);

        // Unfocused: label dimmed so the eye lands on the values, which are the
        // part that changes while the radio runs. Focused: the whole row goes
        // accent-coloured, since the marker alone is easy to lose against a
        // waterfall running next to it.
        let (label_style, mut value_style) = if focused {
            (
                Style::new().fg(Color::Yellow),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )
        } else {
            (Style::new().fg(Color::DarkGray), Style::new())
        };

        if field.inert(self) {
            value_style = value_style
                .fg(Color::DarkGray)
                .add_modifier(Modifier::CROSSED_OUT);
        }

        if focused {
            Line::styled("\u{25b8}", Style::new().fg(Color::Yellow)).render(marker, buf);
        }
        Line::styled(field.label(), label_style).render(label, buf);
        Line::styled(field.value(self), value_style)
            .right_aligned()
            .render(value, buf);
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, focused_pane: bool) {
        let block = Block::bordered()
            .title("Control")
            .border_style(pane_border(focused_pane));
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.is_empty() {
            return;
        }

        // Walked with an explicit cursor rather than a `Layout` of fixed rows:
        // section headers make the row count depend on the list, and running
        // off the bottom of a short pane should stop drawing, not squeeze.
        let mut y = inner.y;
        let mut row = |h: u16| -> Option<Rect> {
            (y < inner.bottom()).then(|| {
                let r = Rect::new(inner.x, y, inner.width, h);
                y += h;
                r
            })
        };

        for (i, &field) in Field::ALL.iter().enumerate() {
            if let Some(name) = field.section() {
                let Some(r) = row(1) else { return };
                Line::styled(
                    name,
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )
                .render(r, buf);
            }

            let Some(r) = row(1) else { return };
            self.render_row(r, buf, field, focused_pane && i == self.selected);
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

fn on_off(b: bool) -> String {
    if b { "on".into() } else { "off".into() }
}

/// Border colour for a pane that may or may not have focus. Shared with the
/// other panels so focus reads the same everywhere.
pub fn pane_border(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_ladder_starts_from_the_nearest_rung_and_saturates() {
        assert_eq!(rung(&STEP_LADDER, 7_000, 1), 10_000, "7k is nearest 5k");
        assert_eq!(
            rung(&STEP_LADDER, 1_000, -1),
            1_000,
            "already at the bottom"
        );
        assert_eq!(rung(&STEP_LADDER, 1_000_000, 1), 1_000_000, "and the top");
    }

    #[test]
    fn tuning_down_past_zero_saturates_rather_than_wrapping() {
        assert_eq!(offset(1_000, -5_000, FREQ_RANGE), FREQ_RANGE.0);
    }

    /// Every field must be reachable, and the cursor must come back round.
    #[test]
    fn the_cursor_visits_every_field_once_per_cycle() {
        let app = Arc::new(AppStates::new(
            2_400_000, 48_000, 91_000_000, 197, 300_000, 0,
        ));
        let mut v = ControlView::new(
            app,
            vec![],
            100_000,
            Rc::new(Cell::new(-90.0)),
            Rc::new(Cell::new(0.0)),
        );

        let mut seen = vec![];
        for _ in 0..Field::ALL.len() {
            seen.push(v.focused().0);
            v.select(1);
        }
        assert_eq!(v.selected, 0, "wraps back to the top");
        assert_eq!(seen.len(), Field::ALL.len());
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), Field::ALL.len(), "no duplicate labels");
    }

    /// The panel must survive a pane far shorter than its row list, clipping
    /// from the bottom rather than panicking on an out-of-bounds cell — the
    /// height it asks `tui::draw` for is not a height it is guaranteed.
    #[test]
    fn a_pane_too_short_for_the_list_clips_instead_of_panicking() {
        let app = Arc::new(AppStates::new(
            2_400_000, 48_000, 91_000_000, 197, 300_000, 0,
        ));
        let v = ControlView::new(
            app,
            vec![58, 197],
            100_000,
            Rc::new(Cell::new(-90.0)),
            Rc::new(Cell::new(0.0)),
        );

        for height in 0..=HEIGHT + 4 {
            let area = Rect::new(0, 0, 30, height);
            let mut buf = Buffer::empty(area);
            v.render(area, &mut buf, true);
        }

        // At full height every field is on screen, in list order.
        let area = Rect::new(0, 0, 30, HEIGHT);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf, true);
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        for f in Field::ALL {
            assert!(text.contains(f.label()), "{} missing", f.label());
        }
    }

    #[test]
    fn gain_steps_the_tuners_table_not_whole_db() {
        // The FC0013's three clusters — the ~11 dB gaps are why this matters.
        let table = vec![-99, -73, -65, 58, 61, 63, 179, 181, 197];
        let app = Arc::new(AppStates::new(
            2_400_000, 48_000, 91_000_000, 63, 300_000, 0,
        ));
        let v = ControlView::new(
            app.clone(),
            table,
            100_000,
            Rc::new(Cell::new(-90.0)),
            Rc::new(Cell::new(0.0)),
        );

        assert_eq!(v.gain_step(1), 179, "steps across the gap, not by 1 dB");
        assert_eq!(v.gain_step(-1), 61);

        app.gain_tenths.store(197, Relaxed);
        assert_eq!(v.gain_step(1), 197, "saturates at the ceiling");
    }

    /// An empty table must leave the value alone rather than panic on
    /// `min_by_key` or invent a gain the tuner never offered.
    #[test]
    fn gain_with_no_table_is_a_no_op() {
        let app = Arc::new(AppStates::new(
            2_400_000, 48_000, 91_000_000, 123, 300_000, 0,
        ));
        let v = ControlView::new(
            app,
            vec![],
            100_000,
            Rc::new(Cell::new(-90.0)),
            Rc::new(Cell::new(0.0)),
        );
        assert_eq!(v.gain_step(1), 123);
    }

    /// Dragging one end of the colour range must push the other rather than
    /// letting the span collapse.
    #[test]
    fn the_display_range_cannot_collapse() {
        let app = Arc::new(AppStates::new(
            2_400_000, 48_000, 91_000_000, 197, 300_000, 0,
        ));
        let floor = Rc::new(Cell::new(-20.0));
        let ceil = Rc::new(Cell::new(-12.0));
        let v = ControlView::new(app, vec![], 100_000, floor.clone(), ceil.clone());

        Field::Floor.adjust(&v, 1);
        assert_eq!(floor.get(), -18.0);
        assert_eq!(ceil.get(), -8.0, "ceiling pushed up to keep the span");

        Field::Ceil.adjust(&v, -1);
        assert_eq!(ceil.get(), -10.0);
        assert_eq!(floor.get(), -20.0, "floor pushed down");
    }

    /// Reaching for the gain hands control back to you; the device has to be
    /// told, or the tuner stays in auto and ignores the value on screen.
    #[test]
    fn adjusting_gain_while_agc_is_on_turns_agc_off() {
        let app = Arc::new(AppStates::new(
            2_400_000, 48_000, 91_000_000, 63, 300_000, 0,
        ));
        app.agc.store(true, Relaxed);
        let v = ControlView::new(
            app.clone(),
            vec![58, 63, 179],
            100_000,
            Rc::new(Cell::new(-90.0)),
            Rc::new(Cell::new(0.0)),
        );

        let sig = Field::Gain.adjust(&v, 1);
        assert!(!app.agc.load(Relaxed));
        assert_eq!(
            app.gain_tenths.load(Relaxed),
            179,
            "and the gain still moved"
        );
        assert!(
            matches!(sig, Some(CtrlSignal::GainTenths(179))),
            "one signal carries both halves: manual mode, at this value"
        );
    }

    /// Leaving AGC must re-assert the gain shown in the panel, or the display
    /// and the tuner disagree about what the radio is doing.
    #[test]
    fn turning_agc_off_re_applies_the_panel_gain() {
        let app = Arc::new(AppStates::new(
            2_400_000, 48_000, 91_000_000, 63, 300_000, 0,
        ));
        let v = ControlView::new(
            app.clone(),
            vec![58, 63, 179],
            100_000,
            Rc::new(Cell::new(-90.0)),
            Rc::new(Cell::new(0.0)),
        );

        assert!(matches!(Field::Agc.adjust(&v, 1), Some(CtrlSignal::AgcOn)));
        assert!(app.agc.load(Relaxed));

        let sig = Field::Agc.adjust(&v, 1);
        assert!(!app.agc.load(Relaxed));
        assert!(matches!(sig, Some(CtrlSignal::GainTenths(63))));
    }
}
