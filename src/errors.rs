use sqa_jack::errors::JackError;

pub type EngineResult<T> = Result<T, EngineError>;

#[derive(Debug, Fail)]
pub enum EngineError {
    #[fail(display = "JACK error: {}", _0)]
    Jack(JackError),
    #[fail(display = "Engine channel or sender limit exceeded")]
    LimitExceeded,
    #[fail(display = "No such channel.")]
    NoSuchChannel
}
impl From<JackError> for EngineError {
    fn from(je: JackError) -> EngineError {
        EngineError::Jack(je)
    }
}
