//! Audio pipeline: codec, jitter buffer (later), mixing (later).

pub mod codec;

pub use codec::{
    FRAME_MS, MAX_PACKET_SIZE, OpusDecoder, OpusEncoder, SAMPLE_RATE, SAMPLES_PER_FRAME,
};
