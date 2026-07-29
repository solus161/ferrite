#[derive(Debug, PartialEq)]
pub enum CustomError {
    MismatchInputLength,    // buffer length != input length
    InvalidIndex,
    SlowProducer,           // ring empty: nothing new to read
    SlowConsumer,           // lapped repeatedly; gave up after RETRIES attempts
}
