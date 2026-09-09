//! Shared engine used by the server *and* every client:
//! session I/O, media pipeline, jitter buffer, mixing, rate control.

pub mod error;
pub mod session;

pub use error::Error;
