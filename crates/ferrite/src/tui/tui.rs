use std::cell::Cell;
use std::io;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::{DefaultTerminal, Frame};

use sdr_core::spmc::RingConsumer;
use sdr_core::{control_signal::CtrlSignal, fft::Fft};

use crate::tui::control_view::{self, ControlView};
use crate::tui::info_view::{self, InfoView};
use crate::tui::log_view::LogView;
use crate::tui::status_bar::StatusBar;
use crate::tui::tui_states::{Health, Pane, TuiStates};

use super::signal_view::SignalView;

/// 30 fps is good enough
const FRAME: Duration = Duration::from_millis(33);

/// Width of the left column, in columns.
///
/// A `Length`, not a `Percentage`: the panels hold short fixed-width readouts,
/// so every column past what they need belongs to the waterfall, where
/// resolution is the product. At 200 columns a 30 % split would spend 60 of
/// them on `Freq 91.500 MHz`.
const SIDEBAR: u16 = 30;

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
    log_view: LogView,
    status_bar: StatusBar,
    status: Option<(String, Instant)>,

    // IQ stream buffer, after centered and high-pass filter
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
        health: Arc<Health>,
    ) -> Self {
        // Block size must be x*window size
        const { assert!(BLOCK % (2 * N) == 0) };

        let states = Rc::new(states);

        let signal_view = SignalView::new(
            N,
            256,
            states.center_freq(),
            states.sample_rate(),
            states.floor_db(),
            states.ceil_db(),
        );

        let info_view = InfoView::new(
            states.center_freq(),
            states.sample_rate(),
            states.audio_rate(),
            health,
        );

        let control_view = ControlView::new(states.clone(), gain_table);

        Self {
            states,
            signal_view,
            control_view,
            info_view,
            log_view: LogView,
            status_bar: StatusBar,
            status: None,
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

    pub fn status(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STATUS_TTL)
            .map(|(t, _)| t.as_str())
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
            KeyCode::Char('q') | KeyCode::Esc => return self.quit(),

            KeyCode::Tab | KeyCode::BackTab => {
                self.states
                    .focus
                    .swap(&Cell::new(self.states.focus.get().next()));
                // A pane you scrolled back through should be showing the tail
                // again next time you come to it.
                self.states.log_scroll.set(0);
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
            }

            // Kept as global shortcuts even though both are now Control rows —
            // the colour range is the one thing you reach for without wanting
            // to move the cursor off the frequency.
            KeyCode::Char(']') => self.nudge_floor(2.0),
            KeyCode::Char('[') => self.nudge_floor(-2.0),

            // Mute earns a global key for the same reason every media player
            // gives it one.
            KeyCode::Char('m') => {
                let muted = !self.states.muted.get();
                self.states.muted.set(muted);
                self.set_status(if muted { "Muted" } else { "Unmuted" });
            }

            // Back to the tail of the log.
            KeyCode::Char('g') => self.states.log_scroll.set(0),

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

    /// Post a transient message to the bottom bar. The log keeps the history;
    /// the bar only ever shows the latest.
    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Instant::now()));
    }

    fn scroll(&mut self, dir: i32) {
        match self.states.focus.get() {
            Pane::Control => self.control_view.select(dir),
            // Up scrolls *back* through history, which is the direction the
            // text moves.
            Pane::Log => {
                let scroll = self
                    .states
                    .log_scroll
                    .get()
                    .saturating_add_signed(-dir as isize);
                self.states.log_scroll.set(scroll);
            }
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
    /// The two panel heights come from the panels themselves, so adding a
    /// control cannot silently clip the list. The log takes what is left; in a
    /// terminal too short for all three each panel clips from its own bottom
    /// rather than squeezing, which keeps the top of every list — the part you
    /// steer with — on screen.
    fn draw(&mut self, frame: &mut Frame) {
        let [main, status] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());

        let [sidebar, signal] =
            Layout::horizontal([Constraint::Length(SIDEBAR), Constraint::Fill(1)]).areas(main);

        let [area_control, area_info, area_log] = Layout::vertical([
            Constraint::Length(control_view::HEIGHT),
            Constraint::Length(info_view::HEIGHT),
            Constraint::Fill(1),
        ])
        .areas(sidebar);

        let focus = self.states.focus.get();
        self.control_view
            .render(area_control, frame.buffer_mut(), focus == Pane::Control);
        self.info_view.render(area_info, frame.buffer_mut());

        // Clamped by the view, which is the only thing that knows how many
        // lines fit — otherwise a held key winds the counter off past the end
        // of the history and the pane goes blank.
        let scroll = self.log_view.render(
            area_log,
            frame.buffer_mut(),
            self.states.log_scroll.get(),
            focus == Pane::Log,
        );
        self.states.log_scroll.set(scroll);

        frame.render_widget(&self.signal_view, signal);
        self.status_bar
            .render(status, frame.buffer_mut(), &self.states, self.status());
    }
}
