use std::sync::mpsc::channel;

#[macro_use]
mod log;

mod source;
mod speaker;
mod tui;

use sdr_core::{
    control_signal::CtrlSignal,
    exceptions::CustomError,
    spmc::RingProducer,
};

use crate::source::source::Source;
use crate::source::source::{IQ_SLOTS, IQ_BLOCK, CPAL_BLOCK};
use crate::speaker::Speaker;
use crate::tui::{tui::Tui, app_states::AppStates};

/// Ring geometry lives with the producers: audio is `IQ_SLOTS × CPAL_BLOCK`,
/// raw IQ is `IQ_SLOTS × IQ_BLOCK` (see `source::source`). A block only becomes
/// readable once it is full, so the block size is a latency floor paid by every
/// consumer — CPAL_BLOCK @ 48 kHz is ~3.4 ms, ~55 ms for the whole ring.
/// Consumers wanting a larger window (an FFT, say) accumulate blocks on their
/// own side; that is cheap, whereas splitting a large block down to cpal's
/// ~512-frame callbacks would mean holding a borrow into a ring slot across
/// dozens of real-time callbacks.
const FFT_N: usize = 2048;

fn main() -> Result<(), CustomError> {
    // ── Device discovery ────────────────────────────────────────────────────
    println!("===RTL-SDR devices===");
    for (i, name) in rtlsdr_mt::devices().enumerate() {
        println!("#{i}: {}", name.to_string_lossy());
    }

    // Ring producer
    // Non-blocking: a slow DSP consumer must never stall the USB callback, so the ring overwrites and drags the reader forward to the freshest data.

    /* A shared ring for speaker and fft, stream of interleaving IQ, block size 16384
    The flow is like this:

    RTL-STD sample rate 2.4MHz, return in queue of 15 blocks of size 16384 interleaving IQ, u8
    |―Preprocessing
    | |―Centered IQ
    | |―DC Blocker: high-pass filter
    |―Splitted to Audio/Cpal and FFT (TUI), [f32; 16384]
      |―Audio/CPAL, most transformation such as decimator work best with separated array of I and Q
      | |―Xlator: optional
      | |―Split [f32; 16384] into two [F32; 8192] for I and Q, for SIMD
      | |―Multi-stage decimator: slicing window mutiplied by precomputed array of coefficients/taps, 
      | | |                      that's SDR++ implementation, may I borrow this idea
      | | |―WFM: `plan_8`
      | | | |―27 taps, step size 4, output of 2 [f32; 2048] ~ 600kHz
      | | | |―69 taps, step size 2, output of 2 [f32; 1024] ~ 300kHz
      | | |―NFM: `plan_32`
      | |   |―44 taps, step size 8, output of 2 [f32; 1024] ~ 300kHz
      | |   |―12 taps, step size 2, output of 2 [f32; 512] ~ 150kHz
      | |   |―69 taps, step size 2, output of 2 [f32; 256] ~ 75kHz
      | |―FM Demodulation — must run at 300kHz, before any resampling:
      | |                   a 200kHz channel does not fit in 48kHz of bandwidth
      | |―Polyphase resampler 4/25, 300kHz -> 48kHz; its prototype LPF is the
      | |                     audio channel filter (15kHz) and the anti-alias
      | |―De-emphasis, 50us one-pole, at 48kHz
      |―FFT: convert to ComplexF32 for more graceful api

    */

    // For speaker
    let producer_sp = RingProducer::<f32, IQ_SLOTS, CPAL_BLOCK>::new(false);

    // For FFT
    let producer_fft = RingProducer::<f32, IQ_SLOTS, IQ_BLOCK>::new(false);

    // Ring for TUI: spectrograme, etc., required u8 of raw IQ
    // let producer_iq = RingProducer::<u8, IQ_SLOTS, IQ_BLOCK>::new(false);

    // A consumer for cpal/audio stream
    let consumer_sp = producer_sp.subscribe();

    // A consumer for tui
    let consumer_fft = producer_fft.subscribe();

    // For controller
    let (ctrl_tx, ctrl_rx) = channel::<CtrlSignal>();

    // Speaker
    let speaker = Speaker::new(consumer_sp);
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
    let (source_handle, ctrl_handle) = source.start_receive(producer_sp, producer_fft)?;

    let tui = Tui::<IQ_SLOTS, IQ_BLOCK, FFT_N>::new(
        app_states,
        consumer_fft, 
        ctrl_tx);
    let _ = tui.run();

    let _ = source_handle.join();
    let _ = ctrl_handle.join();
    Ok(())
}
