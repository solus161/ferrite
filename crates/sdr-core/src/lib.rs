//! Reusable SDR building blocks: the lock-free ring the USB callback publishes
//! into, the DSP that runs on the samples, and the shared vocabulary the app's
//! threads speak to each other.
//!
//! Nothing here depends on cpal, ratatui or rtlsdr_mt — `Cargo.toml` has an
//! empty `[dependencies]`, so the boundary is enforced by the compiler rather
//! than by convention. Device I/O and rendering live in the `ferrite` binary,
//! and the dependency arrow only ever runs `ferrite -> sdr-core`.
//!
//! Being a library is also what lets `benches/` link against it: a bench target
//! is its own crate and can never depend on a `main.rs`.

pub mod buffer;
pub mod complex;
pub mod control_signal;
pub mod dsp;
pub mod exceptions;
pub mod fft;
pub mod spmc;
mod fir;
mod fir_taps;
