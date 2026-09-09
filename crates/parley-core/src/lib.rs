//! Shared engine used by the server *and* every client
//! QUIC session, media pipeline, jitter buffer, mixing, rate control.
//! Stub today - contents arrive with Phase 1.

pub mod error;

pub use error::Error;
