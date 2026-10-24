//! parley-client library: the test-client's modules split from main so
//! integration tests can exercise audio processing without hardware
//! (see tests/aec.rs).

pub mod cert;
pub mod endpoint;

/// The share-audio echo guard: subtract this app's playback from the
/// default-output monitor. Pure DSP, no device dependency — both the
/// Flutter app and the terminal client run it.
pub mod share_echo;

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

/// Share lifecycle events (6a), reported by the capture supervisor (or
/// the synthetic sender) back to the client's command task. Which
/// variants get constructed depends on build features — hence the allow.
#[allow(dead_code)]
pub enum ShareEvent {
    /// Capture pipeline is up and streaming.
    Started,
    /// Start failed (portal denied/cancelled, pipewire error).
    StartFailed(String),
    /// Share fully torn down; the client may now signal StopShare.
    /// `external` = the desktop closed it from its sharing indicator.
    Ended { external: bool },
}
