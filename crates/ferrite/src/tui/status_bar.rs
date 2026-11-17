//! The bottom bar: key hints, and the last thing that happened.
//!
//! One row, no border. The old "Shortcut" panel spent four rows and a box on
//! nothing; a single dim line carries more hints than fit in that box and costs
//! a row that the waterfall was not using anyway.
//!
//! At rest it shows a single hint, `h help`: the full list lives in the help
//! popup, because on a 70-column screen a row of hints is truncated before it
//! says anything useful. A transient status message takes the whole row while
//! it is live —
//! the bar only ever shows the latest; `--trace` keeps the history. Warnings
//! and errors arrive here too, styled by level, since there is no log panel.


use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::log::Level;
use crate::tui::colors;

pub struct StatusBar;

impl StatusBar {
    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        status: Option<(Level, &str)>,
    ) {
        if area.is_empty() {
            return;
        }

        if let Some((level, msg)) = status {
            let style = match level {
                Level::Info => Style::new().fg(colors::STATUS_FG).bg(colors::STATUS_BG),
                Level::Warn => Style::new().fg(colors::ALERT_WARN_FG).bg(colors::BG_CARD),
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

        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "h",
                Style::new()
                    .fg(colors::HINT_KEY)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" help", Style::new().fg(colors::HINT_TEXT)),
        ])
        .render(area, buf);
    }
}
