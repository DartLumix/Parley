//! The wire-protocol vocabulary shared by the server and every client.

pub mod frame;
pub mod handshake;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/parley.rs"));
}

pub use handshake::PROTOCOL_VERSION;
pub use pb::client_message::Body as ClientBody;
pub use pb::server_message::Body as ServerBody;
pub use pb::{
    AuthDenied, AuthOk, AuthRequest, Channel, ChannelList, ChatMessage, ClientMessage,
    FetchHistory, Heartbeat, HeartbeatAck, Hello, HelloAck, HistoryResult, ListChannels,
    PresenceList, PresenceRequest, PresenceUser, SendChat, ServerMessage,
};
