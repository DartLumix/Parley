//! Audio pipeline: codec, jitter buffer, (later) mixing.

pub mod codec;
pub mod jitter;

pub use codec::{
    FRAME_MS, MAX_PACKET_SIZE, OpusDecoder, OpusEncoder, SAMPLE_RATE, SAMPLES_PER_FRAME,
};
pub use jitter::{DEFAULT_TARGET_DEPTH, JitterBuffer, PopOutcome};
