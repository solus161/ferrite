use std::sync::mpsc::channel;

mod speaker;

use rust_radio::{
    exceptions::CustomError,
    source::source::Source,
    spmc::RingProducer,
    tui::{control_signal::CtrlSignal, tui::Tui, app_states::AppStates},
};

use crate::speaker::Speaker;

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

    // For controller
    let (ctrl_tx, ctrl_rx) = channel::<CtrlSignal>();

    // Speaker
    let speaker = Speaker::new::<RING_SLOTS, RING_BLOCK>(consumer_sp);
    let rtl_rate = speaker.rtl_rate;
    speaker.play()?;

    // States of app
    let app_states = AppStates::new( 
        rtl_rate,
        500_000,
        91_000_000,
        40,
        300_000,
        0);

    let source = Source::new(
        app_states.sample_rate.get(), 
        app_states.center_freq.get(), 
        app_states.bandwidth.get(),
        ctrl_rx);
    let (source_handle, ctrl_handle) = source.start_receive(producer_sp, producer_iq)?;

    let tui = Tui::<IQ_SLOTS, IQ_BLOCK, FFT_N>::new(
        app_states,
        consumer_iq, 
        ctrl_tx);
    let _ = tui.run();

    let _ = source_handle.join();
    let _ = ctrl_handle.join();
    Ok(())
}
