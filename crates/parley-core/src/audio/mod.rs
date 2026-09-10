//! Audio pipeline: codec, jitter, resample, device I/O (client-only),
//! send/recv pipelines.

pub mod codec;
#[cfg(feature = "audio-device")]
pub mod device;
pub mod jitter;
pub mod pipeline;

pub use codec::{
    FRAME_MS, MAX_PACKET_SIZE, OpusDecoder, OpusEncoder, SAMPLE_RATE, SAMPLES_PER_FRAME,
};
#[cfg(feature = "audio-device")]
pub use device::{default_input_config, default_output_config, list_devices};
pub use jitter::{DEFAULT_TARGET_DEPTH, JitterBuffer, PopOutcome};
pub use pipeline::{RecvPipeline, SendPipeline};
