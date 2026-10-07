//! Shared engine used by the server *and* every client: session I/O,
//! audio pipeline (codec, jitter, mixing via the client mixer), rate
//! control (loss ladders), and the display-agnostic video receive hub.

pub mod audio;
pub mod error;
pub mod session;
#[cfg(feature = "video")]
pub mod video;
#[cfg(feature = "video")]
pub mod video_receive;
#[cfg(feature = "vaapi")]
pub mod video_vaapi;

pub use error::Error;
