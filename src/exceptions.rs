#[derive(Debug, PartialEq)]
pub enum CustomError {
    MismatchInputLength,    // buffer length != input length
    InvalidIndex,
    SlowProducer,
}
