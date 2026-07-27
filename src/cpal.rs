use cpal::{Stream, traits::{DeviceTrait, HostTrait, StreamTrait}};

use crate::spmc::RingConsumer;

pub struct Cpal {
    stream: Stream,
    pub rtl_rate: u32,
    pub audio_rate: u32,
}

/// RTL sample rate = audio sample rate × this. 50 keeps both 48k and 44.1k in
/// the RTL's valid range (2.4 MS/s and 2.205 MS/s). Also the boxcar decimation
/// factor — see the DSP callback.
pub const AUDIO_DECIM: u32 = 50;

impl Cpal {
    /// `T` is pinned to `f32` because the callback writes straight into cpal's
    /// `&mut [f32]` — there is no meaningful conversion from an arbitrary `T`.
    pub fn new<const N: usize, const M: usize>(consumer: RingConsumer<f32, N, M>) -> Self
    {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .expect("no output device available");

        // ── Audio output config ─────────────────────────────────────────────────
        // Take the device's default; derive the RTL rate so decimation is an exact
        // integer.
        let out_cfg = device.default_output_config().expect("Error getting config for default output");
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
        // This is a *copy*, not a borrow into the slot. The producer is
        // non-blocking and overwrites slots regardless of readers, so a `&[f32]`
        // held across callbacks would rot underneath us; copying narrows the
        // exposure to the memcpy itself.
        let mut staging = [0.0f32; M];
        let mut pos = M; // start "drained" so the first callback pulls a block

        // cpal drives this callback on its own real-time thread.
        let stream = device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    if pos == M {
                        match consumer.read() {
                            Ok(block) => {
                                staging.copy_from_slice(block);
                                pos = 0;
                            }
                            Err(_) => {
                                // Ring empty — underrun. Emit silence for this
                                // frame and retry on the next one, so a block
                                // landing mid-callback is picked up immediately.
                                for out in frame.iter_mut() {
                                    *out = 0.0;
                                }
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
            },
            move |err| eprintln!("audio stream error: {err}"),
            None,
        ).expect("Error setting up cpal stream");

        Self { stream, rtl_rate, audio_rate }
    }

    pub fn play(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.stream.play()?;
        Ok(())
    }
}
