mod speaker;

use rust_radio::{exceptions::CustomError, source::Source, spmc::RingProducer};

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

fn main() -> Result<(), CustomError> {
    // ── Device discovery ────────────────────────────────────────────────────
    println!("===RTL-SDR devices===");
    for (i, name) in rtlsdr_mt::devices().enumerate() {
        println!("#{i}: {}", name.to_string_lossy());
    }

    // Ring producer
    // Non-blocking: a slow DSP consumer must never stall the USB callback, so
    // the ring overwrites and drags the reader forward to the freshest data.
    let producer = RingProducer::<f32, RING_SLOTS, RING_BLOCK>::new(false);

    // A consumer for cpal
    let consumer_cpal = producer.subscribe();

    // cpal itself
    let cpal = Speaker::new(consumer_cpal);
    cpal.play()?;

    let mut source = Source::<RING_SLOTS, RING_BLOCK>::new(cpal.rtl_rate, 104_000_000);
    let _ = source.receive(producer)?;

    Ok(())
}
