use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::channel;

#[macro_use]
mod log;

mod source;
mod speaker;
mod tui;

use sdr_core::{control_signal::CtrlSignal, exceptions::CustomError, spmc::RingProducer};

use crate::source::source::Source;
use crate::source::source::{CPAL_BLOCK, IQ_BLOCK, IQ_SLOTS, OFFSET_TUNING_HZ};
use crate::speaker::Speaker;
use crate::tui::{app_states::AppStates, tui::Tui};

/// Startup gain, in librtlsdr's tenths of a dB. Snapped to the tuner's own
/// table at open, so this is a request, not a promise — 20 dB lands on the
/// FC0013's 19.7 dB ceiling and near the top of an R820T's range.
const DEFAULT_GAIN_TENTHS: i32 = 200;

/// How far one `←`/`→` press moves the frequency at startup. 100 kHz is the
/// spacing of the FM broadcast raster in most of the world.
const DEFAULT_STEP_HZ: u32 = 100_000;

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

    The LO is parked OFFSET_TUNING_HZ *below* the wanted station (`source.rs`), so
    everything downstream of the Xlator sees the station at 0 Hz and the dongle's
    LO-leakage spike at -350kHz, where the decimators bury it.

    RTL-SDR at 2.4MHz, 15 queued blocks of 16384 interleaved IQ bytes, u8
    |―Preprocessing, per sample, inside the read_async callback (`source.rs`)
    | |―center_iq: u8 -> f32, (x - 127.4)/128
    | |―IqDcBlocker: leaky-integrator high-pass, ~8Hz corner, on I and Q
    | |―Xlator: shift down 350kHz — station lands on 0Hz, spike on -350kHz.
    |            Ahead of the split deliberately, so the waterfall is centred
    |            on the station too.
    |―Split to Audio/CPAL and FFT (TUI), [f32; 16384] interleaved
      |―Audio/CPAL — decimators want I and Q as separate arrays, for SIMD,
      |               so this leg carries two [f32; 8192] rather than the
      |               interleaved block. `DSPFlow` in `source/dsp.rs`:
      | |―Multi-stage decimator: sliding window multiplied by precomputed taps.
      | | |                      Same shape as SDR++'s `plan_8`.
      | | |―WFM (the only mode implemented)
      | | | |―27 taps Hann, fc 100kHz @2.4MHz, step 4 -> 2x [f32; 2048] ~600kHz
      | | | |―69 taps Hann, fc 100kHz @600kHz,  step 2 -> 2x [f32; 1024] ~300kHz
      | | |―NFM `plan_32`, AM, SSB: not built. Needs the mode abstraction first
      | |                           — see PLAN.md R3.0, then R3.1/R3.2.
      | |―FM Demodulation — atan2 discriminator, deviation 75kHz @300kHz, so
      | |                   full deviation maps to +-1.0. Must run here, before
      | |                   any resampling: a 200kHz channel does not fit in
      | |                   48kHz of bandwidth. -> [f32; 1024] real, 300kHz
      | |―Polyphase resampler 4/25, 300kHz -> 48kHz. Its 1900-tap prototype is
      | |                     designed at interp x 300kHz = 1.2MHz with fc 15kHz,
      | |                     and is both the audio channel filter and the
      | |                     anti-alias. Taps are scaled by interp (4) because
      | |                     `create_taps` normalises DC gain to 1.0 and each
      | |                     of the 4 branches would otherwise carry a quarter.
      | |                     -> 163 or 164 samples (8192/50 = 163.84)
      | |―De-emphasis, 50us one-pole, at 48kHz, in place over `out[..n]` only
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

    // ── Shared state ────────────────────────────────────────────────────────
    // One `Arc` for everything the radio has an opinion about: the UI writes
    // settings into it, the DSP and cpal read them, and measurements come back
    // the other way. Purely visual state (cursor, colour range, log scroll)
    // stays in `TuiStates`, on the UI thread, behind no synchronisation at all.
    //
    // The rates go in as zero because the *device* picks them: cpal reports the
    // output rate it will actually run at, and librtlsdr the sample rate it
    // rounded to. Seeding a nominal value here would put a number on screen
    // that nothing had confirmed.
    let app = Arc::new(AppStates::new(
        0,          // sample rate — from librtlsdr, below
        0,          // audio rate  — from cpal, below
        91_000_000, // center
        DEFAULT_GAIN_TENTHS,
        300_000, // channel bandwidth
        0,       // ppm
    ));

    // Speaker. Fills in both rates, since it is what asks cpal for one.
    let speaker = Speaker::new(consumer_sp, app.clone());
    let rtl_rate = speaker.rtl_rate;
    speaker.play()?;

    let source = Source::new(
        rtl_rate,
        app.center_freq.load(Relaxed),
        app.bandwidth.load(Relaxed),
        (DEFAULT_GAIN_TENTHS / 10).max(0) as u32,
        ctrl_rx,
    );

    // The tuner's gain table is discrete and its ceiling is tuner-specific, so
    // the request is rarely what lands. Reflect what the hardware took, and
    // hand the table itself to the Control panel — its Gain row steps that
    // table rather than whole dB, which is what keeps the displayed gain a gain
    // the device actually has.
    app.gain_tenths.store(source.applied_gain_tenths, Relaxed);
    // librtlsdr rounds the requested rate to what its divider can make.
    app.sample_rate.store(source.sample_rate(), Relaxed);
    let gain_table = source.gain_table.clone();

    log_info!(
        "tuner gain {:.1} dB \u{b7} {} steps available",
        source.applied_gain_tenths as f32 / 10.0,
        gain_table.len()
    );
    log_info!(
        "{:.3} MS/s \u{b7} LO {:.3} MHz \u{b7} offset tuning {} kHz",
        source.sample_rate() as f64 / 1e6,
        app.center_freq
            .load(Relaxed)
            .saturating_sub(OFFSET_TUNING_HZ) as f64
            / 1e6,
        OFFSET_TUNING_HZ / 1000
    );

    let (source_handle, ctrl_handle) = source.start_receive(producer_sp, producer_fft)?;

    let tui = Tui::<IQ_SLOTS, IQ_BLOCK, FFT_N>::new(
        app,
        gain_table,
        DEFAULT_STEP_HZ,
        consumer_fft,
        ctrl_tx,
    );
    let _ = tui.run();

    let _ = source_handle.join();
    let _ = ctrl_handle.join();
    Ok(())
}
