mod speaker;
mod tui;

use rust_radio::{exceptions::CustomError, source::Source, spmc::RingProducer};

use crate::{speaker::Speaker};
use tui::tui::Tui;

/// Ring geometry: RING_SLOTS buffers of RING_BLOCK f32 audio samples each.
/// RING_BLOCK is also the staging size in the DSP callback — `Buffer::write`
/// takes exactly M elements, so the two must not drift apart.
///
/// Sized to the *smallest* consumer, not the largest. A block only becomes
/// readable once it is full, so RING_BLOCK is a latency floor paid by every
/// consumer: 512 @ 48 kHz = ~10.7 ms per block, ~171 ms for the whole ring.
/// Consumers wanting a larger window (an FFT, say) accumulate blocks on their
/// own side — that is cheap, whereas splitting a large block down to cpal's
/// ~512-frame callbacks would mean holding a borrow into a ring slot across
/// dozens of real-time callbacks.
const RING_SLOTS: usize = 16;
const RING_BLOCK: usize = 512;
const IQ_SLOTS: usize = 16;
const IQ_BLOCK: usize = 16384;

const FFT_N: usize = 2048;
const CENTER_HZ: u32 = 91_000_000;

fn main() -> Result<(), CustomError> {
    // ── Device discovery ────────────────────────────────────────────────────
    println!("===RTL-SDR devices===");
    for (i, name) in rtlsdr_mt::devices().enumerate() {
        println!("#{i}: {}", name.to_string_lossy());
    }

    // Ring producer
    // Non-blocking: a slow DSP consumer must never stall the USB callback, so
    // the ring overwrites and drags the reader forward to the freshest data.

    // Ring for speaker consumer, required f32, IQ signals are demod in source 
    let producer_sp = RingProducer::<f32, RING_SLOTS, RING_BLOCK>::new(false);

    // Ring for TUI: spectrograme, etc., required u8 of raw IQ
    let producer_iq = RingProducer::<u8, IQ_SLOTS, IQ_BLOCK>::new(false);

    // A consumer for cpal
    let consumer_sp = producer_sp.subscribe();

    // A consumer for tui
    let consumer_iq = producer_iq.subscribe();

    // Speaker
    let speaker = Speaker::new(consumer_sp);
    let rtl_rate = speaker.rtl_rate;
    speaker.play()?;

    let source = Source::new(rtl_rate, CENTER_HZ);
    let source_handle = source.start_receive(producer_sp, producer_iq)?;

    let tui = Tui::<IQ_SLOTS, IQ_BLOCK, FFT_N>::new(consumer_iq, CENTER_HZ as f32, rtl_rate as f32);
    let _ = tui.run();

    let _ = source_handle.join();
    Ok(())
}
