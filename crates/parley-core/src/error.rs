use std::fmt;

/// The crate-wide error type. One enum, one variant per failure class.
/// This is idiomatic Rust error handling - there are no exceptions.

#[derive(Debug)]
pub enum Error {
    /// Remote side violated the protocol (bad version, malformed frame).
    Protocol(&'static str),
    /// Transport-level failure (QUIC stream/datagram error)
    Transport(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Protocol(msg) => write!(f, "protocol violation: {msg}"),
            Error::Transport(msg) => write!(f, "transport error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}
