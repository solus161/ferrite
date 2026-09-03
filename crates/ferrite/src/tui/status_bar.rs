//! The bottom bar: key hints, and the last thing that happened.
//!
//! One row, no border. The old "Shortcut" panel spent four rows and a box on
//! nothing; a single dim line carries more hints than fit in that box and costs
//! a row that the waterfall was not using anyway.
//!
//! Hints are per-focused-pane, which is what keeps the line short as controls
//! are added. A transient status message takes the whole row while it is live —
//! the log keeps the history, the bar only ever shows the latest.


use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::tui::tui_states::{Pane, TuiStates};

/// `(key, what it does)`, joined at render time.
const CONTROL_KEYS: [(&str, &str); 6] = [
    ("\u{2191}\u{2193}", "field"),
    ("\u{2190}\u{2192}", "adjust"),
    ("[ ]", "floor"),
    ("m", "mute"),
    ("tab", "pane"),
    ("q", "quit"),
];

const LOG_KEYS: [(&str, &str); 4] = [
    ("\u{2191}\u{2193}", "scroll"),
    ("g", "tail"),
    ("tab", "pane"),
    ("q", "quit"),
];

pub struct StatusBar;

impl StatusBar {
    pub fn render(&self, area: Rect, buf: &mut Buffer, states: &TuiStates, status: Option<&str>) {
        if area.is_empty() {
            return;
        }

        if let Some(msg) = status {
            Line::styled(
                format!(" {msg}"),
                Style::new().fg(Color::Black).bg(Color::Yellow),
            )
            .render(area, buf);
            return;
        }

        let keys: &[(&str, &str)] = match states.focus.get() {
            Pane::Control => &CONTROL_KEYS,
            Pane::Log => &LOG_KEYS,
        };

        let mut spans = vec![Span::raw(" ")];
        for (key, what) in keys {
            spans.push(Span::styled(
                *key,
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" {what}   "),
                Style::new().fg(Color::DarkGray),
            ));
        }

        Line::from(spans).render(area, buf);
    }
}
