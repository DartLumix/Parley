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
    AddUser, AuthDenied, AuthOk, AuthRequest, AvatarData, BlobDownload, BlobKind, BlobReply,
    BlobRequest, BlobUpload, Channel, ChannelList, ChannelMode, ChatMessage, ClientMessage,
    CreateChannel, DeleteChannel, DeviceKind, DeviceSession, FetchHistory, Heartbeat, HeartbeatAck,
    Hello, HelloAck, HistoryResult, JoinVoice, KickUser, LeaveVoice, ListChannels,
    LogOutOtherDevices, ManageRequest, ManageResult, ManageState, ManageStateRequest,
    ManagedChannel, ManagedUser, PinMessage, Poke, Poked, PresenceList, PresenceRequest,
    PresenceUser, ProfileResult, ReceiverReport, RemoveUser, RequestAvatar, Role, SendChat,
    ServerMessage, SetMute, SetStatus, ShareStarted, ShareStopped, SoundRef, StopShare,
    UpdateChannel, UpdateProfile, UpdateUser, VideoReport, VoiceState, VoiceStateList,
    VoiceStatesRequest,
};
