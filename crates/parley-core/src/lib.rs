//! Shared engine used by the server *and* every client:
//! session I/O, audio pipeline, (later) jitter buffer, mixing, rate control.

pub mod audio;
pub mod error;
pub mod session;

pub use error::Error;
