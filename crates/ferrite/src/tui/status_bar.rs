//! The bottom bar: key hints, and the last thing that happened.
//!
//! One row, no border. The old "Shortcut" panel spent four rows and a box on
//! nothing; a single dim line carries more hints than fit in that box and costs
//! a row that the waterfall was not using anyway.
//!
//! Hints are per-focused-pane, which is what keeps the line short as controls
//! are added. A transient status message takes the whole row while it is live —
//! the bar only ever shows the latest; `--trace` keeps the history. Warnings
//! and errors arrive here too, styled by level, since there is no log panel.


use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::log::Level;
use crate::tui::colors;
use crate::tui::tui_states::{Pane, TuiStates};

/// `(key, what it does)`, joined at render time.
const CONTROL_KEYS: [(&str, &str); 7] = [
    ("\u{2191}\u{2193}", "field"),
    ("\u{2190}\u{2192}", "adjust"),
    ("[ ]", "floor"),
    ("v", "spectrum"),
    ("m", "mute"),
    ("tab", "pane"),
    ("q", "quit"),
];

const INFO_KEYS: [(&str, &str); 2] = [
    ("tab", "pane"),
    ("q", "quit"),
];

pub struct StatusBar;

impl StatusBar {
    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        states: &TuiStates,
        status: Option<(Level, &str)>,
    ) {
        if area.is_empty() {
            return;
        }

        if let Some((level, msg)) = status {
            let style = match level {
                Level::Info => Style::new().fg(colors::STATUS_FG).bg(colors::STATUS_BG),
                Level::Warn => Style::new().fg(colors::ALERT_WARN_FG).bg(colors::STATUS_BG),
                Level::Error => Style::new()
                    .fg(colors::ALERT_ERROR_FG)
                    .bg(colors::ALERT_ERROR_BG),
            };
            // Paint the whole row first so an error reads as a bar, not as a
            // short block that stops where the text does.
            buf.set_style(area, style);
            Line::styled(format!(" {msg}"), style).render(area, buf);
            return;
        }

        let keys: &[(&str, &str)] = match states.focus.get() {
            Pane::Control => &CONTROL_KEYS,
            Pane::Info => &INFO_KEYS,
        };

        let mut spans = vec![Span::raw(" ")];
        for (key, what) in keys {
            spans.push(Span::styled(
                *key,
                Style::new()
                    .fg(colors::HINT_KEY)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" {what}   "),
                Style::new().fg(colors::HINT_TEXT),
            ));
        }

        Line::from(spans).render(area, buf);
    }
}
