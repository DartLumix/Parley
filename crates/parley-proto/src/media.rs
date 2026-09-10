//! Media datagrams: hand-packed header + codec payload, sent as QUIC
//! unreliable datagrams (RFC 9221). Hot path — no protobuf here.
//!
//! Layout (all big-endian):
//!   [0]       version:3 | flags:5
//!   [1..3]    seq: u16
//!   [3..7]    timestamp: u32 (codec clock, e.g. Opus sample count)
//!   [7..11]   sender_id: u32 (stamped by the server)
//!   [11..]    payload (Opus frame)

pub const MEDIA_VERSION: u8 = 0;

/// Keep whole datagrams under the typical QUIC path MTU budget.
pub const MAX_DATAGRAM_SIZE: usize = 1200;

/// Set when the sender's voice activity detection fired.
pub const FLAG_SPEAKING: u8 = 1 << 0;

pub const HEADER_LEN: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaDatagram<'a> {
    pub flags: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub sender_id: u32,
    pub payload: &'a [u8],
}

pub fn encode(flags: u8, seq: u16, timestamp: u32, sender_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.push((MEDIA_VERSION << 5) | (flags & 0x1f));
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf.extend_from_slice(&sender_id.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

pub fn decode(bytes: &[u8]) -> Result<MediaDatagram<'_>, &'static str> {
    if bytes.len() < HEADER_LEN {
        return Err("datagram too short");
    }
    if bytes[0] >> 5 != MEDIA_VERSION {
        return Err("unsupported media version");
    }
    Ok(MediaDatagram {
        flags: bytes[0] & 0x1f,
        seq: u16::from_be_bytes([bytes[1], bytes[2]]),
        timestamp: u32::from_be_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]),
        sender_id: u32::from_be_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]),
        payload: &bytes[HEADER_LEN..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let payload = [1u8, 2, 3, 4, 5];
        let encoded = encode(FLAG_SPEAKING, 7, 96_000, 42, &payload);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.flags, FLAG_SPEAKING);
        assert_eq!(decoded.seq, 7);
        assert_eq!(decoded.timestamp, 96_000);
        assert_eq!(decoded.sender_id, 42);
        assert_eq!(decoded.payload, &payload);
    }

    #[test]
    fn empty_payload_roundtrip() {
        let encoded = encode(0, 0, 0, 0, &[]);
        let decoded = decode(&encoded).unwrap();
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn rejects_short() {
        assert!(decode(&[0u8; 5]).is_err());
    }

    #[test]
    fn rejects_bad_version() {
        let mut buf = encode(0, 0, 0, 0, &[]);
        buf[0] = 0xFF; // version bits = 7
        assert!(decode(&buf).is_err());
    }
}
