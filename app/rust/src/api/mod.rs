//! Bridge API surface — one module per feature area as the client grows.

pub mod session;
pub mod simple;
pub mod video;
pub mod voice;

/// 7k: our own playback, subtracted from the share-audio capture (the
/// feedback-loop guard). pub(crate) items only — nothing crosses the
/// bridge. Gate: the AEC lives behind audio-device.
#[cfg(feature = "audio-device")]
pub(crate) mod share_echo;
