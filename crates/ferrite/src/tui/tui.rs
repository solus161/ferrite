use std::io;
use std::rc::Rc;
use std::cell::Cell;
use std::time::{Duration, Instant};
use std::sync::mpsc::Sender;

use ratatui::crossterm::event::{self, Event, KeyCode};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::Block;
use ratatui::{DefaultTerminal, Frame};

use sdr_core::spmc::RingConsumer;
use sdr_core::{fft::Fft, control_signal::CtrlSignal};

use crate::tui::stats_view::StatsView;

use super::{app_states::AppStates, signal_view::SignalView};

/// 30 fps is good enough
const FRAME: Duration = Duration::from_millis(33);

pub struct Tui<const SLOTS: usize, const BLOCK: usize, const N: usize> {
    states: AppStates,
    consumer: RingConsumer<f32, SLOTS, BLOCK>,
    fft: Fft<N>,

    // Views/panel
    signal_view: SignalView,
    stats_view: StatsView,

    // IQ stream buffer, after centered and high-pass filter
    block: [f32; BLOCK],

    /// Sender of CtrlSignal
    ctrl_tx: Sender<CtrlSignal>,
}

impl<const SLOTS: usize, const BLOCK: usize, const N: usize> Tui<SLOTS, BLOCK, N> {
    pub fn new(
        app_states: AppStates,
        consumer: RingConsumer<f32, SLOTS, BLOCK>,
        ctrl_tx: Sender<CtrlSignal>,
    ) -> Self {
        // Block size must be x*window size
        const { assert!(BLOCK % (2*N) == 0)};

        let center_freq = app_states.center_freq.clone();
        let sample_rate = app_states.sample_rate.clone();
        let step = app_states.step.clone();
        let gain_db = app_states.gain_db.clone();
        let bw = app_states.bandwidth.clone();
        let ppm = app_states.ppm.clone();

        Self { 
            states: app_states,
            consumer, fft: Fft::new(),
            signal_view: SignalView::new(N, 256, center_freq.clone(), sample_rate),
            stats_view: StatsView::new(center_freq, step, gain_db, bw, ppm),
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
                if let Event::Key(k) = event::read()? {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            if let Ok(()) = self.ctrl_tx.send(CtrlSignal::Quit) {
                                return Ok(())
                            };
                        },
                        KeyCode::Up => self.stats_view.select(-1),
                        KeyCode::Down => self.stats_view.select(1),
                        KeyCode::Left | KeyCode::Right => {
                            let dir = if k.code == KeyCode::Right { 1 } else { -1 };
                            // `None` means the field never leaves the UI (Step)
                            // — it has already written itself into the shared
                            // cell either way.
                            if let Some(sig) = self.stats_view.adjust(dir) {
                                let _ = self.ctrl_tx.send(sig);
                            }
                        }
                        // Display floor, moved off the arrows now that those
                        // drive the tuner fields.
                        KeyCode::Char(']') => self.signal_view.floor_db += 2.0,
                        KeyCode::Char('[') => self.signal_view.floor_db -= 2.0,
                        _ => {}
                    }
                }
            };
        };
    }
    
    /// Process one block -> spectrum & spectrogram
    fn sample(&mut self) {
        // ~10 blocks are published per frame and we read one, so we are lapped
        // regardless. The seek makes the block we do read the newest rather
        // than the one `claim`'s resync lands on, N/2 slots back.
        self.consumer.seek_latest();

        // Nothing to read
        if self.consumer.read_into(&mut self.block).is_err() { return; };

        for window in self.block.chunks_exact(2*N) {
            if let Some(spectrum) = self.fft.push(window) {
                self.signal_view.push(spectrum);
            }
        };

        self.signal_view.commit();
    }

    /// Overall layout for TUI
    fn layout(frame: &mut Frame) -> (Rect, Rect) {
        let [signal_view, stats_control] = Layout::vertical(
            [
                // Upper for spectrum and spectrogram/waterfall
                Constraint::Percentage(70),
                // Lower for stats and controls
                Constraint::Percentage(30),
            ])
            .areas(frame.area());
        
        
        // Render block around signal view
        // Self::render_block_waterfall(frame, signal_view);
        (signal_view, stats_control)
    }

    fn render_signal_view(&self, frame: &mut Frame, r: Rect) {
        let block = Block::bordered().title("Spectrogram");
        frame.render_widget(block, r);
    }

    /// Waterfall filling the frame, with a one-row frequency axis beneath it.
    fn draw(&self, frame: &mut Frame) {
        // Layout
        let [area_stats, area_view] = Layout::horizontal([
            Constraint::Percentage(30), // stats 
            Constraint::Fill(1), // signal view
        ]).areas(frame.area());

        // Stats
        frame.render_widget(&self.stats_view, area_stats);
        frame.render_widget(&self.signal_view, area_view);
    }
}

