use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

mod spmc;
mod buffer;
mod exceptions;
mod utils;

/// RTL sample rate = audio sample rate × this. 50 keeps both 48k and 44.1k in
/// the RTL's valid range (2.4 MS/s and 2.205 MS/s). Also the boxcar decimation
/// factor — see the DSP callback.
const AUDIO_DECIM: u32 = 50;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Device discovery ────────────────────────────────────────────────────
    println!("===RTL-SDR devices===");
    for (i, name) in rtlsdr_mt::devices().enumerate() {
        println!("#{i}: {}", name.to_string_lossy());
    }

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("no output device available");

    // ── Audio output config ─────────────────────────────────────────────────
    // Take the device's default; derive the RTL rate so decimation is an exact
    // integer.
    let out_cfg = device.default_output_config()?;
    let channels = out_cfg.channels() as usize;
    let audio_rate = out_cfg.sample_rate(); // u32 in cpal 0.18
    let rtl_rate = audio_rate * AUDIO_DECIM;
    println!(
        "audio: {} Hz × {} ch  |  rtl: {} Hz  |  decim: {}",
        audio_rate, channels, rtl_rate, AUDIO_DECIM
    );

    // ── Ring b (placeholder) ────────────────────────────────────────────────
    // Mutex<VecDeque<f32>> stands in for the bounded SPSC audio ring in the
    // plan. A lock in the audio callback breaks the "never block in the
    // callback" rule — fine for first-sound, replace before it matters.
    let audio_buf = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let cb_buf = Arc::clone(&audio_buf);

    let config = cpal::StreamConfig {
        channels: out_cfg.channels(),
        sample_rate: audio_rate,
        buffer_size: cpal::BufferSize::Default,
    };

    // cpal drives this callback on its own real-time thread.
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let mut buf = cb_buf.lock().unwrap();
            for frame in data.chunks_mut(channels) {
                let s = buf.pop_front().unwrap_or(0.0); // underrun → silence
                for out in frame.iter_mut() {
                    *out = s; // mono → every channel
                }
            }
        },
        move |err| eprintln!("audio stream error: {err}"),
        None,
    )?;
    stream.play()?;

    // ── SDR setup (librtlsdr via rtlsdr_mt) ─────────────────────────────────
    let (mut ctl, mut reader) =
        rtlsdr_mt::open(0).map_err(|_| "rtlsdr: failed to open device 0")?;
    ctl.set_center_freq(100_000_000)
        .map_err(|_| "rtlsdr: set_center_freq failed")?; // 100.0 MHz — set to a strong local station
    ctl.set_sample_rate(rtl_rate)
        .map_err(|_| "rtlsdr: set_sample_rate failed")?;
    ctl.enable_agc()
        .map_err(|_| "rtlsdr: enable_agc failed")?; // FC0013: auto gain for first sound
    ctl.reset_buffer()
        .map_err(|_| "rtlsdr: reset_buffer failed")?; // flush stale USB buffers before streaming

    // ── DSP: runs inside librtlsdr's async read callback ────────────────────
    // read_async blocks this (main) thread and invokes the closure per USB
    // buffer. cpal's callback runs on its own thread → two threads, plus
    // librtlsdr's internal USB thread.
    let (mut prev_i, mut prev_q) = (0.0f32, 0.0f32); // FM discriminator state
    let mut acc = 0.0f32; // boxcar accumulator for decimate-by-AUDIO_DECIM
    let mut acc_n = 0u32;
    let volume = 0.3f32;
    let cap = audio_rate as usize; // ~1 s backlog ceiling

    reader
        .read_async(15, 16384, move |bytes| {
            // bytes: interleaved u8 IQ [I0,Q0,I1,Q1,...] at rtl_rate
            for pair in bytes.chunks_exact(2) {
                // u8 → centered f32 in ~[-1, 1]
                let i = (pair[0] as f32 - 127.5) * (1.0 / 127.5);
                let q = (pair[1] as f32 - 127.5) * (1.0 / 127.5);

                // FM demod: angle of cur × conj(prev). Instantaneous frequency.
                let re = i * prev_i + q * prev_q;
                let im = q * prev_i - i * prev_q;
                let demod = im.atan2(re); // ∈ [-π, π]
                prev_i = i;
                prev_q = q;

                // Decimate rtl_rate → audio_rate by averaging AUDIO_DECIM samples.
                acc += demod;
                acc_n += 1;
                if acc_n == AUDIO_DECIM {
                    let sample =
                        (acc / AUDIO_DECIM as f32) * (1.0 / std::f32::consts::PI) * volume;
                    acc = 0.0;
                    acc_n = 0;

                    let mut b = audio_buf.lock().unwrap();
                    if b.len() < cap {
                        b.push_back(sample);
                    } // else: producer outrunning consumer — drop (clock mismatch)
                }
            }
        })
        .map_err(|_| "rtlsdr: read_async failed")?;

    Ok(())
}
