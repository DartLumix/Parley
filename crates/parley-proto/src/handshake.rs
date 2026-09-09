//! First exchange on the control stream: version + capability negotiation.

/// Wire-protocol version. Bump on breaking changes. Checked by both sides in "Hello".
pub const PROTOCOL_VERSION: u16 = 0;

/// A channel's identity. Newtype - see the note below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelId(pub u64);

/// Bitset of optional capabilities each side advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities(pub u32);

impl Capabilities {
    pub const SCREEN_SHARE: Self = Self(1 << 0);
    pub const CAMERA: Self = Self(1 << 1);
    pub const HIGH_BITRATE: Self = Self(1 << 2);

    /// True if every bit in 'other' is also set in 'self'.
    pub fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Capabilities of a side tha supports both 'self' and 'other'.
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Client -> server: "I speak version N, I can do there things"
#[derive(Debug, Clone)]
pub struct Hello {
    pub protocol_version: u16,
    pub caps: Capabilities,
}

/// Server -> client: chosen version, server's capabilities,
/// and how often the client must send heartbeats.
#[derive(Debug, Clone)]
pub struct HelloAck {
    pub protocol_version: u16,
    pub server_caps: Capabilities,
    pub heartbeat_ms: u32,
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
