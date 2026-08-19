use cpal::{
    Stream,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

use rust_radio::{source::source::AUDIO_DECIM, spmc::RingConsumer};

pub struct Speaker {
    stream: Stream,
    pub rtl_rate: u32,
    pub audio_rate: u32,
}

impl Speaker {
    /// `T` is pinned to `f32` because the callback writes straight into cpal's
    /// `&mut [f32]` — there is no meaningful conversion from an arbitrary `T`.
    pub fn new<const N: usize, const M: usize>(consumer: RingConsumer<f32, N, M>) -> Self {
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
        let audio_rate = out_cfg.sample_rate(); // u32 in cpal 0.18
        let rtl_rate = audio_rate * AUDIO_DECIM;

        println!(
            "audio: {} Hz × {} ch  |  rtl: {} Hz  |  decim: {}",
            audio_rate, channels, rtl_rate, AUDIO_DECIM
        );

        let config = cpal::StreamConfig {
            channels: out_cfg.channels(),
            sample_rate: audio_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        // Staging copy of one ring block. cpal asks for an arbitrary frame count
        // per callback while the ring hands out fixed M-sample blocks, so the
        // remainder has to survive between callbacks.
        //
        // Filled via `read_into`, which copies before releasing the slot. Doing
        // it the other way round — `read()` then copy — leaves a window where the
        // producer can overwrite the block mid-copy, and an audio callback is a
        // prime candidate for being preempted inside it.
        let mut staging = [0.0f32; M];
        let mut pos = M; // start "drained" so the first callback pulls a block

        // ── TEMPORARY INSTRUMENTATION — delete once the silence is diagnosed ──
        // tail -f /tmp/ferrite-spk.log
        let mut dbg_log = std::fs::File::create("/tmp/ferrite-spk.log").ok();
        if let Some(f) = dbg_log.as_mut() {
            use std::io::Write;
            let _ = writeln!(
                f,
                "device={:?}  {} Hz  {} ch  sample_format={:?}",
                device.name().unwrap_or_else(|_| "<unnamed>".into()),
                audio_rate,
                channels,
                out_cfg.sample_format(),
            );
            let _ = writeln!(f, "-- available output devices --");
            if let Ok(devs) = host.output_devices() {
                for d in devs {
                    let _ = writeln!(f, "   {}", d.name().unwrap_or_else(|_| "<unnamed>".into()));
                }
            }
        }
        let mut dbg_cb = 0u64; // callbacks
        let mut dbg_frames = 0u64; // frames cpal asked for
        let mut dbg_ok = 0u64; // blocks successfully read
        let mut dbg_under = 0u64; // frames filled with silence
        let mut dbg_at = std::time::Instant::now();

        // cpal drives this callback on its own real-time thread.
        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    dbg_cb += 1;
                    dbg_frames += (data.len() / channels) as u64;

                    for frame in data.chunks_mut(channels) {
                        if pos == M {
                            match consumer.read_into(&mut staging) {
                                Ok(()) => {
                                    pos = 0;
                                    dbg_ok += 1;
                                }
                                Err(_) => {
                                    // Ring empty — underrun. Emit silence for this
                                    // frame and retry on the next one, so a block
                                    // landing mid-callback is picked up immediately.
                                    for out in frame.iter_mut() {
                                        *out = 0.0;
                                    }
                                    dbg_under += 1;
                                    continue;
                                }
                            }
                        }

                        let s = staging[pos];
                        pos += 1;
                        for out in frame.iter_mut() {
                            *out = s; // mono → every channel
                        }
                    }

                    // ── TEMPORARY INSTRUMENTATION ────────────────────────────
                    if dbg_at.elapsed() >= std::time::Duration::from_secs(1) {
                        use std::io::Write;
                        if let Some(f) = dbg_log.as_mut() {
                            let _ = writeln!(
                                f,
                                "{:>5} cb  {:>6} frames/s  {:>4} blocks read ({} samples)  \
                                 {:>6} silent frames ({:.1}%)",
                                dbg_cb,
                                dbg_frames,
                                dbg_ok,
                                dbg_ok * M as u64,
                                dbg_under,
                                100.0 * dbg_under as f64 / dbg_frames.max(1) as f64,
                            );
                        }
                        dbg_cb = 0;
                        dbg_frames = 0;
                        dbg_ok = 0;
                        dbg_under = 0;
                        dbg_at = std::time::Instant::now();
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
