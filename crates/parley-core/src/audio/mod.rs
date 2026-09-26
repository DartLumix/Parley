//! Audio pipeline: codec, jitter, mixing (device-agnostic), device I/O
//! (client-only, cpal), send/recv pipelines.

pub mod codec;
#[cfg(feature = "audio-device")]
pub mod device;
pub mod jitter;
pub mod mixer;
pub mod pipeline;

pub use codec::{
    FRAME_MS, MAX_PACKET_SIZE, OpusDecoder, OpusEncoder, SAMPLE_RATE, SAMPLES_PER_FRAME,
};
#[cfg(feature = "audio-device")]
pub use device::preferred_input_config;
pub use jitter::{DEFAULT_TARGET_DEPTH, JitterBuffer, PopOutcome};
pub use mixer::SharedMixer;
pub use pipeline::{RecvPipeline, SendPipeline};
