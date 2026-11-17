use std::io;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::{DefaultTerminal, Frame};

use sdr_core::spmc::RingConsumer;
use sdr_core::{control_signal::CtrlSignal, fft::Fft};

use crate::log::{self, Level};
use crate::tui::colors;
use crate::tui::control_view::{self, ControlView};
use crate::tui::help_view::HelpView;
use crate::tui::info_view::InfoView;
use crate::tui::status_bar::StatusBar;
use crate::tui::tui_states::{Health, Pane, TuiStates};

use super::signal_view::SignalView;

/// 30 fps is good enough
pub const FRAME: Duration = Duration::from_millis(33);

/// Width of the left column, in columns.
///
/// A `Length`, not a `Percentage`: the panels hold short fixed-width readouts,
/// so every column past what they need belongs to the waterfall, where
/// resolution is the product. At 200 columns a 30 % split would spend 60 of
/// them on `Freq 91.500 MHz`.
///
/// 24 is the narrowest that still fits a Control row — marker 2, label 8,
/// value up to `1766.000 MHz` (12), border 2. On a 70-column screen that is
/// six more columns of waterfall than the old 30.
const SIDEBAR: u16 = 24;

/// How long a status message stays on the bottom bar before the key hints come
/// back. Long enough to read, short enough that the bar is not stale.
const STATUS_TTL: Duration = Duration::from_secs(3);

pub struct Tui<const SLOTS: usize, const BLOCK: usize, const N: usize> {
    states: Rc<TuiStates>,
    consumer: RingConsumer<f32, SLOTS, BLOCK>,
    fft: Fft<N>,

    // Panels
    signal_view: SignalView,
    control_view: ControlView,
    info_view: InfoView,
    status_bar: StatusBar,
    status: Option<(Level, String, Instant)>,

    /// Whether the help popup is up. While it is, it takes every key except
    /// its own close keys and `q`.
    help: bool,

    /// IQ stream buffer, after centered and high-pass filter
    block: [f32; BLOCK],

    /// Sender of CtrlSignal
    ctrl_tx: Sender<CtrlSignal>,

    
}

impl<const SLOTS: usize, const BLOCK: usize, const N: usize> Tui<SLOTS, BLOCK, N> {
    pub fn new(
        states: TuiStates,
        gain_table: Vec<i32>,
        consumer: RingConsumer<f32, SLOTS, BLOCK>,
        ctrl_tx: Sender<CtrlSignal>,
        health: Health,
    ) -> Self {
        // Block size must be x*window size
        const { assert!(BLOCK.is_multiple_of(2 * N)) };

        let states = Rc::new(states);

        let signal_view = SignalView::new(
            N,
            256,
            states.center_freq(),
            states.tuned_freq(),
            states.sample_rate(),
            states.floor_db(),
            states.ceil_db(),
        );

        let info_view = InfoView::new(
            states.center_freq(),
            states.tuned_freq(),
            states.sample_rate(),
            states.audio_rate(),
            health,
        );

        let control_view = ControlView::new(states.clone(), gain_table);

        Self {
            states, signal_view,
            control_view,
            info_view,
            status_bar: StatusBar,
            status: None,
            help: false,
            consumer,
            fft: Fft::new(),
            block: [0.0f32; BLOCK],
            ctrl_tx,
        }
    }

    pub fn run(mut self) -> io::Result<()> {
        let mut terminal = ratatui::init();
        let res = self.event_loop(&mut terminal);
        ratatui::restore();
        res
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        loop {
            let deadline = Instant::now() + FRAME;
            self.sample();
            self.poll_alert();
            terminal.draw(|f| self.draw(f))?;

            // After drawing, wait for input till deadline
            while let Some(remain) = deadline.checked_duration_since(Instant::now()) {
                if !event::poll(remain)? {
                    break;
                };
                if let Event::Key(k) = event::read()?
                    && self.on_key(k)
                {
                    return Ok(());
                }
            }
        }
    }

    pub fn status(&self) -> Option<(Level, &str)> {
        self.status
            .as_ref()
            .filter(|(_, _, at)| at.elapsed() < STATUS_TTL)
            .map(|(level, t, _)| (*level, t.as_str()))
    }

    /// Returns true when the app should exit.
    fn on_key(&mut self, k: KeyEvent) -> bool {
        // Crossterm reports Press *and* Release under the kitty keyboard
        // protocol and on Windows; without this every keypress fires twice
        // there and one arrow moves two steps.
        if k.kind != KeyEventKind::Press {
            return false;
        }

        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

        match k.code {
            // Raw mode swallows SIGINT, so Ctrl-C has to be handled like any
            // other key or `q` is the only way out.
            KeyCode::Char('c') if ctrl => return self.quit(),
            // Only `q`. Esc used to quit too, and on a handheld it is the key
            // you hit by accident; it now only closes the help popup.
            KeyCode::Char('q') => return self.quit(),

            KeyCode::Char('h') => self.help = !self.help,
            KeyCode::Esc => self.help = false,

            // The popup covers the panels, so a key that changed them would
            // change something you cannot see.
            _ if self.help => {}

            KeyCode::Tab | KeyCode::BackTab => {
                self.states
                    .focus
                    .set(self.states.focus.get().next());
            }

            KeyCode::Enter => {
                let open = match self.states.focus.get() {
                    Pane::Control => &self.states.control_open,
                    Pane::Info => &self.states.info_open,
                };
                open.set(!open.get());
            }

            KeyCode::Up => self.scroll(-1),
            KeyCode::Down => self.scroll(1),

            KeyCode::Left | KeyCode::Right => {
                let dir = if k.code == KeyCode::Right { 1 } else { -1 };
                // `None` means the setting never reaches the device — either
                // the UI owns it outright (Step, the colour range) or the DSP
                // reads it off an atomic (volume, mute, de-emphasis). Either
                // way it has already written itself into the shared state.
                if let Some(sig) = self.control_view.adjust(dir) {
                    let _ = self.ctrl_tx.send(sig);
                }
                let (label, value) = self.control_view.focused();
                self.set_status(format!("{label}  {value}"));

                // Recompute axis mark labels
                self.signal_view.gen_marked_labels(false);
            }

            // Kept as global shortcuts even though both are now Control rows —
            // the colour range is the one thing you reach for without wanting
            // to move the cursor off the frequency.
            KeyCode::Char(']') => self.nudge_floor(2.0),
            KeyCode::Char('[') => self.nudge_floor(-2.0),

            // Mute earns a global key for the same reason every media player
            // gives it one.
            KeyCode::Char('m') => {
                let muted = !self.states.muted.load(Ordering::Relaxed);
                self.states.muted.store(muted, Ordering::Relaxed);
                self.set_status(if muted { "Muted" } else { "Unmuted" });
            }

            // A/B the spectrum renderers. Which of blocks and braille reads
            // better depends on the terminal font as much as on the signal, so
            // it is a key rather than a decision taken in the source.
            KeyCode::Char('v') => {
                let style = self.signal_view.cycle_style();
                self.set_status(format!("Spectrum  {}", style.label()));
            }

            _ => {}
        }
        false
    }

    /// Leave only once the controller has been told: it owns the librtlsdr
    /// handle and has to `cancel_async_read` before the reader thread can be
    /// joined. A closed channel means that thread is already gone, so staying
    /// would hang the UI on a radio that no longer exists.
    fn quit(&mut self) -> bool {
        let _ = self.ctrl_tx.send(CtrlSignal::Quit);
        true
    }

    /// Post a transient message to the bottom bar. The bar only ever shows
    /// the latest; `--trace` keeps the history.
    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some((Level::Info, text.into(), Instant::now()));
    }

    /// Surface the newest warning or error from [`crate::log`] on the bar.
    /// There is no log panel, so this is the only place one is seen live.
    fn poll_alert(&mut self) {
        if let Some((level, text)) = log::take_alert() {
            self.status = Some((level, text, Instant::now()));
        }
    }

    fn scroll(&mut self, dir: i32) {
        match self.states.focus.get() {
            Pane::Control => self.control_view.select(dir),
            Pane::Info => self.info_view.scroll(dir),
        }
    }

    fn nudge_floor(&mut self, db: f32) {
        let floor = self.states.floor_db.get() + db;
        self.states.floor_db.set(floor);
        self.set_status(format!("Floor  {floor:.0} dB"));
    }

    /// Process one block -> spectrum & spectrogram
    fn sample(&mut self) {
        // ~10 blocks are published per frame and we read one, so we are lapped
        // regardless. The seek makes the block we do read the newest rather
        // than the one `claim`'s resync lands on, N/2 slots back.
        self.consumer.seek_latest();

        // Uncomment this if you want to copy into self.block
        // if self.consumer.read_into(&mut self.block).is_err() {
        //     return;
        // };

        // Buffer will be copied to fft anyway, no need to copy here, just return the slice
        let Ok(buf) = self.consumer.read() else {
            return;
        };

        for window in buf.chunks_exact(2 * N) {
            if let Some(spectrum) = self.fft.push(window) {
                self.signal_view.push(spectrum);
            }
        }

        self.signal_view.commit();
    }

    /// Left column of readouts, signal view filling the rest, one status row
    /// across the bottom.
    ///
    /// Control's height comes from the panel itself, so adding a control cannot
    /// silently clip the list; Info takes what is left. In a terminal too short
    /// for both each panel clips from its own bottom rather than squeezing,
    /// which keeps the top of every list — the part you steer with — on screen.
    fn draw(&mut self, frame: &mut Frame) {
        // Paint the ground before anything else. Without this the panels sit on
        // whatever the terminal's own background happens to be, and a palette
        // built around a dark teal surface reads as unrelated colours floating
        // on someone else's theme.
        //
        // Cheap: one pass setting a `bg` on cells that are all about to be
        // written anyway, and it costs no extra escape sequences — a cell
        // carries its background whether or not we chose it.
        let area = frame.area();
        frame
            .buffer_mut()
            .set_style(area, Style::new().bg(colors::BG));

        let [main, status] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

        let [sidebar, signal] =
            Layout::horizontal([Constraint::Length(SIDEBAR), Constraint::Fill(1)]).areas(main);

        let (area_control, area_info) = split_sidebar(
            sidebar,
            self.states.control_open.get(),
            self.states.info_open.get(),
        );

        let focus = self.states.focus.get();
        self.control_view.render(
            area_control,
            frame.buffer_mut(),
            focus == Pane::Control,
            self.states.control_open.get(),
        );
        self.info_view.render(
            area_info,
            frame.buffer_mut(),
            focus == Pane::Info,
            self.states.info_open.get(),
        );

        // The waterfall's own height, not the panel's. The time axis is scaled
        // from this, and the panel is taller by the border, the spectrum and
        // the frequency row — most of a short pane — so using `signal.height`
        // overstated the visible history by well over half.
        //
        // Taken from `SignalView::layout`, the same split `render` uses a step
        // later, so the axis cannot describe a pane other than the one drawn.
        self.signal_view.waterfall_height = SignalView::layout(signal).water.height as f32;
        self.signal_view.gen_marked_labels(true);
        frame.render_widget(&self.signal_view, signal);
        self.status_bar
            .render(status, frame.buffer_mut(), self.status());

        // Last, so it sits over everything else.
        if self.help {
            HelpView.render(main, frame.buffer_mut());
        }
    }
}

/// Split the sidebar between Control (top) and Info (below it).
///
/// A collapsed panel is one row. An open panel whose partner is collapsed takes
/// everything else. With both open, Control gets its full list height (less
/// one row, so Info's header always shows) and Info gets the rest — Control is
/// what you steer with, and both scroll, so whichever is squeezed is still
/// reachable. Collapsing both leaves the space below them blank.
fn split_sidebar(area: Rect, control_open: bool, info_open: bool) -> (Rect, Rect) {
    let h = area.height;
    let control_h = match (control_open, info_open) {
        (false, _) => 1,
        (true, false) => h.saturating_sub(1),
        (true, true) => control_view::HEIGHT.min(h.saturating_sub(1)),
    }
    .min(h);
    let rest = h - control_h;
    let info_h = if info_open { rest } else { rest.min(1) };

    let control = Rect { height: control_h, ..area };
    let info = Rect {
        y: area.y + control_h,
        height: info_h,
        ..area
    };
    (control, info)
}

#[cfg(test)]
mod test {
    use super::*;

    /// The four collapse states at the PocketTerm35's 23 sidebar rows.
    #[test]
    fn the_sidebar_splits_by_what_is_open() {
        let area = Rect::new(0, 0, SIDEBAR, 23);
        let hs = |c, i| {
            let (a, b) = split_sidebar(area, c, i);
            assert_eq!(b.y, a.y + a.height, "Info starts right under Control");
            (a.height, b.height)
        };

        assert_eq!(hs(true, true), (control_view::HEIGHT, 23 - control_view::HEIGHT));
        assert_eq!(hs(true, false), (22, 1));
        assert_eq!(hs(false, true), (1, 22));
        assert_eq!(hs(false, false), (1, 1));
    }

    /// Too short for anything still never overflows the sidebar.
    #[test]
    fn a_tiny_sidebar_never_overflows() {
        for h in 0..5 {
            for (c, i) in [(true, true), (true, false), (false, true), (false, false)] {
                let (a, b) = split_sidebar(Rect::new(0, 0, SIDEBAR, h), c, i);
                assert!(a.height + b.height <= h, "h={h} c={c} i={i}");
            }
        }
    }
}

