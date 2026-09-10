//! Audio pipeline: codec, jitter buffer, (optional) device I/O, (later) mixing.

pub mod codec;
#[cfg(feature = "audio-device")]
pub mod device;
pub mod jitter;

pub use codec::{
    FRAME_MS, MAX_PACKET_SIZE, OpusDecoder, OpusEncoder, SAMPLE_RATE, SAMPLES_PER_FRAME,
};
#[cfg(feature = "audio-device")]
pub use device::{default_input_config, default_output_config, list_devices};
pub use jitter::{DEFAULT_TARGET_DEPTH, JitterBuffer, PopOutcome};
