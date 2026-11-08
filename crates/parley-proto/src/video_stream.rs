//! Reliable video frames over QUIC unidirectional streams.
//!
//! The datagram path fragments one encoded frame across many datagrams.
//! That is fine for audio and for tiny delta frames, but one lost fragment
//! makes a whole keyframe undecodable. A video frame instead gets its own
//! unidirectional stream: QUIC retransmits the stream's bytes as needed,
//! and separate streams retire independently, so a slow frame cannot block
//! the next one (no head-of-line blocking between frames).
//!
//! Audio deliberately stays on datagrams: an Opus frame fits in one MTU,
//! and retransmitting a late packet would only add latency.
//!
//! The stream carries only a frame. The server stamps the authenticated
//! sender id before fan-out; a client must never be trusted to do so.

use crate::media::VIDEO_CODEC_H264;

/// Magic prefix, so a stray stream (a future extension, or a bug) is
/// rejected instead of being fed to a decoder.
pub const MAGIC: [u8; 4] = *b"PVS1";
/// Header version. Bump only with a negotiated capability revision.
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 20;

/// Largest encoded frame the transport accepts. This matches the existing
/// datagram path's frame ceiling; it is also the `read_to_end` bound that
/// keeps a malicious authenticated peer from allocating unbounded memory.
pub const MAX_FRAME_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_STREAM_BYTES: usize = MAX_FRAME_BYTES + HEADER_LEN;

const FLAG_KEYFRAME: u8 = 1 << 0;
const FLAG_HIGH_RATE: u8 = 1 << 1;

/// One frame as it travels on a QUIC stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFrame<'a> {
    pub sender_id: u32,
    pub frame_id: u32,
    pub timestamp: u32,
    pub keyframe: bool,
    pub high_rate: bool,
    pub codec: u8,
    pub data: &'a [u8],
}

/// Encode the fixed header. The payload follows immediately.
pub fn encode_header(frame: &StreamFrame<'_>) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0..4].copy_from_slice(&MAGIC);
    out[4] = VERSION;
    out[5] =
        (u8::from(frame.keyframe) * FLAG_KEYFRAME) | (u8::from(frame.high_rate) * FLAG_HIGH_RATE);
    out[6] = frame.codec;
    // out[7] reserved, stays zero.
    out[8..12].copy_from_slice(&frame.frame_id.to_le_bytes());
    out[12..16].copy_from_slice(&frame.timestamp.to_le_bytes());
    out[16..20].copy_from_slice(&frame.sender_id.to_le_bytes());
    out
}

/// Parse a complete stream payload (header plus frame bytes).
///
/// The returned slice borrows from `bytes`; callers that fan out clone the
/// payload once behind an `Arc` rather than copying per viewer.
pub fn decode_frame(bytes: &[u8]) -> Result<StreamFrame<'_>, &'static str> {
    if bytes.len() < HEADER_LEN {
        return Err("video stream shorter than its header");
    }
    if bytes.len() > MAX_STREAM_BYTES {
        return Err("video stream exceeds the frame ceiling");
    }
    if bytes[0..4] != MAGIC {
        return Err("video stream has the wrong magic");
    }
    if bytes[4] != VERSION {
        return Err("video stream has an unsupported version");
    }
    if bytes[7] != 0 {
        return Err("video stream has reserved flags set");
    }
    let codec = bytes[6];
    if codec != VIDEO_CODEC_H264 {
        return Err("video stream has an unsupported codec");
    }
    let frame_id = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let timestamp = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let sender_id = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    Ok(StreamFrame {
        sender_id,
        frame_id,
        timestamp,
        keyframe: bytes[5] & FLAG_KEYFRAME != 0,
        high_rate: bytes[5] & FLAG_HIGH_RATE != 0,
        codec,
        data: &bytes[HEADER_LEN..],
    })
}

/// Build one complete stream payload. Used by the sender and by tests.
pub fn encode_frame(frame: &StreamFrame<'_>) -> Vec<u8> {
    let header = encode_header(frame);
    let mut out = Vec::with_capacity(header.len() + frame.data.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(frame.data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: &[u8]) -> StreamFrame<'_> {
        StreamFrame {
            sender_id: 42,
            frame_id: 7,
            timestamp: 1234,
            keyframe: true,
            high_rate: true,
            codec: VIDEO_CODEC_H264,
            data,
        }
    }

    #[test]
    fn header_round_trips() {
        let original = frame(b"encoded bytes");
        let encoded = encode_frame(&original);
        let decoded = decode_frame(&encoded).unwrap();
        assert_eq!(decoded.sender_id, original.sender_id);
        assert_eq!(decoded.frame_id, original.frame_id);
        assert_eq!(decoded.timestamp, original.timestamp);
        assert_eq!(decoded.keyframe, original.keyframe);
        assert_eq!(decoded.high_rate, original.high_rate);
        assert_eq!(decoded.codec, original.codec);
        assert_eq!(decoded.data, original.data);
    }

    #[test]
    fn rejects_malformed_streams() {
        let encoded = encode_frame(&frame(b"x"));
        assert!(decode_frame(&encoded[..HEADER_LEN - 1]).is_err());
        let mut wrong_magic = encoded.clone();
        wrong_magic[0] = b'X';
        assert!(decode_frame(&wrong_magic).is_err());
        let mut wrong_version = encoded.clone();
        wrong_version[4] = VERSION + 1;
        assert!(decode_frame(&wrong_version).is_err());
        let mut wrong_codec = encoded;
        wrong_codec[6] = 9;
        assert!(decode_frame(&wrong_codec).is_err());
    }

    #[test]
    fn flags_are_individual_bits() {
        let mut f = frame(b"x");
        f.keyframe = false;
        f.high_rate = true;
        let encoded = encode_frame(&f);
        let decoded = decode_frame(&encoded).unwrap();
        assert!(!decoded.keyframe);
        assert!(decoded.high_rate);
    }
}
