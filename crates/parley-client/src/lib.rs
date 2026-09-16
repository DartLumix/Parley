//! parley-client library: the test-client's modules split from main so
//! integration tests can exercise audio processing without hardware
//! (see tests/aec.rs).

pub mod cert;

#[cfg(feature = "audio-device")]
pub mod aec;
#[cfg(feature = "audio-device")]
pub mod audio;
#[cfg(feature = "audio-device")]
pub mod mixer;
#[cfg(feature = "screen-capture")]
pub mod screen;
#[cfg(feature = "video")]
pub mod video;
