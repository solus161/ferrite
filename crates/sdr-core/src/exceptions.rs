use std::fmt::Display;

#[derive(Debug, PartialEq)]
pub enum CustomError {
    MismatchInputLength, // buffer length != input length
    InvalidIndex,
    SlowProducer, // ring empty: nothing new to read
    SlowConsumer, // lapped repeatedly; gave up after RETRIES attempts
    Speaker,
    Internal,
    IQStreamNotEven,
    RtlOpenDevice(u32),
    RtlSetFreq(u32),
    RtlSetBandwidth(u32),
    RtlSetSampleRate(u32),
    RtlSetGain(u32),
    RtlSetPpm(u32),
    RtlEnableAgc,
    RtlDisableAgc,
    RtlResetBuffer,
}

impl Display for CustomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RtlOpenDevice(id) => write!(f, "RTL-SDR: Failed to open device {}", &id),
            Self::RtlSetFreq(freq) => write!(f, "RTL-SDR: Failed to set center freq {}", &freq),
            Self::RtlSetBandwidth(bw) => write!(f, "RTL-SDR: Failed to set bandwidth {}", &bw),
            Self::RtlSetSampleRate(rate) => write!(f, "RTL-SDR: Failed to set sample rate {}", &rate),
            Self::RtlSetGain(db) => write!(f, "RTL-SDR: Failed to set tuner gain {} dB", &db),
            Self::RtlSetPpm(ppm) => write!(f, "RTL-SDR: Failed to set ppm correction {}", &ppm),
            Self::RtlEnableAgc => write!(f, "RTL-SDR: Failed to enable AGC"),
            Self::RtlDisableAgc => write!(f, "RTL-SDR: Failed to disable AGC"),
            Self::RtlResetBuffer => write!(f, "RTL-SDR: Failed to reset buffer"),
            _ => Ok(())
        }
    }
}

impl From<Box<dyn std::error::Error>> for CustomError {
    fn from(_: Box<dyn std::error::Error>) -> Self {
        Self::Internal
    }
}

// There is deliberately no `From<cpal::Error>` here: this crate must not depend
// on cpal, and the orphan rule forbids the binary from adding the impl itself
// (both `CustomError` and `cpal::Error` would be foreign to it). Nothing used
// it — `Speaker::play` surfaces `Box<dyn Error>`, which the impl above covers.
// If a real cpal conversion is ever needed, give `Speaker` a payload here or
// let the binary define its own error type wrapping this one.
