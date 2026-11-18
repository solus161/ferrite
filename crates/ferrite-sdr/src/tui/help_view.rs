//! The help popup: every shortcut, on demand.
//!
//! The bottom bar used to carry the hints, but at 70 columns a row of seven
//! `key what` pairs is already truncated, and it grows with every feature. So
//! the bar now advertises only `h`, and this lists the rest. [`KEYS`] is the
//! single list — add a shortcut in `tui_core::on_key`, add its line here.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};

use crate::tui::colors;

/// `(key, what it does)`, in the order shown.
pub const KEYS: [(&str, &str); 12] = [
    ("\u{2191}\u{2193}", "select / scroll"),
    ("\u{2190}\u{2192}", "adjust"),
    ("tab", "switch pane"),
    ("enter", "collapse / expand pane"),
    ("space", "pause / resume"),
    ("f", "fullscreen waterfall"),
    ("[ ]", "floor -/+"),
    ("v", "spectrum style"),
    ("m", "mute"),
    ("h", "toggle help"),
    ("esc", "close help"),
    ("q", "quit"),
];

/// Width of the key column, sized to the longest key plus a gap.
const KEY_W: u16 = 7;

pub struct HelpView;

impl HelpView {
    /// Draw the popup centred over `area`. Clears what is under it first, or the
    /// waterfall shows through every cell the text does not cover.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let longest = KEYS.iter().map(|(_, what)| what.len()).max().unwrap_or(0) as u16;
        // Border on both sides, one cell of padding each side.
        let want_w = KEY_W + longest + 4;
        let want_h = KEYS.len() as u16 + 2;

        let [row] = Layout::vertical([Constraint::Length(want_h.min(area.height))])
            .flex(Flex::Center)
            .areas(area);
        let [popup] = Layout::horizontal([Constraint::Length(want_w.min(area.width))])
            .flex(Flex::Center)
            .areas(row);

        Clear.render(popup, buf);
        let block = Block::bordered()
            .style(colors::pane_card())
            .title(" Help ")
            .title_style(colors::pane_title())
            .border_style(colors::pane_border(true));
        let inner = block.inner(popup);
        block.render(popup, buf);

        // Running off the bottom of a very short terminal stops drawing rather
        // than writing outside the popup.
        for ((key, what), y) in KEYS.iter().zip(inner.y..inner.bottom()) {
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("{key:<w$}", w = KEY_W as usize - 1),
                    Style::new()
                        .fg(colors::HINT_KEY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(*what, Style::new().fg(colors::TEXT)),
            ])
            .render(Rect::new(inner.x, y, inner.width, 1), buf);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn text(buf: &Buffer) -> String {
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    /// At the PocketTerm35's 70×24 every shortcut must be on screen.
    #[test]
    fn every_key_fits_on_a_small_screen() {
        let area = Rect::new(0, 0, 70, 24);
        let mut buf = Buffer::empty(area);
        HelpView.render(area, &mut buf);
        let t = text(&buf);
        for (key, what) in KEYS {
            assert!(t.contains(key) && t.contains(what), "{key} {what} missing");
        }
    }

    /// A terminal smaller than the popup clips it instead of panicking.
    #[test]
    fn a_tiny_terminal_clips_instead_of_panicking() {
        for (w, h) in [(0, 0), (5, 3), (20, 6), (70, 4)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            HelpView.render(area, &mut buf);
        }
    }
}
