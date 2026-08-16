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

impl From<cpal::Error> for CustomError {
    fn from(_: cpal::Error) -> Self {
        Self::Speaker
    }
}
