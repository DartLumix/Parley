//! Length-prefixed framing for control-stream messages.
//! Format: [4-byte big-endian length N][N bytes of protobuf payload]

/// Maximum accepted message size (64 KiB).
pub const MAX_SIZE: usize = 64 * 1024;

/// Frame a payload: prepend a 4-byte big-endian length.
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}
