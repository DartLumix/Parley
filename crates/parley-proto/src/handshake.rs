//! Handshake constants and newtypes.
//! Hello/HelloAck messages are prost-generated (see `pb` module).

/// Wire-protocol version. Bump on breaking changes.
///
/// 1: media datagrams carry the sender's per-device `media_id` instead
///    of the account id. An old client cannot tell two devices apart and
///    would decode the new value as a different account, so server and
///    every client must move together (ALPN carries this number).
pub const PROTOCOL_VERSION: u32 = 1;

/// A channel's identity. Newtype — never a bare u64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelId(pub u64);

/// Bitset of optional capabilities each side advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities(pub u32);

impl Capabilities {
    pub const SCREEN_SHARE: Self = Self(1 << 0);
    pub const CAMERA: Self = Self(1 << 1);
    pub const HIGH_BITRATE_AUDIO: Self = Self(1 << 2);
    /// Video frames travel on reliable QUIC unidirectional streams rather
    /// than fragmented media datagrams. Negotiated in Hello/HelloAck; an
    /// older peer leaves it clear and keeps the datagram path.
    pub const VIDEO_STREAMS: Self = Self(1 << 3);

    pub fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_sets_union_and_membership() {
        let client = Capabilities::SCREEN_SHARE;
        let both = client.union(Capabilities::CAMERA);

        assert!(both.has(Capabilities::SCREEN_SHARE));
        assert!(both.has(Capabilities::CAMERA));
        assert!(!client.has(Capabilities::CAMERA));
    }
}
