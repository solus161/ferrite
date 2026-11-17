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
        field: Option<(&str, String)>,
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

        // Fullscreen: the selected Control field on the left, since the panel
        // that would show it is hidden.
        if let Some((label, value)) = field {
            Line::from(vec![
                Span::styled(" \u{25b8} ", Style::new().fg(colors::FOCUS_BRIGHT)),
                Span::styled(format!("{label}  "), Style::new().fg(colors::FOCUS)),
                Span::styled(
                    value,
                    Style::new()
                        .fg(colors::FOCUS_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
            .render(area, buf);
        }

        Line::from(vec![
            Span::styled(
                "h",
                Style::new()
                    .fg(colors::HINT_KEY)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" help ", Style::new().fg(colors::HINT_TEXT)),
        ])
        .right_aligned()
        .render(area, buf);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn row(field: Option<(&str, String)>, status: Option<(Level, &str)>) -> String {
        let area = Rect::new(0, 0, 70, 1);
        let mut buf = Buffer::empty(area);
        StatusBar.render(area, &mut buf, status, field);
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    /// Fullscreen keeps the selected field and the help hint both on the bar.
    #[test]
    fn fullscreen_shows_the_field_and_the_help_hint() {
        let r = row(Some(("Freq", "90.650 MHz".into())), None);
        assert!(r.contains("Freq") && r.contains("90.650 MHz"), "{r:?}");
        assert!(r.trim_end().ends_with("h help"), "{r:?}");
    }

    /// A transient message takes the whole bar, field or not.
    #[test]
    fn a_status_message_replaces_everything() {
        let r = row(Some(("Freq", "90.650 MHz".into())), Some((Level::Info, "Muted")));
        assert!(r.contains("Muted") && !r.contains("Freq") && !r.contains("help"), "{r:?}");
    }
}
