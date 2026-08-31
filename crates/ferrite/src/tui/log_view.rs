//! The Log panel: everything that *happened*.
//!
//! Renders the tail of [`crate::log`]'s in-memory ring. Newest at the bottom,
//! so a running radio scrolls the way a terminal does and the eye stays put.
//!
//! Nothing on the USB, DSP or audio path writes here — that rule lives with the
//! macros in [`crate::log`], and it is why a dropped ring block shows up as a
//! counter in the Info panel rather than as a line here. What belongs here is
//! anything with a human cause: a retune, a mode change, a setting the tuner
//! silently refused.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Widget};

use crate::log::{self, Level};
use crate::tui::control_view::pane_border;

pub struct LogView;

impl LogView {
    /// `scroll` is lines back from the newest; 0 follows the tail.
    ///
    /// Returns how far the view was actually able to scroll, so the caller can
    /// clamp its own counter instead of letting a held key wind it off into a
    /// number that means nothing.
    pub fn render(&self, area: Rect, buf: &mut Buffer, scroll: usize, focused: bool) -> usize {
        let block = Block::bordered()
            .title("Log")
            .border_style(pane_border(focused));
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.is_empty() {
            return 0;
        }
        let height = inner.height as usize;

        log::with_entries(|entries, start| {
            // Scrolling past the top of the history would silently show an
            // empty pane; hold at the oldest line instead.
            let scroll = scroll.min(entries.len().saturating_sub(height));
            let end = entries.len() - scroll;
            let first = end.saturating_sub(height);

            for (i, e) in entries.range(first..end).enumerate() {
                let secs = e.at.duration_since(start).as_secs();
                let stamp = format!("{:02}:{:02} ", secs / 60, secs % 60);

                let line = Line::from(vec![
                    Span::styled(stamp, Style::new().fg(Color::DarkGray)),
                    Span::styled(e.text.clone(), Style::new().fg(level_color(e.level))),
                ]);

                let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
                line.render(row, buf);
            }

            scroll
        })
    }
}

fn level_color(level: Level) -> Color {
    match level {
        Level::Info => Color::Gray,
        Level::Warn => Color::Yellow,
        Level::Error => Color::Red,
    }
}
