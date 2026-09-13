use std::sync::atomic::Ordering;
use std::sync::{Arc, atomic::{AtomicU64, AtomicU32, AtomicI32}};

use cpal::{
    Stream,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

use sdr_core::spmc::RingConsumer;

use crate::source::source::{AUDIO_DECIM, CPAL_BLOCK, IQ_SLOTS};

pub struct Speaker {
    stream: Stream,
    pub rtl_rate: u32,
    pub audio_rate: u32,
}

impl Speaker {
    /// `T` is pinned to `f32` because the callback writes straight into cpal's
    /// `&mut [f32]` — there is no meaningful conversion from an arbitrary `T`.
    ///
    /// `app` is read, never written, and only through atomics: the callback has
    /// a ~10.7 ms hard deadline, so it may not lock, allocate or log. Volume and
    /// mute arrive this way rather than over a channel for the same reason —
    /// see [`crate::log`] for the rule and PLAN.md §1 for why it exists.
    pub fn new(
        consumer: RingConsumer<f32, IQ_SLOTS, CPAL_BLOCK>,
        volume_scale: Arc<AtomicU32>,
        underruns: Arc<AtomicU64>,
        ) -> Self {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .expect("no output device available");

        // ── Audio output config ─────────────────────────────────────────────────
        // Take the device's default; derive the RTL rate so decimation is an exact
        // integer.
        let out_cfg = device
            .default_output_config()
            .expect("Error getting config for default output");
        let channels = out_cfg.channels() as usize;
        println!("channels {}", channels);
        let audio_rate = out_cfg.sample_rate(); // u32 in cpal 0.18
        let rtl_rate = audio_rate * AUDIO_DECIM;

        // cpal decides the output rate, so this is the first place either rate
        // is known. The Info panel reads both; `sample_rate` is corrected again
        // once librtlsdr reports what its divider rounded to.
        // app.audio_rate.store(audio_rate, Relaxed);
        // app.sample_rate.store(rtl_rate, Relaxed);

        println!(
            "audio: {} Hz × {} ch  |  rtl: {} Hz  |  decim: {}",
            audio_rate, channels, rtl_rate, AUDIO_DECIM
        );

        let config = cpal::StreamConfig {
            channels: out_cfg.channels(),
            sample_rate: audio_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let mut staging = [0.0f32; CPAL_BLOCK];
        let mut pos = CPAL_BLOCK;
        // cpal starts asking for frames as soon as `play()` is called, which is
        // before `Source::new` has finished opening the dongle — roughly half a
        // second of frames that starve because nothing has produced a sample
        // yet. Counting those makes the Info panel's underrun row read ~24 000
        // on a perfectly healthy radio, which is worse than not measuring at
        // all. An underrun is the ring running dry *after* it has been fed.

        // let volume_scale = Arc::new(RwLock::new(1.0_f32));
        // let volume_scale_clone = volume_scale.clone();

        let mut started = false;
        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // Rates, per second:
                    // - the SDR delivers 16,384 bytes = 8192 IQ pairs per USB
                    //   transfer at 2.4 MS/s: 293 transfers/s, 3.41 ms apart.
                    //   Each yields 163 or 164 audio samples (8192/50 = 163.84),
                    //   which `push` accumulates into CPAL_BLOCK-sized blocks.
                    // - cpal asks for 512 frames at 48 kHz: 93.8 calls/s,
                    //   10.7 ms apart, so one callback drains ~3.1 ring blocks.
                    //
                    // The 163.84 is fractional, so ring blocks never align with
                    // USB transfers. That is harmless because `push` carries the
                    // fill position in the producer: block edges and transfer
                    // edges are independent, and the sample stream stays
                    // contiguous across both.
                    // One load per callback, not per frame: volume cannot
                    // usefully change inside 10.7 ms, and a per-sample atomic
                    // read would defeat autovectorisation of the copy loop.

                    for frame in data.chunks_mut(channels) {
                        if pos == CPAL_BLOCK {
                            match consumer.read_into(&mut staging) {
                                Ok(_) => {
                                    pos = 0;
                                    started = true;
                                }
                                Err(_) => {
                                    // Error mean sample rate mismatch 
                                    // Bump up underruns for display
                                    if started {
                                        underruns.fetch_add(1, Ordering::Relaxed);
                                    }
                                    for out in frame.iter_mut() {
                                        *out = 0.0;
                                    }
                                    continue;
                                }
                            }
                        }

                        let s = staging[pos] * volume_scale.load(Ordering::Relaxed) as f32 / 100.0;
                        pos += 1;
                        for out in frame.iter_mut() {
                            *out = s; // mono → every channel
                        }
                    }
                },
                move |err| eprintln!("audio stream error: {err}"),
                None,
            )
            .expect("Error setting up cpal stream");

        Self {
            stream,
            rtl_rate,
            audio_rate,
        }
    }

    pub fn play(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.stream.play()?;
        Ok(())
    }
}

/// Apply squelch to volume scale, based on RSSI level
/// `TRANSITION_BAND` is needed so volume does not changed drastically
fn squelch_scale(rssi_dbfs_x10: &Arc<AtomicI32>, squelch: &Arc<AtomicI32>) -> f32 {
    // It's 5.0 db but x10 to match with RSSI dbfs x10
    const TRANSITION_BAND: i32 = 50;    
    let squelch_start = squelch.load(Ordering::Relaxed) * 10 - TRANSITION_BAND;
    rssi_dbfs_x10.load(Ordering::Relaxed).saturating_sub(squelch_start).clamp(0, TRANSITION_BAND) as f32 / TRANSITION_BAND as f32
}
