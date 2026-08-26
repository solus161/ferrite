use std::{array, thread, time::Duration};

use cpal::{
    Stream,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

use sdr_core::{
    dsp::{DecimationWFM, Demodulation, windowed_sinc_resample, Deemphasis, PartialWriter},
    spmc::{RingConsumer, RingProducer}};

use crate::source::source::{AUDIO_DECIM, IQ_BLOCK, IQ_SLOTS, COMPLEX_BLOCK};

const FRAME_BLOCK: usize = 512;
const FM_DEVIATION: f32 = 75_000.0;
const DEEMPHASIS_TAU: f32 = 50e-6;

pub struct Speaker {
    stream: Stream,
    pub rtl_rate: u32,
    pub audio_rate: u32,
}

impl Speaker {
    /// `T` is pinned to `f32` because the callback writes straight into cpal's
    /// `&mut [f32]` — there is no meaningful conversion from an arbitrary `T`.
    pub fn new(consumer: RingConsumer<f32, IQ_SLOTS, IQ_BLOCK>) -> Self {
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
        println!("channels {}", &channels);
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

        // For decimation
        let mut decim = DecimationWFM::new();

        // Demodulation, outout 1024 300kHz
        let mut demod = Demodulation::new(FM_DEVIATION, 300_000.0_f32);
        
        // Deemphasis
        let mut deemph = Deemphasis::new(DEEMPHASIS_TAU, audio_rate as f32);

        let mut bufs = Box::new(DspBuffers::new());

        // A ring from DSP to audio
        let producer_audio = RingProducer::<f32, IQ_SLOTS, FRAME_BLOCK>::new(false);
        let consumer_audio = producer_audio.subscribe();
        
        // Partial write
        let mut writer = PartialWriter::<512>::new(producer_audio);
        // A dedicated thread DSP
        let builder = thread::Builder::new().name("thread-dsp".to_string());
        let _handle = builder.spawn(move || {
            loop {
                match consumer.read_into(&mut bufs.stg) {
                    Ok(_) => {
                        // Decimation
                        for (idx, pair) in bufs.stg.chunks_exact(2).enumerate() {
                            bufs.stg_i[idx] = pair[0];
                            bufs.stg_q[idx] = pair[1];
                        };
                        decim.process(&bufs.stg_i, &bufs.stg_q, &mut bufs.decim_i, &mut bufs.decim_q);
                        
                        // Demodulation at 300kHz
                        demod.process::<1024>(&bufs.decim_i, &bufs.decim_q, &mut bufs.demod_buf);

                        // Windowed sinc
                        windowed_sinc_resample(
                            &bufs.demod_buf, 
                            300_000.0f32,
                            audio_rate as f32,
                            &mut bufs.output,
                            32);

                        deemph.process(&mut bufs.output);

                        let _write_count = writer.write(&mut bufs.output);
                    }
                    Err(_) => thread::park_timeout(Duration::from_millis(1)),
                }
            }
        });

        let mut staging = [0.0f32; FRAME_BLOCK];
        let mut pos = FRAME_BLOCK;
        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // For 1 sec:
                    // - SDR has sample rate of 2.4MHz, each rate produces 2 IQ samples;
                    //   one call fill 16,384 IQ samples, or 8192 complex samples;
                    //   this is equivalent to 293 calls/s, 3.4ms between calls;
                    // - cpal askes for buf at 48kHz;
                    //   each buf is of size 1024, or 512 frames as there are 2 channels;
                    //   this is equivalent to 93.8 calls/s, 10.7ms between calls;
                    //
                    // So the catchup scenario is like this:
                    // - T0:
                    //   - SDR sampling, 
                    //   - cpal fires, but nothing to stream out
                    // - T0 + 3.4ms:
                    //   - SDR pushs 16,384 IQ samples into ring;
                    //   - cpal waits;         
                    // - T0 + 6.8ms:
                    //   - SDR pushs 
                    //   - cpal waits;
                    // - T0 + 10.2:
                    //   - SDR pushs, total 3x 16,384 samples in ring of size 16;
                    //   - cpal waits;
                    // - T0 + 10.7:
                    //   - SDR waits;
                    //   - cpal fires, get last 16,384 samples, convert to 512 samples
                    // println!("cpal data len {}", data.len());
                    for frame in data.chunks_mut(channels) {
                        if pos == FRAME_BLOCK {
                            match consumer_audio.read_into(&mut staging) {
                                Ok(_) => {
                                    pos = 0;
                                },
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

struct DspBuffers {
    pub stg: [f32; IQ_BLOCK],
    pub stg_i: [f32; COMPLEX_BLOCK],
    pub stg_q: [f32; COMPLEX_BLOCK],
    pub decim_i: [f32; 1024],
    pub decim_q: [f32; 1024],
    pub demod_buf: [f32; 1024],
    pub output: [f32; 163],
}

impl DspBuffers {
    pub fn new() -> Self {
        Self {
            stg: [0.0f32; IQ_BLOCK],
            stg_i: [0.0f32; COMPLEX_BLOCK],
            stg_q: [0.0f32; COMPLEX_BLOCK],
            decim_i: [0.0f32; 1024],
            decim_q: [0.0f32; 1024],
            demod_buf: [0.0f32; 1024],
            output: [0.0f32; 163],
        }
    }
}
