//! The palette: "Holo Table", adapted to ratatui.
//!
//! Two layers, and the split is the point:
//!
//! - **Ink** — the palette as given, named by hue. Nothing outside this file
//!   should use these directly.
//! - **Roles** — what a colour *means* on screen. Widgets use these.
//!
//! The reason for the second layer: three different things are yellow in the
//! pre-palette code — the key hints, the Control cursor, and the warn log level
//! — and they are three unrelated jobs that happened to share a hue. Collapse
//! them into one constant and changing the warn colour silently drags the
//! cursor with it.
//!
//! **The governing rule of this palette: cyan is data, orange is the UI
//! talking.** Orange never appears in the spectrum or waterfall, so an accent
//! can never be mistaken for a signal, and a warning is the only warm thing on
//! screen.
//!
//! Everything is `Color::Rgb`, which needs a truecolor terminal — ratatui emits
//! 24-bit SGR and does not degrade to the 256-colour cube on its own. Any
//! terminal from the last decade is fine; `TERM=xterm-256color` without
//! `COLORTERM=truecolor` is the case that is not.

use ratatui::style::{Color, Style};

// ── Ink ─────────────────────────────────────────────────────────────────────
// The palette verbatim. Const, and already `Color`, so a use site costs nothing
// and cannot fail — a hex string would need parsing at every call.

pub const BG_VOID: Color = Color::Rgb(0x01, 0x01, 0x01);
pub const BG_PRIMARY: Color = Color::Rgb(0x07, 0x14, 0x1A);
pub const BG_PANEL: Color = Color::Rgb(0x10, 0x10, 0x10);
pub const BG_PANEL_ALT: Color = Color::Rgb(0x11, 0x1F, 0x23);
pub const BG_ELEVATED: Color = Color::Rgb(0x16, 0x2D, 0x32);

pub const CYAN_MUTED: Color = Color::Rgb(0x1D, 0x40, 0x48);
pub const CYAN_MID: Color = Color::Rgb(0x25, 0x54, 0x5D);
pub const CYAN_BASE: Color = Color::Rgb(0x30, 0x68, 0x74);
pub const CYAN_BRIGHT: Color = Color::Rgb(0x4F, 0x9C, 0xAB);
pub const CYAN_LIGHT: Color = Color::Rgb(0x68, 0x97, 0xA0);
pub const CYAN_GLOW: Color = Color::Rgb(0xB1, 0xEF, 0xF2);
pub const CYAN_HIGHLIGHT: Color = Color::Rgb(0xC8, 0xF6, 0xF6);

pub const ORANGE_DIM: Color = Color::Rgb(0x50, 0x3D, 0x26);
pub const ORANGE_BASE: Color = Color::Rgb(0xCE, 0x7C, 0x34);
pub const ORANGE_GLOW: Color = Color::Rgb(0xFD, 0xCE, 0x95);

/// Note the source names are inverted against intuition: `grey-light`
/// (#383A38, L\*=24) is *darker* than `grey-mid` (#565B5A, L\*=38). Kept as
/// given so the file still matches the palette it came from — which is exactly
/// why the role layer below exists to be read instead.
pub const GREY_MID: Color = Color::Rgb(0x56, 0x5B, 0x5A);
pub const GREY_LIGHT: Color = Color::Rgb(0x38, 0x3A, 0x38);
pub const WHITE_DIM: Color = Color::Rgb(0xE7, 0xE9, 0xE9);
pub const WHITE: Color = Color::Rgb(0xF2, 0xF3, 0xF3);
pub const WHITE_BRIGHT: Color = Color::Rgb(0xFD, 0xFD, 0xFD);

// ── Roles ───────────────────────────────────────────────────────────────────
// Three groups, and which one a thing belongs to is decided by its *state*, not
// by what kind of widget it is:
//
//   cyan   — structure and chrome at rest: borders, titles, section headers,
//            the label beside a value. Everything the eye should skip over.
//   grey   — neutral text. The values themselves, log lines, hints.
//   orange — focused, or critical. Nothing else, ever.
//
// The discipline is in that last line. Orange reads as "here" only while it is
// rare; spend it on a decorative accent and the focused row stops being
// findable at a glance, which is the one job it has. There is exactly one
// orange thing on a resting screen — the focused pane's border — and a second
// only when something has gone wrong.
//
// Contrast ratios are against `BG_PRIMARY`. 4.5:1 is WCAG AA for body text; a
// deliberately de-emphasised row is allowed below it, and the ones that go
// there say so.

// Four levels of ground, deepest first:
//
//   BG_WELL      the spectrum and waterfall, sunk below everything
//   BG           the app surface — the gaps between panels, the status row
//   BG_CARD      a sidebar panel, floating on that surface
//   BG_SELECTED  the focused row, lifted off its card
//
// Four works because the palette's backgrounds were laid out as a ladder
// already: L* of 0.27, 5.60, 10.71, 16.88 — steps of 5.3, 5.1, 6.2, a 1.21×
// spread. Each is roughly twice the luminance of the one below, which is what
// keeps every step visible without any of them shouting.
//
// Adding a fifth would not survive colour rounding on a mid-range terminal.

/// The app's ground: what shows between the panels.
pub const BG: Color = BG_PRIMARY;

/// The dark well the spectrum and waterfall sit in — near-black, so the cyan
/// gradient reads as emitted light rather than as paint on a surface. It is
/// also what makes the signal panel look recessed next to the sidebar without
/// needing a heavier border.
pub const BG_WELL: Color = BG_VOID;

/// A sidebar panel. One step above [`BG`], which is what makes the Control,
/// Info and Log panels read as cards laid on the surface rather than as regions
/// of one flat sheet divided by borders.
pub const BG_CARD: Color = BG_PANEL_ALT;

/// Under the focused row: lifts it off its card without spending colour on it,
/// so the orange foreground stays the thing that carries the focus.
pub const BG_SELECTED: Color = BG_ELEVATED;

// ── Cyan: structure at rest ─────────────────────────────────────────────────

/// Quietest thing on screen that is still a thing.
pub const BORDER: Color = CYAN_MUTED;

/// Panel titles, sitting on the border and belonging to it. 5.80:1.
pub const TITLE: Color = CYAN_LIGHT;

/// Section headers inside a list — RADIO, AUDIO, DISPLAY. Brighter than a
/// title because they organise content rather than frame it. 5.94:1.
pub const SECTION: Color = CYAN_BRIGHT;

/// The dim left column of a readout — 2.99:1, below AA on purpose: a label is
/// signposting, and must not compete with the number beside it.
pub const LABEL: Color = CYAN_BASE;

// ── Grey: neutral text ──────────────────────────────────────────────────────

/// The number you actually came to read. 15.3:1.
pub const TEXT: Color = WHITE_DIM;

/// Secondary text — key hints, log timestamps. 2.70:1.
pub const TEXT_DIM: Color = GREY_MID;

/// Set, but doing nothing: struck-through Gain under AGC, an unimplemented
/// mode. Legible if you look, invisible if you don't, which is the intent —
/// a control that exists but has no effect is more honest than one that
/// vanishes.
pub const INERT: Color = GREY_MID;

// ── Orange: focused, or critical ────────────────────────────────────────────

/// A focused pane's border, and the label of the focused row. 5.83:1.
pub const FOCUS: Color = ORANGE_BASE;

/// The cursor marker and the focused row's value — the brightest point on a
/// resting screen, and the thing the eye is meant to find first. 12.85:1.
pub const FOCUS_BRIGHT: Color = ORANGE_GLOW;

/// A transient status message takes the whole bar. Inverted rather than merely
/// coloured, because it has to win against whatever it replaced.
pub const STATUS_FG: Color = BG_VOID;
pub const STATUS_BG: Color = ORANGE_BASE;

// ── Status-bar hints ────────────────────────────────────────────────────────

/// The key glyph, then what it does.
///
/// Cyan, deliberately not orange. The hint row is on screen permanently, and an
/// accent that is always visible is not an accent — it would flatten the
/// focused pane's border back into the noise. Chrome at rest is cyan even when
/// it is the brightest cyan on the row.
pub const HINT_KEY: Color = CYAN_BRIGHT;
pub const HINT_TEXT: Color = TEXT_DIM;

// ── Log levels ──────────────────────────────────────────────────────────────
// The palette has **no red**. Rather than invent one — a hue from outside is
// what makes a designed UI look assembled — error escalates by *inversion*:
// warn is orange text, error is a block of orange. That reads as louder at a
// glance and costs no new ink.

pub const LOG_TIME: Color = TEXT_DIM;
pub const LOG_INFO: Color = TEXT;
pub const LOG_WARN: Color = ORANGE_BASE;
pub const LOG_ERROR_FG: Color = BG_VOID;
pub const LOG_ERROR_BG: Color = ORANGE_BASE;

// ── Data ────────────────────────────────────────────────────────────────────

/// A cell the waterfall has no data for.
///
/// [`BG_WELL`], matching the panel's own ground, so "no history yet" reads as
/// empty well rather than as a signal sitting at the floor. It was `Reset`
/// before the ground was painted; leaving it there now would punch the
/// terminal's own background through the middle of the panel.
pub const WATERFALL_EMPTY: Color = BG_WELL;

/// A synthesised stop, the midpoint of [`CYAN_BRIGHT`] and [`CYAN_GLOW`].
///
/// Not in the source palette, and the gradient below is unusable without it:
/// `cyan-glow` and `cyan-highlight` differ by 3.3 L\*, so a ramp built only
/// from palette entries spends a whole fifth of its range on a step nobody can
/// see, and lands at a 9.3× worst-step ratio. This one stop takes that to 1.6×.
const CYAN_SOFT: [f32; 3] = [0x80 as f32, 0xC5 as f32, 0xCE as f32];

/// dB → colour, interpolated by `SignalView::color`.
///
/// Monotone in lightness, which is what keeps a weak signal legible against the
/// noise floor and survives a grayscale screenshot — see
/// [`stops_are_monotone_in_lightness`](self::tests::stops_are_monotone_in_lightness).
/// It is also near-uniform perceptually: L\* steps of 24.7, 15.9, 19.3, 15.2,
/// 18.4, a 1.62× spread against inferno's 1.32×.
///
/// All cyan, deliberately. Orange is the accent, and a waterfall that borrows
/// it would make a loud carrier and a warning the same colour.
pub const SPECTRUM_STOPS: [[f32; 3]; 6] = [
    [0x01 as f32, 0x01 as f32, 0x01 as f32], // bg-void
    [0x1D as f32, 0x40 as f32, 0x48 as f32], // cyan-muted
    [0x30 as f32, 0x68 as f32, 0x74 as f32], // cyan-base
    [0x4F as f32, 0x9C as f32, 0xAB as f32], // cyan-bright
    CYAN_SOFT,
    [0xC8 as f32, 0xF6 as f32, 0xF6 as f32], // cyan-highlight
];

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Border style for a pane, by whether it has focus.
///
/// Lives here rather than in `control_view`, which is where it was — `log_view`
/// and `info_view` both had to reach across to import it, which is the sign
/// that chrome had outgrown the panel it started in.
pub fn pane_border(focused: bool) -> Style {
    Style::new().fg(if focused { FOCUS } else { BORDER })
}

/// Panel title style. Paired with [`pane_border`] so a panel's frame is
/// specified in one place.
pub fn pane_title() -> Style {
    Style::new().fg(TITLE)
}

/// A sidebar panel's ground. Applied with `Block::style`, which paints the
/// whole panel — border and inner both — so the card is one surface rather than
/// a frame with a differently-coloured hole in it.
pub fn pane_card() -> Style {
    Style::new().bg(BG_CARD)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance.
    fn luminance(c: [f32; 3]) -> f32 {
        let lin = |v: f32| {
            let v = v / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
    }

    /// The property the whole gradient rests on. A ramp that dips in lightness
    /// — every cyan→orange arrangement of this palette does, because
    /// `orange-glow` is darker than `cyan-glow` — makes a stronger signal
    /// render *darker* than a weaker one somewhere in the range, and buries
    /// weak carriers against the noise floor.
    #[test]
    fn stops_are_monotone_in_lightness() {
        let mut prev = f32::NEG_INFINITY;
        for (i, stop) in SPECTRUM_STOPS.iter().enumerate() {
            let l = luminance(*stop);
            assert!(
                l > prev,
                "stop {i} ({stop:?}) is not lighter than the one below it: {l} <= {prev}"
            );
            prev = l;
        }
    }

    /// Even steps are what make the scale readable across its whole range
    /// rather than only at one end. Measured in L\*, not luminance: the eye
    /// reports lightness, and uniform L\* steps look uneven in Y.
    ///
    /// 2.0 is slack — the ramp measures 1.62 and inferno 1.32, so this catches
    /// a stop being dropped or reordered without failing on a tasteful tweak.
    #[test]
    fn stops_are_perceptually_evenly_spaced() {
        let lstar = |c: [f32; 3]| {
            let y = luminance(c);
            if y <= 0.008856 {
                903.3 * y
            } else {
                116.0 * y.cbrt() - 16.0
            }
        };

        let steps: Vec<f32> = SPECTRUM_STOPS
            .windows(2)
            .map(|w| lstar(w[1]) - lstar(w[0]))
            .collect();

        let mn = steps.iter().copied().fold(f32::MAX, f32::min);
        let mx = steps.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            mx / mn < 2.0,
            "worst step ratio {:.2}x is too uneven: {steps:?}",
            mx / mn
        );
    }
}
