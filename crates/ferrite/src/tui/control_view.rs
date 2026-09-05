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
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Widget};

use sdr_core::control_signal::CtrlSignal;

use crate::source::source::clamp_tuned;
use crate::tui::colors;
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
    Tuned,
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
    const ALL: [Field; 13] = [
        Field::Mode,
        Field::Freq,
        Field::Tuned,
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
            Field::Tuned => "Tuned",
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
            Field::Tuned => format!("{:.3} MHz", states.tuned_freq.get() as f64 / 1e6),
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

            // The hardware LO, and the centre of the waterfall. Moving it slides
            // the whole span with the channel riding along at a fixed offset, so
            // the cursor holds its place on screen and the translator is never
            // disturbed. The channel's *absolute* frequency does move, so this
            // does land on a different station — `Tuned` is what walks the
            // channel around inside the span.
            Field::Freq => {
                let was = states.center_freq.get();
                let hz = offset(was, states.step.get() as i64 * dir as i64, FREQ_RANGE);
                states.center_freq.set(hz);

                // The channel is pinned *relative* to the centre, so it moves by
                // exactly what the centre moved. Taking the delta after the fact
                // rather than reusing the step matters at the ends of
                // `FREQ_RANGE`, where `offset` clamped and the centre went less
                // far than asked.
                //
                // The translator offset is therefore unchanged, which is why
                // this still emits one signal: the controller derives the offset
                // from the difference, and the difference did not move.
                let delta = hz as i64 - was as i64;
                let tuned = (states.tuned_freq.get() as i64 + delta).max(0) as u32;
                states.tuned_freq.set(tuned);

                Some(CtrlSignal::CenterHz(hz))
            }

            // The channel actually demodulated, and the only way to change what
            // is heard. Pure DSP — the device is never told, so no retune.
            Field::Tuned => {
                let hz = offset(
                    states.tuned_freq.get(),
                    states.step.get() as i64 * dir as i64,
                    FREQ_RANGE,
                );
                // Through the shared helper, so the window rule lives in one
                // place and the controller cannot land somewhere else.
                let hz = clamp_tuned(states.center_freq.get(), hz);
                states.tuned_freq.set(hz);
                Some(CtrlSignal::TunedHz(hz))
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
        // The lift under the focused row runs the full width of the pane, so it
        // has to be painted before the three columns are laid out — a per-span
        // background would stop at the end of each string and leave the row
        // looking ragged.
        if focused {
            buf.set_style(area, Style::new().bg(colors::BG_SELECTED));
        }

        let [marker, label, value] = Layout::horizontal([
            Constraint::Length(2),
            Constraint::Length(8),
            Constraint::Fill(1),
        ])
        .areas(area);

        // At rest the row is cyan label + neutral value, and the eye skips it.
        // Focused, the whole row goes orange — the marker alone is easy to lose
        // against a waterfall running next to it, and orange is reserved for
        // exactly this so it stays findable at a glance.
        let (label_style, mut value_style) = if focused {
            (
                Style::new().fg(colors::FOCUS),
                Style::new()
                    .fg(colors::FOCUS_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (
                Style::new().fg(colors::LABEL),
                Style::new().fg(colors::TEXT),
            )
        };

        if field.inert(self) {
            value_style = value_style
                .fg(colors::INERT)
                .add_modifier(Modifier::CROSSED_OUT);
        }

        if focused {
            Line::styled("\u{25b8}", Style::new().fg(colors::FOCUS_BRIGHT)).render(marker, buf);
        }
        Line::styled(field.label(), label_style).render(label, buf);
        Line::styled(field.value(self), value_style)
            .right_aligned()
            .render(value, buf);
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, focused_pane: bool) {
        let block = Block::bordered()
            .style(colors::pane_card())
            .title("Control")
            .title_style(colors::pane_title())
            .border_style(colors::pane_border(focused_pane));
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
                    Style::new()
                        .fg(colors::SECTION)
                        .add_modifier(Modifier::BOLD),
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
    use crate::source::source::TUNE_SPAN_HZ;

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

    /// The panel's whole world, with the two knobs any test might care about.
    ///
    /// Everything else is a fixed plausible radio: 2.4 MS/s, 48 kHz audio,
    /// 91 MHz, 100 kHz step, 300 kHz channel, no PPM correction.
    ///
    /// Note `TuiStates::new` starts AGC **on**, so a test about the gain path
    /// has to say which it wants rather than lean on the default.
    fn states(gain_tenths: i32, floor_db: f32, ceil_db: f32) -> Rc<TuiStates> {
        Rc::new(TuiStates::new(
            2_400_000,
            48_000,
            91_000_000,
            91_350_000,
            100_000,
            gain_tenths,
            300_000,
            0,
            floor_db,
            ceil_db,
        ))
    }

    /// Moving the centre must not change the *offset* between the two.
    ///
    /// The channel is pinned relative to the centre, so retuning slides both by
    /// the same delta and the translator offset — the only thing the audio path
    /// actually sees — never moves. Holding the channel at a fixed absolute
    /// frequency instead would drag its cursor across the waterfall and
    /// eventually shove it out of the span.
    #[test]
    fn moving_the_centre_carries_the_channel_with_it() {
        let v = ControlView::new(states(197, -90.0, 0.0), vec![]);
        let st = &v.states;
        let gap = st.tuned_freq.get() as i64 - st.center_freq.get() as i64;

        for dir in [1, 1, 1, -1, -1, -1, -1] {
            let sig = Field::Freq.adjust(&v, dir);
            assert!(matches!(sig, Some(CtrlSignal::CenterHz(_))));
            assert_eq!(
                st.tuned_freq.get() as i64 - st.center_freq.get() as i64,
                gap,
                "the channel drifted relative to the centre"
            );
        }
    }

    /// ...and `Tuned` is the one that does change it, within the window.
    #[test]
    fn the_channel_moves_alone_and_stops_at_the_span_edge() {
        let v = ControlView::new(states(197, -90.0, 0.0), vec![]);
        let st = &v.states;
        let center = st.center_freq.get();

        // Far enough to hit the edge whichever way it walks: the span is
        // TUNE_SPAN_HZ either side and the step is 100 kHz.
        for dir in [1, -1] {
            for _ in 0..40 {
                Field::Tuned.adjust(&v, dir);
                assert_eq!(st.center_freq.get(), center, "Tuned must not retune");
            }
            let gap = st.tuned_freq.get() as i64 - center as i64;
            assert_eq!(
                gap.abs(),
                TUNE_SPAN_HZ as i64,
                "should have clamped hard at the span edge, not run past it"
            );
        }
    }

    /// Every field must be reachable, and the cursor must come back round.
    #[test]
    fn the_cursor_visits_every_field_once_per_cycle() {
        let mut v = ControlView::new(states(197, -90.0, 0.0), vec![]);

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
        let v = ControlView::new(states(197, -90.0, 0.0), vec![58, 197]);

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

    /// The focused row is lifted by a background, not only by its foreground
    /// colours, and the lift has to span the whole pane width — a per-span
    /// background stops at the end of each string and leaves the row ragged.
    #[test]
    fn the_focused_row_is_lifted_across_the_full_width() {
        let mut v = ControlView::new(states(197, -90.0, 0.0), vec![]);
        v.select(1); // off the first row, so the test is not passing by accident

        let area = Rect::new(0, 0, 30, HEIGHT);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf, true);

        // Find the lifted row, then check it is lifted edge to edge.
        let lifted: Vec<u16> = (area.y..area.bottom())
            .filter(|&y| buf.cell((1, y)).unwrap().bg == colors::BG_SELECTED)
            .collect();

        assert_eq!(lifted.len(), 1, "exactly one row carries the focus");
        let y = lifted[0];
        for x in (area.x + 1)..(area.right() - 1) {
            assert_eq!(
                buf.cell((x, y)).unwrap().bg,
                colors::BG_SELECTED,
                "column {x} of the focused row is not lifted"
            );
        }
    }

    /// An unfocused pane must lift nothing — otherwise both panes look focused
    /// and `Tab` stops meaning anything.
    #[test]
    fn an_unfocused_pane_lifts_no_row() {
        let v = ControlView::new(states(197, -90.0, 0.0), vec![]);

        let area = Rect::new(0, 0, 30, HEIGHT);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf, false);

        assert!(
            !buf.content().iter().any(|c| c.bg == colors::BG_SELECTED),
            "an unfocused pane painted a selection"
        );
    }

    /// The panel is a card: its ground covers border and inner alike, so it
    /// floats on the app surface instead of being a frame with a hole in it.
    #[test]
    fn the_panel_paints_a_card_ground() {
        let v = ControlView::new(states(197, -90.0, 0.0), vec![]);

        let area = Rect::new(0, 0, 30, HEIGHT);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf, false);

        // A border corner and an interior cell: both belong to the card.
        for (x, y) in [(area.x, area.y), (area.x + 1, area.y + 1)] {
            assert_eq!(
                buf.cell((x, y)).unwrap().bg,
                colors::BG_CARD,
                "cell ({x}, {y}) is not on the card ground"
            );
        }
    }

    #[test]
    fn gain_steps_the_tuners_table_not_whole_db() {
        // The FC0013's three clusters — the ~11 dB gaps are why this matters.
        let table = vec![-99, -73, -65, 58, 61, 63, 179, 181, 197];
        let s = states(63, -90.0, 0.0);
        let v = ControlView::new(s.clone(), table);

        assert_eq!(v.gain_step(1), 179, "steps across the gap, not by 1 dB");
        assert_eq!(v.gain_step(-1), 61);

        s.gain_tenths.set(197);
        assert_eq!(v.gain_step(1), 197, "saturates at the ceiling");
    }

    /// An empty table must leave the value alone rather than panic on
    /// `min_by_key` or invent a gain the tuner never offered.
    #[test]
    fn gain_with_no_table_is_a_no_op() {
        let v = ControlView::new(states(123, -90.0, 0.0), vec![]);
        assert_eq!(v.gain_step(1), 123);
    }

    /// Dragging one end of the colour range must push the other rather than
    /// letting the span collapse.
    #[test]
    fn the_display_range_cannot_collapse() {
        let s = states(197, -20.0, -12.0);
        let v = ControlView::new(s.clone(), vec![]);

        Field::Floor.adjust(&v, 1);
        assert_eq!(s.floor_db.get(), -18.0);
        assert_eq!(s.ceil_db.get(), -8.0, "ceiling pushed up to keep the span");

        Field::Ceil.adjust(&v, -1);
        assert_eq!(s.ceil_db.get(), -10.0);
        assert_eq!(s.floor_db.get(), -20.0, "floor pushed down");
    }

    /// Reaching for the gain hands control back to you; the device has to be
    /// told, or the tuner stays in auto and ignores the value on screen.
    #[test]
    fn adjusting_gain_while_agc_is_on_turns_agc_off() {
        let s = states(63, -90.0, 0.0);
        s.agc.set(true);
        let v = ControlView::new(s.clone(), vec![58, 63, 179]);

        let sig = Field::Gain.adjust(&v, 1);
        assert!(!s.agc.get());
        assert_eq!(s.gain_tenths.get(), 179, "and the gain still moved");
        assert!(
            matches!(sig, Some(CtrlSignal::GainTenths(179))),
            "one signal carries both halves: manual mode, at this value"
        );
    }

    /// Leaving AGC must re-assert the gain shown in the panel, or the display
    /// and the tuner disagree about what the radio is doing.
    #[test]
    fn turning_agc_off_re_applies_the_panel_gain() {
        let s = states(63, -90.0, 0.0);
        // Explicitly off first: this test is about the off -> on -> off cycle,
        // and `TuiStates::new` starts it on.
        s.agc.set(false);
        let v = ControlView::new(s.clone(), vec![58, 63, 179]);

        assert!(matches!(Field::Agc.adjust(&v, 1), Some(CtrlSignal::AgcOn)));
        assert!(s.agc.get());

        let sig = Field::Agc.adjust(&v, 1);
        assert!(!s.agc.get());
        assert!(matches!(sig, Some(CtrlSignal::GainTenths(63))));
    }
}
