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
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Widget};

use crate::log::{self, Level};
use crate::tui::colors;

pub struct LogView;

impl LogView {
    /// `scroll` is lines back from the newest; 0 follows the tail.
    ///
    /// Returns how far the view was actually able to scroll, so the caller can
    /// clamp its own counter instead of letting a held key wind it off into a
    /// number that means nothing.
    pub fn render(&self, area: Rect, buf: &mut Buffer, scroll: usize, focused: bool) -> usize {
        let block = Block::bordered()
            .style(colors::pane_card())
            .title("Log")
            .title_style(colors::pane_title())
            .border_style(colors::pane_border(focused));
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
                    Span::styled(stamp, Style::new().fg(colors::LOG_TIME)),
                    Span::styled(e.text.clone(), level_style(e.level)),
                ]);

                let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
                line.render(row, buf);
            }

            scroll
        })
    }
}

/// The palette carries no red, so error escalates by *inversion* rather than by
/// hue: warn is orange text, error is a block of orange. Louder at a glance,
/// and it does not introduce a colour from outside the palette — which is the
/// thing that makes a designed UI look assembled.
fn level_style(level: Level) -> Style {
    match level {
        Level::Info => Style::new().fg(colors::LOG_INFO),
        Level::Warn => Style::new().fg(colors::LOG_WARN),
        Level::Error => Style::new()
            .fg(colors::LOG_ERROR_FG)
            .bg(colors::LOG_ERROR_BG),
    }
}
