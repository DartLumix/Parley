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

/// Largest datagram total that survives a connection whose path-MTU
/// estimate has COLLAPSED to the 1200-byte floor — quinn's black-hole
/// detection does this after sustained loss (observed live in the 6e
/// netem run: 1199-byte datagrams rejected with TooLarge minutes into
/// 10% loss). 1200 minus QUIC packet overhead (short header + CID +
/// PN + 16-byte AEAD tag, ~35B) minus the DATAGRAM frame size bound.
/// Video fragments are sized from THIS, not MAX_DATAGRAM_SIZE: a
/// datagram that only fits while the MTU estimate is healthy dies the
/// moment it collapses. Audio packets (~60–660 B) fit naturally.
pub const MTU_SAFE_DATAGRAM_SIZE: usize = 1150;

/// Set when the sender's voice activity detection fired.
pub const FLAG_SPEAKING: u8 = 1 << 0;

/// Set on video datagrams (Phase 5). Payload carries the video fragment
/// sub-header (see video.rs).
pub const FLAG_VIDEO: u8 = 1 << 1;

/// Stereo audio payload (6d): the Opus frame is interleaved L/R.
/// Receivers key the decoder configuration off this flag PER PACKET —
/// never off negotiated state (same rule as FLAG_VIDEO).
pub const FLAG_STEREO: u8 = 1 << 2;

/// 7k: this audio datagram is a SCREEN SHARE's system audio, not the
/// sender's voice. The two streams are separate (the share runs its own
/// music-profile encoder), so a receiver can decide what to play
/// independently — the app plays share audio only while that share is
/// being watched, and the sender's voice is never affected.
///
/// Audio-only: shares bit 3 with the video codec id, which is
/// meaningful only when FLAG_VIDEO is set. No datagram is ever both.
pub const FLAG_SHARE_AUDIO: u8 = 1 << 3;

/// 7h: video codec identity, carried in flag bits 3..4 of VIDEO
/// datagrams. The media header has no codec field and the flag byte has
/// two spare bits, so the codec rides there: no length change, no
/// fragment-size change, and the relay (which only decodes the media
/// header) needs no change at all.
///
/// Why it exists: the receive path is H.264-only, and a stream in any
/// other codec used to produce pure silence — frames assembled, the
/// decoder rejected every one, nothing on screen and nothing in the log.
/// With the codec on the wire a receiver can refuse the stream and name
/// it, which is the difference between a five-minute diagnosis and a
/// lost evening.
pub const VIDEO_CODEC_SHIFT: u8 = 3;
pub const VIDEO_CODEC_MASK: u8 = 0b11 << VIDEO_CODEC_SHIFT;
/// Codec ids (flag bits 3..4). Only H.264 exists today.
pub const VIDEO_CODEC_H264: u8 = 0;

/// Codec id from a datagram's flags (meaningful only with FLAG_VIDEO).
pub fn video_codec(flags: u8) -> u8 {
    (flags & VIDEO_CODEC_MASK) >> VIDEO_CODEC_SHIFT
}

/// Set the codec id, preserving every other flag bit (absence of the
/// bits means H.264 — every pre-7h sender — so the default is safe).
pub fn with_video_codec(flags: u8, codec: u8) -> u8 {
    (flags & !VIDEO_CODEC_MASK) | ((codec & 0b11) << VIDEO_CODEC_SHIFT)
}

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

    #[test]
    fn stereo_flag_survives_roundtrip() {
        // 6d: FLAG_STEREO must round-trip through the 5 flag bits.
        let encoded = encode(FLAG_STEREO, 1, 96_000, 1, &[0xAA]);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.flags, FLAG_STEREO);
    }

    #[test]
    fn share_audio_flag_is_independent_of_the_audio_flags() {
        // 7k: the share-audio marker rides alongside SPEAKING/STEREO on
        // an audio datagram and never touches the video codec bits.
        let flags = FLAG_SHARE_AUDIO | FLAG_STEREO | FLAG_SPEAKING;
        let encoded = encode(flags, 5, 960, 12, &[0x55]);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.flags, flags);
        assert_ne!(decoded.flags & FLAG_SHARE_AUDIO, 0);
        assert_eq!(decoded.flags & FLAG_VIDEO, 0);
    }

    #[test]
    fn video_codec_bits_roundtrip_and_leave_other_flags_alone() {
        // 7h: the codec id shares the flag byte with SPEAKING/VIDEO/STEREO.
        let flags = with_video_codec(FLAG_VIDEO | FLAG_SPEAKING, VIDEO_CODEC_H264);
        assert_eq!(video_codec(flags), VIDEO_CODEC_H264);
        assert_ne!(flags & FLAG_VIDEO, 0, "FLAG_VIDEO must survive");
        assert_ne!(flags & FLAG_SPEAKING, 0, "FLAG_SPEAKING must survive");
        let encoded = encode(flags, 3, 7, 9, &[1, 2, 3]);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.flags, flags);
        assert_eq!(video_codec(decoded.flags), VIDEO_CODEC_H264);
        // A future codec does not collide with the audio flags either.
        let hevc = with_video_codec(FLAG_VIDEO, 1);
        assert_eq!(video_codec(hevc), 1);
        assert_eq!(hevc & FLAG_STEREO, 0);
    }
}
