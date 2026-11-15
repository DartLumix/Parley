//! Shared engine used by the server *and* every client: session I/O,
//! audio pipeline (codec, jitter, mixing via the client mixer), rate
//! control (loss ladders), and the display-agnostic video receive hub.

pub mod adaptive;
pub mod audio;
pub mod error;
#[cfg(feature = "video")]
pub mod moq_media;
pub mod session;
#[cfg(feature = "video")]
pub mod video;
#[cfg(all(feature = "video", target_os = "android"))]
mod video_android;
#[cfg(feature = "video")]
pub mod video_receive;
#[cfg(feature = "vaapi")]
pub mod video_vaapi;
#[cfg(all(feature = "vaapi", target_os = "linux"))]
mod video_vaapi_decode;
#[cfg(all(feature = "video", windows))]
mod video_windows;

pub use error::Error;
