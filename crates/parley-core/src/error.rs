/// The crate-wide error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Remote side violated the protocol (bad version, malformed frame).
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    /// Transport-level failure (QUIC stream/datagram error).
    #[error("transport error: {0}")]
    Transport(String),
}
