use std::io;
use std::rc::Rc;
use std::cell::Cell;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::{DefaultTerminal, Frame};

use rust_radio::fft::Fft;
use rust_radio::spmc::RingConsumer;

use super::signal_view::SignalView;

/// 30 fps is good enough
const FRAME: Duration = Duration::from_millis(33);

pub struct Tui<const SLOTS: usize, const BLOCK: usize, const N: usize> {
    consumer: RingConsumer<u8, SLOTS, BLOCK>,
    fft: Fft<N>,
    signal_view: SignalView,

    // IQ stream buffer
    block: [u8; BLOCK],

    center_hz: Rc<Cell<f32>>,
    sample_rate: Rc<Cell<f32>>,
}

impl<const SLOTS: usize, const BLOCK: usize, const N: usize> Tui<SLOTS, BLOCK, N> {
    pub fn new(
        consumer: RingConsumer<u8, SLOTS, BLOCK>,
        center_hz: f32,
        sample_rate: f32,
    ) -> Self {
        // Block size must be x*window size
        const { assert!(BLOCK % (2*N) == 0)};

        let center_hz = Rc::new(Cell::new(center_hz));
        let sample_rate = Rc::new(Cell::new(sample_rate));

        Self { 
            consumer, fft: Fft::new(),
            signal_view: SignalView::new(N, 256, center_hz.clone(), sample_rate.clone()),
            block: [0u8; BLOCK],
            center_hz, sample_rate
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
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Up => self.signal_view.floor_db += 2.0,
                        KeyCode::Down => self.signal_view.floor_db -= 2.0,
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

        // post_process rotates DC to the middle, so the display spans
        // center ± sample_rate/2. Three labels aligned to thirds rather than
        // evenly spaced ticks: no manual padding, and they cannot collide.
        // let half = self.sample_rate / 2.0;
        // let mhz = |hz: f32| format!("{:.3} MHz", hz / 1e6);
        // let cols = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(r);
        //
        // frame.render_widget(Line::from(mhz(self.center_hz - half)).left_aligned(), cols[0]);
        // frame.render_widget(Line::from(mhz(self.center_hz)).centered(), cols[1]);
        // frame.render_widget(Line::from(mhz(self.center_hz + half)).right_aligned(), cols[2]);
    }

    /// Waterfall filling the frame, with a one-row frequency axis beneath it.
    fn draw(&self, frame: &mut Frame) {
        // Layout
        let [area_stats, area_view] = Layout::horizontal([
            Constraint::Percentage(30), // stats 
            Constraint::Fill(1), // signal view
        ]).areas(frame.area());

        // Stats
        let block_stats = Block::bordered().title("Stats");
        frame.render_widget(block_stats, area_stats);

        frame.render_widget(&self.signal_view, area_view);
        // self.render_signal_view(frame, main);

        // post_process rotates DC to the middle, so the display spans
        // center ± sample_rate/2. Three labels aligned to thirds rather than
        // evenly spaced ticks: no manual padding, and they cannot collide.
        // let half = self.sample_rate / 2.0;
        // let mhz = |hz: f32| format!("{:.3} MHz", hz / 1e6);
        // let cols = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(axis);
        //
        // frame.render_widget(Line::from(mhz(self.center_hz - half)).left_aligned(), cols[0]);
        // frame.render_widget(Line::from(mhz(self.center_hz)).centered(), cols[1]);
        // frame.render_widget(Line::from(mhz(self.center_hz + half)).right_aligned(), cols[2]);
    }
}

