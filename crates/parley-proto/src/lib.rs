//! The wire-protocol vocabulary shared by the server and every client.

pub mod frame;
pub mod handshake;
pub mod media;
pub mod video;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/parley.rs"));
}

pub use handshake::PROTOCOL_VERSION;
pub use pb::client_message::Body as ClientBody;
pub use pb::server_message::Body as ServerBody;
pub use pb::{
    AddUser, AuthDenied, AuthOk, AuthRequest, Channel, ChannelList, ChannelMode, ChatMessage,
    ClientMessage, CreateChannel, DeleteChannel, FetchHistory, Heartbeat, HeartbeatAck, Hello,
    HelloAck, HistoryResult, JoinVoice, KickUser, LeaveVoice, ListChannels, ManageRequest,
    ManageResult, ManageState, ManageStateRequest, ManagedChannel, ManagedUser, PresenceList,
    PresenceRequest, PresenceUser, ReceiverReport, RemoveUser, Role, SendChat, ServerMessage,
    SetMute, ShareStopped, StopShare, UpdateChannel, UpdateUser, VideoReport, VoiceState,
    VoiceStateList, VoiceStatesRequest,
};
