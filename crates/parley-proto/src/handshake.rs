//! Handshake constants and newtypes.
//! Hello/HelloAck messages are prost-generated (see `pb` module).

/// Wire-protocol version. Bump on breaking changes.
///
/// 1: media datagrams carry the sender's per-device `media_id` instead
///    of the account id.
/// 2: screen, camera and share audio moved from the custom QUIC media
///    streams/datagrams to the MoQ media plane. `HelloAck` advertises the
///    listener port. There is no dual-stack path: server and every client
///    must move together (ALPN carries this number).
pub const PROTOCOL_VERSION: u32 = 2;

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
    /// Screen video, camera video and share audio are published and
    /// subscribed on the MoQ listener. Voice chat deliberately stays on
    /// the existing QUIC datagrams and does not require this bit.
    pub const MOQ_MEDIA: Self = Self(1 << 3);

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
