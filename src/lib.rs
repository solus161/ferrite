//! Core building blocks for the radio, split out of the binary so that
//! benchmarks and integration tests can link against them — a `benches/` target
//! can only depend on a library target, not on `main.rs`.
//!
//! The cpal glue deliberately stays in the binary: it is application wiring, not
//! a reusable component.

pub mod buffer;
pub mod exceptions;
pub mod fft;
pub mod source;
pub mod spmc;
pub mod utils;
