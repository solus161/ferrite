use std::sync::atomic::{AtomicU32, AtomicI32};
use std::rc::Rc;
use std::cell::Cell;

/// Several tuner modes
pub enum TunerMode {
    WbFm,   // Wide-band FM
    Nfm,    // Narrow-band FM
    Am,     // Amplitude modulation
    Usb,    // Upper sideband
    Lsb,    // Lower sideband
    Raw,    // No demodulation
}

pub struct AppStates {
    pub sample_rate: Rc<Cell<u32>>, // Hz
    pub step: Rc<Cell<u32>>,        // Hz
    pub center_freq: Rc<Cell<u32>>,
    pub gain_db: Rc<Cell<u32>>,
    pub bandwidth: Rc<Cell<u32>>,
    pub ppm: Rc<Cell<u32>>,
    pub tuner_mode: TunerMode,
}

impl AppStates {
    pub fn new(
        sample_rate: u32, 
        step: u32,
        center_freq: u32,
        gain_db: u32,
        bandwidth: u32,
        ppm: u32,
    ) -> Self {
        Self { 
            sample_rate: Rc::new(Cell::new(sample_rate)),
            step: Rc::new(Cell::new(step)),
            center_freq: Rc::new(Cell::new(center_freq)),
            gain_db: Rc::new(Cell::new(gain_db)),
            bandwidth: Rc::new(Cell::new(bandwidth)),
            ppm: Rc::new(Cell::new(ppm)),
            tuner_mode: TunerMode::WbFm,
        }
    }

    pub fn tuner_mode(&self) -> &TunerMode {
        &self.tuner_mode
    }
}
