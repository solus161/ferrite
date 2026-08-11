#[derive(Debug, PartialEq)]
pub enum CustomError {
    MismatchInputLength, // buffer length != input length
    InvalidIndex,
    SlowProducer, // ring empty: nothing new to read
    SlowConsumer, // lapped repeatedly; gave up after RETRIES attempts
    Speaker,
    Internal,
    IQStreamNotEven,
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
