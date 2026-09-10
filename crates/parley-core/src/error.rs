/// The crate-wide error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("stream read: {0}")]
    ReadExact(#[from] quinn::ReadExactError),
    #[error("stream write: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("protobuf decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("message exceeds size limit")]
    MessageTooLarge,
    #[error("opus codec: {0}")]
    Codec(String),
    #[error("audio device: {0}")]
    AudioDevice(String),
}
