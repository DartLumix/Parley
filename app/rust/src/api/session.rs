//! Connection/session bridge. Step 1: connect + auth + TOFU +
//! heartbeat + liveness. Step 2: the event stream — the reader task
//! forwards decoded ServerMessages into a channel; a forwarder task
//! owns the session's sinks and maps messages to SessionEvents.
//!
//! Session semantics match the terminal client: heartbeats at the
//! server-advertised interval (default 5 s; server reaps at 10 s QUIC
//! idle), session lifetime = control-stream lifetime, and the quinn
//! Endpoint is stored alongside the connection (dropping it closes
//! the UDP socket and kills the session).
//!
//! Plain types only across the bridge. Requests are fire-and-forget:
//! send, and the response arrives as an event (the event stream IS the
//! response path; the server broadcasts self-confirmation VoiceStates
//! and echoes chat to the sender). Bootstrap requests (presence,
//! channel list) are issued by the DART side after subscribing —
//! issuing them from connect() races sink registration.
//!
//! 7e VOICE OWNERSHIP: each LiveSession owns its voice state
//! (devices + media reader + encoder) as Option<VoiceControl>. The
//! session's OWN VoiceState events drive it — join enables (listen +
//! transmit, TeamSpeak shape; channel mode picks the encoder
//! profile), leave disables. Registry removal (disconnect / death
//! teardown) drops it too, so voice can never outlive its session —
//! the structural closure of the 7d voice-state-across-reconnects
//! gap. The enable path spawns on the blocking pool (device build
//! blocks ~100 ms) and installs the control via an epoch-guarded
//! async block: the SESSIONS lock is NEVER held across device
//! startup, and a leave/disconnect landing mid-install still wins
//! (the install checks the epoch under the lock at the end).

use parley_core::audio::mixer::{self as core_mixer, SharedMixer};
use std::collections::HashMap;
#[cfg(feature = "audio-device")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prost::Message as _;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedSender;

#[cfg(feature = "audio-device")]
use crate::api::voice::VoiceControl;

#[cfg(feature = "video")]
use crate::api::video;
use parley_client::cert;
use parley_core::session;
#[cfg(feature = "video")]
use parley_core::video_receive::SharedVideoReceiveHub;
use parley_proto::pb::manage_request::Action as ManageAction;
#[cfg(feature = "audio-device")]
use parley_proto::ReceiverReport;
#[cfg(feature = "video")]
use parley_proto::VideoReport;
use parley_proto::{
    AddUser, AuthDenied, AuthOk, AuthRequest, ChannelMode, ClientBody, ClientMessage,
    CreateChannel, DeleteChannel, FetchHistory, Heartbeat, Hello, HelloAck, JoinVoice, KickUser,
    LeaveVoice, ListChannels, ManageRequest, ManageStateRequest, PresenceRequest, RemoveUser, Role,
    SendChat, ServerBody, ServerMessage, SetMute, UpdateChannel, UpdateUser, VoiceStatesRequest,
    PROTOCOL_VERSION,
};

use crate::frb_generated::StreamSink;

/// One tokio runtime for the whole bridge (FRB calls run on their own
/// worker threads; block_on there never touches the UI isolate).
static RT: OnceLock<Runtime> = OnceLock::new();
pub(crate) fn rt() -> &'static Runtime {
    RT.get_or_init(|| Runtime::new().expect("tokio runtime"))
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
/// 7g share-audio mic route: the share-audio mux owns this — while the
/// flag is set the mic tee routes mic chunks into the mux (mixed with
/// the share's system audio at FLAG_STEREO 256 kbps), otherwise steer
/// back to the normal voice encoder. Owned by the mux; flipped back on
/// every mux exit (cargo-style: terminal client's `mic_route`).
#[cfg(all(feature = "audio-device", feature = "screen-capture"))]
static SHARE_MIC_ROUTE: LazyLock<Arc<AtomicBool>> =
    LazyLock::new(|| Arc::new(AtomicBool::new(false)));

#[cfg(all(feature = "audio-device", feature = "screen-capture"))]
pub(crate) fn share_mic_route() -> &'static Arc<AtomicBool> {
    &SHARE_MIC_ROUTE
}

/// 7g: the parked mic receiver for the share-audio mux (the tee's
/// mux-side channel). start_voice installs it; the mux borrows it and
/// hands it back on exit. None when voice has started without the
/// share feature or the mux is currently holding it (sys-only
/// degradation while).
#[cfg(all(feature = "audio-device", feature = "screen-capture"))]
static SHARE_MIC_MUX_SLOT: LazyLock<Mutex<Option<tokio::sync::mpsc::Receiver<Vec<f32>>>>> =
    LazyLock::new(|| Mutex::new(None));

/// 7g: the tee's mux-side channel for a new voice enable. Fresh every
/// enable (the mux may still be draining an old receiver from the last
/// share; a fresh channel drops nothing — the tee holds the only
/// sender and it routes exclusively by the flag). The parked slot only
/// matters for the receiver the mux RETURNS; that flow passes through
/// SHARE_MIC_MUX_SLOT untouched here.
#[cfg(all(feature = "audio-device", feature = "screen-capture"))]
fn share_mic_mux_channel() -> (
    tokio::sync::mpsc::Sender<Vec<f32>>,
    tokio::sync::mpsc::Receiver<Vec<f32>>,
) {
    tokio::sync::mpsc::channel(64)
}
/// Monotonic enable epoch (7e): guards the async install path. Each
/// start_voice attempt takes a ticket; only the CURRENT ticket may
/// install — a leave/disconnect/newer-join landing mid-install
/// invalidates the older one.
#[cfg(feature = "audio-device")]
static VOICE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Commands the session writer accepts (the heartbeat it generates
/// itself). pub(crate): the voice module's loss reporter (7e2) sends
/// ReceiverReport through the same writer path as every other command.
#[derive(Debug)]
pub(crate) enum Command {
    ListChannels,
    JoinVoice {
        channel_id: u64,
    },
    LeaveVoice,
    SendChat {
        channel_id: u64,
        text: String,
    },
    FetchHistory {
        channel_id: u64,
        limit: u32,
        before_message_id: u64,
    },
    Presence,
    /// My microphone is (un)muted — roster news for everyone else. The
    /// local capture flag flips first (set_mic_muted); this only makes
    /// the state visible to the other clients.
    SetMute {
        muted: bool,
    },
    /// Voice-membership snapshot ("voice_states" event). VoiceState
    /// broadcasts cover every join/leave that happens while we are
    /// connected, but a peer already sitting in a channel before we
    /// connect is invisible until this snapshot arrives — the UI's
    /// per-channel roster needs it once at connect.
    VoiceStates,
    /// 7e2 (4d): our loss report — one aggregate worst-peer permille,
    /// sent every 5 s in speech channels. reporter_id: 0 — the server
    /// stamps the authenticated id and routes the report to the other
    /// voice-channel members (never back to us). Gated with the voice
    /// path: without audio-device nothing constructs it.
    #[cfg(feature = "audio-device")]
    ReceiverReport {
        gap_rate_permille: u32,
    },
    /// 7f (6e): OUR video report as a VIEWER — worst per-sender gap
    /// rate, every 5 s while streams are active. Feeds the sharer's
    /// video bitrate ladder (VideoLadder), exactly as 7e2 feeds the
    /// audio ladder. Gated with the video path.
    #[cfg(feature = "video")]
    VideoReport {
        gap_rate_permille: u32,
        /// Pictures per second this viewer sustains (0 = unknown). The
        /// sharer's pace governor steps resolution down to it; see
        /// `screen::PaceGovernor`.
        viewer_fps: u32,
    },
    /// 7g: after the local share's supervisor reported Ended, tell the
    /// server (never before Ended — no datagram may follow the notice).
    #[cfg(feature = "screen-capture")]
    StopShare,
    /// Management page refresh (answer: a `manage` event).
    ManageState,
    ManageAddUser {
        username: String,
        password: String,
        role: String,
    },
    ManageUpdateUser {
        user_id: u32,
        username: String,
        password: String,
        role: String,
    },
    ManageRemoveUser {
        user_id: u32,
    },
    ManageCreateChannel {
        name: String,
        mode: String,
    },
    ManageUpdateChannel {
        channel_id: u64,
        name: String,
        mode: String,
    },
    ManageDeleteChannel {
        channel_id: u64,
    },
    ManageKick {
        user_id: u32,
        channel_id: u64,
    },
}

/// The bridge carries roles/modes as the same strings the tables show;
/// the server validates them again, so an unknown value here must never
/// become an over-grant. Unknown role strings are USER.
fn role_code(role: &str) -> i32 {
    match role {
        "admin" => Role::Admin as i32,
        "mod" => Role::Mod as i32,
        _ => Role::User as i32,
    }
}

fn mode_code(mode: &str) -> i32 {
    if mode == "music" {
        ChannelMode::Music as i32
    } else {
        ChannelMode::Speech as i32
    }
}

impl Command {
    fn into_client_body(self) -> ClientBody {
        match self {
            Command::ListChannels => ClientBody::ListChannels(ListChannels {}),
            Command::JoinVoice { channel_id } => ClientBody::JoinVoice(JoinVoice { channel_id }),
            Command::LeaveVoice => ClientBody::LeaveVoice(LeaveVoice {}),
            Command::SendChat { channel_id, text } => {
                ClientBody::SendChat(SendChat { channel_id, text })
            }
            Command::FetchHistory {
                channel_id,
                limit,
                before_message_id,
            } => ClientBody::FetchHistory(FetchHistory {
                channel_id,
                limit,
                before_message_id,
            }),
            Command::Presence => ClientBody::PresenceRequest(PresenceRequest {}),
            Command::SetMute { muted } => ClientBody::SetMute(SetMute { muted }),
            Command::VoiceStates => ClientBody::VoiceStatesRequest(VoiceStatesRequest {}),
            #[cfg(feature = "audio-device")]
            Command::ReceiverReport { gap_rate_permille } => {
                ClientBody::ReceiverReport(ReceiverReport {
                    gap_rate_permille,
                    reporter_id: 0, // server stamps the authenticated id
                })
            }
            #[cfg(feature = "video")]
            Command::VideoReport {
                gap_rate_permille,
                viewer_fps,
            } => ClientBody::VideoReport(VideoReport {
                gap_rate_permille,
                reporter_id: 0, // server stamps the authenticated id
                viewer_fps,
            }),
            #[cfg(feature = "screen-capture")]
            Command::StopShare => ClientBody::StopShare(parley_proto::StopShare {}),
            Command::ManageState => ClientBody::Manage(ManageRequest {
                action: Some(ManageAction::State(ManageStateRequest {})),
            }),
            Command::ManageAddUser {
                username,
                password,
                role,
            } => ClientBody::Manage(ManageRequest {
                action: Some(ManageAction::AddUser(AddUser {
                    username,
                    password,
                    role: role_code(&role),
                })),
            }),
            Command::ManageUpdateUser {
                user_id,
                username,
                password,
                role,
            } => ClientBody::Manage(ManageRequest {
                action: Some(ManageAction::UpdateUser(UpdateUser {
                    user_id,
                    username,
                    password,
                    role: role_code(&role),
                })),
            }),
            Command::ManageRemoveUser { user_id } => ClientBody::Manage(ManageRequest {
                action: Some(ManageAction::RemoveUser(RemoveUser { user_id })),
            }),
            Command::ManageCreateChannel { name, mode } => ClientBody::Manage(ManageRequest {
                action: Some(ManageAction::CreateChannel(CreateChannel {
                    name,
                    mode: mode_code(&mode),
                })),
            }),
            Command::ManageUpdateChannel {
                channel_id,
                name,
                mode,
            } => ClientBody::Manage(ManageRequest {
                action: Some(ManageAction::UpdateChannel(UpdateChannel {
                    channel_id,
                    name,
                    mode: mode_code(&mode),
                })),
            }),
            Command::ManageDeleteChannel { channel_id } => ClientBody::Manage(ManageRequest {
                action: Some(ManageAction::DeleteChannel(DeleteChannel { channel_id })),
            }),
            Command::ManageKick {
                user_id,
                channel_id,
            } => ClientBody::Manage(ManageRequest {
                action: Some(ManageAction::KickUser(KickUser {
                    user_id,
                    channel_id,
                })),
            }),
        }
    }
}

struct LiveSession {
    /// Kept alive: dropping it closes the UDP socket.
    _endpoint: quinn::Endpoint,
    /// Kept alive: dropping it closes the QUIC connection (streams die
    /// with it; the server ends the session on control-stream close).
    _conn: quinn::Connection,
    /// Writer-side command channel.
    cmd_tx: UnboundedSender<Command>,
    /// Flipped false by the reader task when the control stream ends.
    alive: Arc<AtomicBool>,
    /// Event sinks, shared with the forwarder task (events() registers
    /// through this handle).
    sinks: SinkList,
    /// The session's voice mixer — shared with the voice module's
    /// device reader/callback (start_voice takes it; membership events
    /// below add/remove voices).
    mixer: SharedMixer,
    /// Authenticated user id (media reader's self-echo filter).
    user_id: u32,
    /// Mic mute: the capture callback drops chunks while this is set
    /// (no packets at all, rather than silence packets). Session-scoped
    /// and deliberately not cleared by leave/join — muting is a user
    /// preference, not a device property.
    mic_muted: Arc<AtomicBool>,
    /// Capture peak (f32 bits) written by the mic callback, read by the
    /// UI's self speaking indicator. 0 while muted or out of voice.
    #[cfg(feature = "audio-device")]
    mic_level: Arc<AtomicU32>,
    /// 7g: event stream origin for share lifecycle events (share_
    /// started / share_ended) and the share supervisor's forwarder.
    /// Featureless builds never touch it (share fns are stubs) —
    /// kept for the one-struct-serves-all-configs simplicity.
    #[cfg(feature = "screen-capture")]
    event_tx: UnboundedSender<SessionEvent>,
    /// 7g share state (the in-app "Share screen" button):
    /// controller slot + keyframe-poke slot + the loss slot the share
    /// encoder's ladder reads (viewers' VideoReports land here).
    #[cfg(feature = "screen-capture")]
    share_ctrl: Arc<Mutex<Option<parley_client::screen::ShareController>>>,
    #[cfg(feature = "screen-capture")]
    share_poke: Arc<Mutex<Option<parley_client::screen::ShareHandle>>>,
    #[cfg(feature = "screen-capture")]
    share_loss: Arc<AtomicU32>,
    /// Worst decode rate reported by a viewer (0 = none yet). Read by
    /// the share's rate governor; written by to_event.
    #[cfg(feature = "screen-capture")]
    share_viewer_fps: Arc<AtomicU32>,
    /// When each viewer last reported and what it said, so the minimum
    /// of the ACTIVE viewers steers the pace (a viewer who stopped
    /// reporting must not pin the stream at its old rate).
    #[cfg(feature = "screen-capture")]
    viewer_fps_reports: Mutex<HashMap<u32, (u32, std::time::Instant)>>,
    /// The channel listing as seen by the last channel_list event —
    /// 7e profile selection: the mode of the channel being joined
    /// picks speech (mono + AEC) vs music (stereo 256 kbps, no AEC).
    /// Channels joined before any listing default to speech.
    channels: Vec<(u64, String)>,
    /// 7e2: the latest peer loss report (permille) for the send
    /// ladder. Written by to_event (reader task), read by the voice
    /// encoder's 1 Hz tick. 0 = no loss reported (yet).
    #[cfg(feature = "audio-device")]
    loss_report: Arc<AtomicU32>,
    /// 7f: the display bridge — consumer thread + per-sender Flutter
    /// textures, plus the receive hub they drain. Session-owned like
    /// the voice rig (7e rule): dropped with the registry entry,
    /// textures destroyed on the platform thread (see api/video.rs).
    #[cfg(feature = "video")]
    display: Option<video::DisplayBridge>,
    /// 7e: the session's voice (devices + reader + encoder). Some
    /// while the user is in a voice channel; dropping it tears
    /// everything down — voice can never outlive its session.
    #[cfg(feature = "audio-device")]
    voice: Option<VoiceControl>,
}

static SESSIONS: LazyLock<Mutex<HashMap<u32, LiveSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Shared sink list: the forwarder drains events into every registered
/// sink; `events()` registers through the registry's copy.
type SinkList = Arc<Mutex<Vec<StreamSink<SessionEvent>>>>;

/// One row of a channel listing.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub id: u64,
    pub name: String,
    /// "speech" | "music" (ChannelMode mapped to a string: plain types
    /// only across the bridge).
    pub mode: String,
}

/// One row of a presence listing.
#[derive(Debug, Clone)]
pub struct PresenceEntry {
    pub user_id: u32,
    pub username: String,
}

/// One chat row (live or history).
#[derive(Debug, Clone)]
pub struct ChatEntry {
    pub message_id: u64,
    pub channel_id: u64,
    pub user_id: u32,
    pub username: String,
    pub text: String,
    pub timestamp_ms: u64,
}

/// One voice-state snapshot row.
#[derive(Debug, Clone)]
pub struct VoiceEntry {
    pub user_id: u32,
    pub channel_id: u64,
    pub joined: bool,
    /// Their microphone is muted (announced, not inferred).
    pub muted: bool,
}

/// One row of the management users table.
#[derive(Debug, Clone)]
pub struct ManagedUserInfo {
    pub user_id: u32,
    pub username: String,
    /// "user" | "mod" | "admin"
    pub role: String,
    pub online: bool,
    /// 0 = not in a voice channel; otherwise the channel a kick would
    /// remove them from.
    pub channel_id: u64,
}

/// One row of the management channels table.
#[derive(Debug, Clone)]
pub struct ManagedChannelInfo {
    pub id: u64,
    pub name: String,
    /// "speech" | "music"
    pub mode: String,
    pub members: u32,
}

/// A full management snapshot. The page replaces its tables with this on
/// every response; there are no partial-update races.
#[derive(Debug, Clone)]
pub struct ManageStateInfo {
    pub users: Vec<ManagedUserInfo>,
    pub channels: Vec<ManagedChannelInfo>,
}

/// One sender's live output levels, for the roster's speaking glow.
/// `voice` and `share` are peaks (0..1) of the DECODED streams — the
/// listener's own volume does not change them.
#[derive(Debug, Clone)]
pub struct VoiceLevel {
    pub user_id: u32,
    pub voice: f64,
    pub share: f64,
}

/// 7f: one remote share's display surface. `texture_id` feeds a plain
/// Flutter `Texture` widget (pixels come from the Rust side); width /
/// height are the decoded stream's dimensions and `fps` the cadence the
/// display bridge measured (a provisional 30/60 from the stream's rate
/// flag until the first one-second window closes).
#[derive(Debug, Clone)]
pub struct VideoSurfaceInfo {
    pub user_id: u32,
    pub width: u64,
    pub height: u64,
    pub fps: u64,
    pub texture_id: i64,
}

/// Everything the server can tell the UI, as one flat event struct.
/// `kind` is one of: "channel_list", "presence", "chat", "history",
/// "voice_state", "voice_states", "video_started", "video_stats",
/// "video_stopped", "manage", "session_closed".
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub session_id: u32,
    pub kind: String,
    /// channel_list
    pub channels: Vec<ChannelInfo>,
    /// presence
    pub users: Vec<PresenceEntry>,
    /// chat / history
    pub messages: Vec<ChatEntry>,
    /// voice_state / voice_states (the single-row fields)
    pub user_id: u32,
    pub username: String,
    pub channel_id: u64,
    pub joined: bool,
    /// voice_state: their microphone is muted (see SetMute).
    pub muted: bool,
    /// voice_states (the full snapshot)
    pub states: Vec<VoiceEntry>,
    /// 7f: video_started / video_stopped (the surface)
    pub surface: Option<VideoSurfaceInfo>,
    /// "manage": the full users/channels snapshot. None on an error
    /// result (see `manage_error`).
    pub manage: Option<ManageStateInfo>,
    /// "manage": empty on success, the server's reason otherwise.
    pub manage_error: String,
}

/// Connected-session snapshot returned at connect time.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: u32,
    pub user_id: u32,
    pub session_token: String,
    pub server: String,
    pub fingerprint: String,
    /// "user" | "mod" | "admin" — gates the management page.
    pub role: String,
}

/// What a server presents on the wire, beside what this machine has
/// pinned — the facts behind a "trust new certificate" decision.
#[derive(Debug, Clone)]
pub struct ServerCertificate {
    /// The address as the user typed it.
    pub server: String,
    /// The fingerprint on file, if this server was trusted before.
    pub pinned: Option<String>,
    /// SHA-256 of the certificate the server presented just now.
    pub seen: String,
    /// `pinned == Some(seen)`; false when the identity changed.
    pub matches: bool,
}

struct Probe {
    key: String,
    path: std::path::PathBuf,
    fingerprint: String,
    pinned: Option<String>,
}

/// TLS handshake only: no Hello, no auth, no session, and no pin applied
/// (the point is to read what is presented even when it is not trusted).
/// Bounded so a firewalled UDP port fails in seconds.
async fn probe(server: &str) -> Result<Probe, String> {
    let (addr, server_name) = parley_client::endpoint::resolve(server)?;
    let key = parley_client::endpoint::pin_key(server)?;
    let (pinned, path) = cert::load_pin(&key)?;
    let (endpoint, seen) = cert::build_endpoint(None);
    let connecting = endpoint
        .connect(addr, &server_name)
        .map_err(|e| e.to_string())?;
    let conn = tokio::time::timeout(Duration::from_secs(10), connecting)
        .await
        .map_err(|_| format!("no TLS handshake with {server} within 10 s"))?
        .map_err(|e| e.to_string())?;
    let fingerprint = seen
        .lock()
        .unwrap()
        .clone()
        .ok_or("server presented no certificate")?;
    conn.close(0u32.into(), b"probe");
    Ok(Probe {
        key,
        path,
        fingerprint,
        pinned,
    })
}

/// The certificate a server presents right now, beside the pin we hold.
/// Read-only: nothing is trusted or written here.
pub fn probe_server(server: String) -> Result<ServerCertificate, String> {
    let probe = rt().block_on(probe(&server))?;
    Ok(ServerCertificate {
        matches: probe.pinned.as_deref() == Some(probe.fingerprint.as_str()),
        server,
        pinned: probe.pinned,
        seen: probe.fingerprint,
    })
}

/// Re-pin `server` to the certificate it presents now, returning that
/// fingerprint. Only call after the user compared it with the fingerprint
/// the server logs at startup — this is the "I know the identity changed"
/// step, and it is the one operation that can undo TOFU's protection.
pub fn trust_server(server: String) -> Result<String, String> {
    let probe = rt().block_on(probe(&server))?;
    cert::save_pin(&probe.path, &probe.key, &probe.fingerprint)?;
    Ok(probe.fingerprint)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read one length-prefixed frame raw — auth-phase messages are sent
/// bare (pre-oneof), so the auth reader must try AuthOk OR AuthDenied.
async fn read_frame(rx: &mut quinn::RecvStream) -> Result<Vec<u8>, String> {
    let mut len = [0u8; 4];
    rx.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let n = u32::from_be_bytes(len) as usize;
    if n > parley_proto::frame::MAX_SIZE {
        return Err(format!("frame of {n} bytes exceeds MAX_SIZE"));
    }
    let mut buf = vec![0u8; n];
    rx.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    Ok(buf)
}

fn closed_event(session_id: u32) -> SessionEvent {
    SessionEvent {
        session_id,
        kind: "session_closed".to_string(),
        channels: Vec::new(),
        users: Vec::new(),
        messages: Vec::new(),
        user_id: 0,
        username: String::new(),
        channel_id: 0,
        joined: false,
        muted: false,
        states: Vec::new(),
        surface: None,
        manage: None,
        manage_error: String::new(),
    }
}

/// Map one ServerMessage to SessionEvents (with empty defaults for
/// the fields a kind doesn't use). Returns a Vec because one message
/// can carry two UI facts (7f: a voice-leave is both a voice_state
/// AND a video_stopped).
fn to_event(session_id: u32, msg: &ServerMessage) -> Vec<SessionEvent> {
    let mut extra = Vec::new();
    // (The `?` below became a plain early-return: a body-less message
    // maps to zero events.)
    let ev = |kind: &str| SessionEvent {
        session_id,
        kind: kind.to_string(),
        channels: Vec::new(),
        users: Vec::new(),
        messages: Vec::new(),
        user_id: 0,
        username: String::new(),
        channel_id: 0,
        joined: false,
        muted: false,
        states: Vec::new(),
        surface: None,
        manage: None,
        manage_error: String::new(),
    };
    let Some(body) = msg.body.as_ref() else {
        return Vec::new();
    };
    match body {
        ServerBody::ChannelList(list) => {
            // Cache for the voice profile lookup below (7e).
            if let Some(s) = SESSIONS.lock().unwrap().get_mut(&session_id) {
                s.channels = list
                    .channels
                    .iter()
                    .map(|c| {
                        (
                            c.id,
                            match c.mode() {
                                ChannelMode::Music => "music",
                                ChannelMode::Speech => "speech",
                            }
                            .to_string(),
                        )
                    })
                    .collect();
            }
            let mut e = ev("channel_list");
            e.channels = list
                .channels
                .iter()
                .map(|c| ChannelInfo {
                    id: c.id,
                    name: c.name.clone(),
                    mode: match c.mode() {
                        ChannelMode::Music => "music",
                        ChannelMode::Speech => "speech",
                    }
                    .to_string(),
                })
                .collect();
            vec![e]
        }
        ServerBody::PresenceList(list) => {
            let mut e = ev("presence");
            e.users = list
                .users
                .iter()
                .map(|u| PresenceEntry {
                    user_id: u.user_id,
                    username: u.username.clone(),
                })
                .collect();
            vec![e]
        }
        ServerBody::ChatMessage(m) => {
            let mut e = ev("chat");
            e.user_id = m.user_id;
            e.username = m.username.clone();
            e.channel_id = m.channel_id;
            e.messages.push(ChatEntry {
                message_id: m.message_id,
                channel_id: m.channel_id,
                user_id: m.user_id,
                username: m.username.clone(),
                text: m.text.clone(),
                timestamp_ms: m.timestamp_ms,
            });
            vec![e]
        }
        ServerBody::HistoryResult(r) => {
            let mut e = ev("history");
            e.channel_id = r.messages.last().map(|m| m.channel_id).unwrap_or(0);
            e.messages = r
                .messages
                .iter()
                .map(|m| ChatEntry {
                    message_id: m.message_id,
                    channel_id: m.channel_id,
                    user_id: m.user_id,
                    username: m.username.clone(),
                    text: m.text.clone(),
                    timestamp_ms: m.timestamp_ms,
                })
                .collect();
            vec![e]
        }
        ServerBody::VoiceState(v) => {
            // A departed user's decoder state drops (the terminal
            // client's mixer::remove — done here so the event path and
            // the mix stay consistent). 7f: their video surface is
            // hidden too (hub forgets them; a re-join re-announces the
            // texture on their next frame). Emits video_stopped BEFORE
            // voice_state (the Vec return carries both).
            if !v.joined {
                #[cfg(feature = "video")]
                if let Some(s) = SESSIONS.lock().unwrap().get(&session_id) {
                    s.mixer.lock().unwrap().remove(v.user_id);
                    if let Some(d) = s.display.as_ref() {
                        d.sender_stopped(v.user_id, true);
                    }
                }
                #[cfg(not(feature = "video"))]
                if let Some(s) = SESSIONS.lock().unwrap().get(&session_id) {
                    s.mixer.lock().unwrap().remove(v.user_id);
                }
                #[cfg(feature = "video")]
                {
                    let mut e = ev("video_stopped");
                    e.user_id = v.user_id;
                    e.surface = Some(VideoSurfaceInfo {
                        user_id: v.user_id,
                        width: 0,
                        height: 0,
                        fps: 0,
                        texture_id: 0,
                    });
                    extra.push(e);
                }
            }
            // 7e: own join/leave DRIVES the local voice (join = listen
            // + transmit; TeamSpeak shape). Music profile iff the
            // joined channel's cached mode says music. Failures log;
            // the voice_state event still flows (the UI shows
            // membership truth). Featureless builds (CI): no local
            // voice exists — membership events stay pure UI state.
            if v.user_id == own_user_id(session_id) {
                if v.joined {
                    #[cfg(feature = "audio-device")]
                    enable_voice(session_id, v.channel_id);
                    #[cfg(not(feature = "audio-device"))]
                    let _ = v.channel_id;
                } else {
                    #[cfg(feature = "audio-device")]
                    disable_voice(session_id);
                }
            }
            let mut e = ev("voice_state");
            e.user_id = v.user_id;
            e.channel_id = v.channel_id;
            e.joined = v.joined;
            e.muted = v.muted;
            extra.push(e);
            extra
        }
        ServerBody::ReceiverReport(rep) => {
            // 7e2 (4d): a peer's loss report steers OUR send ladder —
            // it describes what that peer received (worst voice in
            // its mix). Skip unstamped reports and our own echo (the
            // server excludes the reporter from routing;
            // belt-and-suspenders, the terminal client's filter).
            // Stored for the voice encoder's 1 Hz tick — not a UI
            // event: the ladder is invisible by design. Featureless
            // builds have no voice path; reports are simply dropped.
            #[cfg(feature = "audio-device")]
            {
                let is_echo = rep.reporter_id == 0 || rep.reporter_id == own_user_id(session_id);
                if !is_echo {
                    if let Some(s) = SESSIONS.lock().unwrap().get(&session_id) {
                        s.loss_report
                            .store(rep.gap_rate_permille, Ordering::Relaxed);
                    }
                }
            }
            #[cfg(not(feature = "audio-device"))]
            let _ = rep;
            Vec::new()
        }
        // 7g: a peer's video report steers OUR share's bitrate ladder
        // (same shape as ReceiverReport → voice ladder). The server
        // routes VideoReport to the other voice-channel members; our
        // own echo is excluded (reporter_id stamped server-side).
        ServerBody::VideoReport(rep) => {
            #[cfg(feature = "screen-capture")]
            {
                let is_echo = rep.reporter_id == 0 || rep.reporter_id == own_user_id(session_id);
                if !is_echo {
                    if let Some(s) = SESSIONS.lock().unwrap().get(&session_id) {
                        s.share_loss.store(rep.gap_rate_permille, Ordering::Relaxed);
                        if rep.viewer_fps > 0 {
                            let now = std::time::Instant::now();
                            let mut reports = s.viewer_fps_reports.lock().unwrap();
                            reports.retain(|_, (_, when)| {
                                now.duration_since(*when) < Duration::from_secs(15)
                            });
                            reports.insert(rep.reporter_id, (rep.viewer_fps, now));
                            let worst = reports
                                .values()
                                .map(|(fps, _)| *fps)
                                .filter(|&fps| fps > 0)
                                .min()
                                .unwrap_or(0);
                            s.share_viewer_fps.store(worst, Ordering::Relaxed);
                        }
                    }
                }
            }
            #[cfg(not(feature = "screen-capture"))]
            {
                let _ = rep;
            }
            Vec::new()
        }
        ServerBody::VoiceStateList(l) => {
            let mut e = ev("voice_states");
            e.states = l
                .states
                .iter()
                .map(|v| VoiceEntry {
                    user_id: v.user_id,
                    channel_id: v.channel_id,
                    joined: v.joined,
                    muted: v.muted,
                })
                .collect();
            vec![e]
        }
        ServerBody::ShareStopped(s) => {
            // 7f: a sharer stopped (own echo filtered — the server
            // excludes the reporter from routing, but be defensive,
            // the terminal client's rule). Hub suppresses stragglers
            // until their next frame_id-0 keyframe; the display hides
            // the surface (kept, re-announced on a re-share — see
            // api/video.rs).
            #[cfg(not(feature = "video"))]
            {
                let _ = s;
                Vec::new()
            }
            #[cfg(feature = "video")]
            {
                if s.user_id != own_user_id(session_id) {
                    if let Some(sess) = SESSIONS.lock().unwrap().get(&session_id) {
                        // 7k: the sharer's audio stream ends with the
                        // share. Flush what is still buffered (and stop
                        // the decoder's PLC extrapolation) so no tail
                        // plays on after the card is gone — the stream
                        // itself keeps existing for their voice.
                        sess.mixer.lock().unwrap().remove(s.user_id);
                        if let Some(d) = sess.display.as_ref() {
                            d.sender_stopped(s.user_id, false);
                        }
                    }
                } else {
                    // OUR share ended (desktop sharing-indicator stop);
                    // clear the slots so "Share screen" can restart.
                    #[cfg(feature = "screen-capture")]
                    if let Some(sess) = SESSIONS.lock().unwrap().get(&session_id) {
                        *sess.share_ctrl.lock().unwrap() = None;
                        *sess.share_poke.lock().unwrap() = None;
                    }
                }
                let mut e = ev("video_stopped");
                e.user_id = s.user_id;
                e.surface = Some(VideoSurfaceInfo {
                    user_id: s.user_id,
                    width: 0,
                    height: 0,
                    fps: 0,
                    texture_id: 0,
                });
                vec![e]
            }
        }
        ServerBody::ManageResult(r) => {
            let mut e = ev("manage");
            if !r.ok {
                e.manage_error = r.error.clone();
            }
            if let Some(state) = r.state.as_ref() {
                e.manage = Some(ManageStateInfo {
                    users: state
                        .users
                        .iter()
                        .map(|u| ManagedUserInfo {
                            user_id: u.user_id,
                            username: u.username.clone(),
                            role: match u.role() {
                                Role::Admin => "admin",
                                Role::Mod => "mod",
                                Role::User => "user",
                            }
                            .to_string(),
                            online: u.online,
                            channel_id: u.channel_id,
                        })
                        .collect(),
                    channels: state
                        .channels
                        .iter()
                        .map(|c| ManagedChannelInfo {
                            id: c.channel_id,
                            name: c.name.clone(),
                            mode: match c.mode() {
                                ChannelMode::Music => "music",
                                ChannelMode::Speech => "speech",
                            }
                            .to_string(),
                            members: c.members,
                        })
                        .collect(),
                });
            }
            vec![e]
        }
        // HeartbeatAck: nothing to surface in the UI.
        _ => Vec::new(),
    }
}

fn own_user_id(session_id: u32) -> u32 {
    SESSIONS
        .lock()
        .unwrap()
        .get(&session_id)
        .map(|s| s.user_id)
        .unwrap_or(0)
}

/// 7e: the profile for a channel, from the cached listing. Unknown
/// channels default to speech (the conservative choice: AEC on).
#[cfg(feature = "audio-device")]
fn channel_is_music(session_id: u32, channel_id: u64) -> bool {
    SESSIONS
        .lock()
        .unwrap()
        .get(&session_id)
        .and_then(|s| s.channels.iter().find(|(id, _)| *id == channel_id))
        .map(|(_, mode)| mode == "music")
        .unwrap_or(false)
}

/// 7f: the per-session video wire — reader feeds it, a dedicated task
/// drains it into the hub (3b: decode never on the datagram reader).
#[cfg(feature = "video")]
fn start_video_task(session_id: u32, hub: SharedVideoReceiveHub) {
    // Unbounded: the hub itself bounds memory via its frame queue and
    // assembler caps; a burst of fragments just queues briefly.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u32, u8, Vec<u8>)>();
    rt().spawn(async move {
        while let Some((sender_id, codec, payload)) = rx.recv().await {
            hub.push_datagram_with_codec(sender_id, codec, &payload);
        }
    });
    VIDEO_FEEDS.lock().unwrap().insert(session_id, tx);
}

/// 7f: one session's video wire (reader → hub task).
#[cfg(feature = "video")]
/// (sender_id, codec id from the media flag bits, payload) — 7h.
type VideoFeed = tokio::sync::mpsc::UnboundedSender<(u32, u8, Vec<u8>)>;

/// 7f: feeds parked per session; enable_voice snapshots the one for
/// its session and hands it to start_voice.
#[cfg(feature = "video")]
static VIDEO_FEEDS: LazyLock<Mutex<HashMap<u32, VideoFeed>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 7e: enable voice for this session's own join. Runs entirely on the
/// blocking pool — the SESSIONS lock is never held across device
/// startup. Epoch-guarded: a newer epoch (re-join, leave, disconnect)
/// invalidates an in-flight install.
#[cfg(feature = "audio-device")]
fn enable_voice(session_id: u32, channel_id: u64) {
    let music = channel_is_music(session_id, channel_id);
    // Snapshot what the voice path needs; drop the lock immediately.
    // (7e2 adds the loss-report slot — reader task writes, encoder
    // ladder reads; 7f adds the video feed for the same media path.)
    #[cfg(feature = "screen-capture")]
    let mic_mux_tx = Some(share_mic_mux_channel().0);
    let (mixer, conn, my_id, loss_report, cmd_tx, video_tx, mic_muted, mic_level) = {
        let sessions = SESSIONS.lock().unwrap();
        let Some(s) = sessions.get(&session_id) else {
            return;
        };
        // Double-join with a LIVE control: short-circuit. A STOPPED
        // one (reader saw connection death) is replaced.
        if let Some(ctrl) = s.voice.as_ref() {
            if !ctrl.is_stopped() {
                return;
            }
        }
        (
            s.mixer.clone(),
            s._conn.clone(),
            s.user_id,
            s.loss_report.clone(),
            s.cmd_tx.clone(),
            #[cfg(feature = "video")]
            VIDEO_FEEDS.lock().unwrap().get(&session_id).cloned(),
            #[cfg(not(feature = "video"))]
            None,
            s.mic_muted.clone(),
            s.mic_level.clone(),
        )
    };

    let epoch = VOICE_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    tokio::task::spawn_blocking(move || {
        match crate::api::voice::start_voice(
            mixer,
            conn,
            my_id,
            music,
            loss_report,
            cmd_tx,
            video_tx,
            mic_muted,
            mic_level,
            #[cfg(feature = "screen-capture")]
            mic_mux_tx,
        ) {
            Ok(ctrl) => {
                // Install only if this epoch is still the newest —
                // else Drop the (already live) devices immediately.
                let mut sessions = SESSIONS.lock().unwrap();
                let current = VOICE_EPOCH.load(Ordering::SeqCst);
                match sessions.get_mut(&session_id) {
                    Some(s) if current == epoch => {
                        s.voice = Some(ctrl);
                        println!("voice: enabled for session {session_id}");
                    }
                    _ => {
                        drop(ctrl); // superseded: tear down what just started
                        println!(
                            "voice: enable for session {session_id} superseded mid-install — torn down"
                        );
                    }
                }
            }
            Err(e) => eprintln!("voice: enable failed: {e}"),
        }
    });
}

/// 7e: disable voice (own leave / session teardown). Bumps the epoch
/// FIRST so any in-flight enable's install is invalidated, then drops
/// the control — Drop flips the device stop flag, signals the reader,
/// aborts the encoder; the device thread self-terminates within
/// ~100 ms.
#[cfg(feature = "audio-device")]
fn disable_voice(session_id: u32) {
    VOICE_EPOCH.fetch_add(1, Ordering::SeqCst);
    let ctrl = SESSIONS
        .lock()
        .unwrap()
        .get_mut(&session_id)
        .and_then(|s| s.voice.take());
    // No capture callback will run again: the self meter must read 0
    // rather than the last chunk it saw.
    if let Some(s) = SESSIONS.lock().unwrap().get(&session_id) {
        s.mic_level.store(0f32.to_bits(), Ordering::Relaxed);
    }
    drop(ctrl); // Drop = teardown signal
}

/// Session teardown, used by disconnect (and valid at any death
/// point): voice first, then the registry entry.
#[cfg(feature = "audio-device")]
fn teardown_voice(session_id: u32) {
    disable_voice(session_id);
}

#[cfg(not(feature = "audio-device"))]
fn teardown_voice(_session_id: u32) {}

/// Connect + authenticate. Blocks until the handshake completes (runs
/// on an FRB worker thread; Dart sees a Future).
pub fn connect(server: String, user: String, password: String) -> Result<SessionInfo, String> {
    rt().block_on(connect_async(server, user, Auth::Password(password)))
}

/// Reconnect with a remembered session token (api/login.rs) instead of a
/// password. The server still issues and returns a fresh token, so the
/// caller should save the new one over the old.
pub fn connect_with_token(
    server: String,
    user: String,
    token: String,
) -> Result<SessionInfo, String> {
    rt().block_on(connect_async(server, user, Auth::Token(token)))
}

/// What the auth request carries: exactly one of the two credentials.
enum Auth {
    Password(String),
    Token(String),
}

async fn connect_async(server: String, user: String, auth: Auth) -> Result<SessionInfo, String> {
    // host[:port], with DNS. The user may type an IP, a LAN name or a
    // domain; the TOFU pin stays keyed by the exact string they typed.
    let (addr, server_name) = parley_client::endpoint::resolve(&server)?;

    // TOFU + endpoint (cert.rs — same path as the terminal client).
    // The pin key spells out the port, so "host" and "host:4443" are one
    // trust decision rather than two.
    let pin_key = parley_client::endpoint::pin_key(&server)?;
    let (pin, hosts_path) = cert::load_pin(&pin_key)?;
    let (endpoint, seen) = cert::build_endpoint(pin.clone());
    let conn = endpoint
        .connect(addr, &server_name)
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| {
            // A changed certificate is a decision for the user, not a
            // transport failure to dump on them: name both digests and
            // the file that holds the pin.
            let seen = seen.lock().unwrap().clone();
            cert::mismatch_report(&hosts_path, &pin_key, pin.as_deref(), seen.as_deref())
                .unwrap_or_else(|| format!("connect: {e}"))
        })?;

    // Handshake + auth (bare messages, pre-oneof).
    let (mut tx, mut rx) = conn.open_bi().await.map_err(|e| format!("open_bi: {e}"))?;
    session::write_message(
        &mut tx,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await
    .map_err(|e| format!("hello: {e}"))?;
    let ack: HelloAck = session::read_message(&mut rx)
        .await
        .map_err(|e| format!("hello ack: {e}"))?;

    let (password, session_token) = match auth {
        Auth::Password(p) => (p, String::new()),
        Auth::Token(t) => (String::new(), t),
    };
    session::write_message(
        &mut tx,
        &AuthRequest {
            username: user,
            password,
            session_token,
        },
    )
    .await
    .map_err(|e| format!("auth send: {e}"))?;
    let payload = read_frame(&mut rx).await?;
    let ok = if let Ok(ok) = AuthOk::decode(payload.as_slice()) {
        ok
    } else if let Ok(denied) = AuthDenied::decode(payload.as_slice()) {
        return Err(format!("auth denied: {}", denied.reason));
    } else {
        return Err("unexpected message during auth".into());
    };

    // TOFU: pin on first successful auth.
    let fingerprint = seen
        .lock()
        .unwrap()
        .clone()
        .ok_or("server presented no certificate")?;
    if pin.is_none() {
        cert::save_pin(&hosts_path, &pin_key, &fingerprint)?;
    }

    // Assign the session id BEFORE spawning tasks: the tasks stamp it
    // onto every event.
    let session_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

    // The voice mixer: created here, shared with the voice path.
    // Voice membership lives server-side; the mixer follows.
    let session_mixer: SharedMixer = core_mixer::new();
    // 7f: the video receive hub — always on while connected (the
    // display bridge and the FLAG_VIDEO datagram feed gate on it).
    #[cfg(feature = "video")]
    let session_hub: SharedVideoReceiveHub =
        parley_core::video_receive::VideoReceiveHub::shared_rgba();
    // --- event path: reader → channel → forwarder → sinks ---
    let sinks: SinkList = Arc::new(Mutex::new(Vec::new()));
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();

    // Forwarder: drains mapped events into every registered sink.
    // Exits after delivering session_closed (whichever task noticed
    // death first sends it; no duplicate can follow — this loop breaks).
    {
        let sinks = sinks.clone();
        tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                for sink in sinks.lock().unwrap().iter() {
                    let _ = sink.add(ev.clone());
                }
                if ev.kind == "session_closed" {
                    break;
                }
            }
        });
    }

    // Reader: control stream → decode → map → event channel.
    let alive = Arc::new(AtomicBool::new(true));
    let reader_alive = alive.clone();
    {
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut rx = rx;
            loop {
                match session::read_message::<ServerMessage>(&mut rx).await {
                    Ok(msg) => {
                        for ev in to_event(session_id, &msg) {
                            let _ = event_tx.send(ev);
                        }
                    }
                    Err(_) => {
                        reader_alive.store(false, Ordering::Relaxed);
                        let _ = event_tx.send(closed_event(session_id));
                        break;
                    }
                }
            }
        });
    }

    // --- writer task: heartbeats + commands ---
    let hb_ms = if ack.heartbeat_ms == 0 {
        5000
    } else {
        ack.heartbeat_ms as u64
    };
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    {
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut tx = tx;
            let mut tick = tokio::time::interval(Duration::from_millis(hb_ms));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let msg = ClientMessage {
                            body: Some(ClientBody::Heartbeat(Heartbeat {
                                client_time_ms: now_ms(),
                            })),
                        };
                        if session::write_message(&mut tx, &msg).await.is_err() {
                            break;
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(c) => {
                                if session::write_message(
                                    &mut tx,
                                    &ClientMessage { body: Some(c.into_client_body()) },
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                            None => break, // session dropped: registry removal
                        }
                    }
                }
            }
            // Writer exit = session over too (write failure often
            // precedes read failure): belt-and-suspenders closed event.
            let _ = event_tx.send(closed_event(session_id));
        });
    }

    // 7f: hub + display bridge + the video wire + the viewer-side
    // VideoReport reporter. The display (and its textures) is
    // session-owned; the wire task and reporter end when the command
    // channel / hub drop (registry removal).
    #[cfg(feature = "video")]
    start_video_task(session_id, session_hub.clone());
    #[cfg(feature = "video")]
    start_video_reporter(session_id, session_hub.clone(), cmd_tx.clone());

    SESSIONS.lock().unwrap().insert(
        session_id,
        LiveSession {
            _endpoint: endpoint,
            _conn: conn.clone(),
            cmd_tx,
            alive,
            sinks,
            mixer: session_mixer.clone(),
            user_id: ok.user_id,
            mic_muted: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "audio-device")]
            mic_level: Arc::new(AtomicU32::new(0)),
            #[cfg(feature = "screen-capture")]
            event_tx: event_tx.clone(),
            #[cfg(feature = "screen-capture")]
            share_ctrl: Arc::new(Mutex::new(None)),
            #[cfg(feature = "screen-capture")]
            share_poke: Arc::new(Mutex::new(None)),
            #[cfg(feature = "screen-capture")]
            share_loss: Arc::new(AtomicU32::new(0)),
            #[cfg(feature = "screen-capture")]
            share_viewer_fps: Arc::new(AtomicU32::new(0)),
            #[cfg(feature = "screen-capture")]
            viewer_fps_reports: Mutex::new(HashMap::new()),
            channels: Vec::new(),
            #[cfg(feature = "audio-device")]
            loss_report: Arc::new(AtomicU32::new(0)),
            #[cfg(feature = "video")]
            display: Some(video::DisplayBridge::start(
                session_hub.clone(),
                event_tx.clone(),
                session_id,
            )),
            #[cfg(feature = "audio-device")]
            voice: None,
        },
    );

    // 7k: share-expiry watchdog. A stop notice is the normal end of a
    // share; a crash, a killed capture thread or a dropped network ends
    // it silently, and the viewer would hold the card (showing the last
    // decoded frame) forever. Silence is the only signal in that case.
    #[cfg(feature = "video")]
    start_share_watchdog(session_id, session_hub.clone(), event_tx.clone());

    let role = match ok.role() {
        Role::Admin => "admin",
        Role::Mod => "mod",
        Role::User => "user",
    }
    .to_string();
    Ok(SessionInfo {
        session_id,
        user_id: ok.user_id,
        session_token: ok.session_token,
        server,
        fingerprint,
        role,
    })
}

fn send_command(session_id: u32, cmd: Command) -> Result<(), String> {
    let sessions = SESSIONS.lock().unwrap();
    let s = sessions
        .get(&session_id)
        .ok_or(format!("no session {session_id}"))?;
    s.cmd_tx.send(cmd).map_err(|e| format!("writer gone: {e}"))
}

/// Register a Dart-side event stream for this session. The Dart
/// wrapper exposes this as a Stream — every SessionEvent flows to it.
pub fn events(session_id: u32, sink: StreamSink<SessionEvent>) -> Result<(), String> {
    let sessions = SESSIONS.lock().unwrap();
    let s = sessions
        .get(&session_id)
        .ok_or(format!("no session {session_id}"))?;
    s.sinks.lock().unwrap().push(sink);
    Ok(())
}

/// Liveness: the reader task's flag (control stream still open).
pub fn is_connected(session_id: u32) -> bool {
    SESSIONS
        .lock()
        .unwrap()
        .get(&session_id)
        .map(|s| s.alive.load(Ordering::Relaxed))
        .unwrap_or(false)
}

/// Disconnect: voice devices die FIRST (deterministic teardown), then
/// the registry entry drops, and the QUIC connection is closed
/// EXPLICITLY.
///
/// The explicit close is load-bearing. Several background tasks (the
/// video reporter's command sender, the video wire, the voice reader)
/// hold clones of the connection or of the command channel, so merely
/// dropping the registry entry does not necessarily end the writer
/// task — and a control stream that never closes leaves the server's
/// session registered, so the account keeps showing as online for every
/// later login. Closing the connection is one deterministic signal the
/// server cannot miss.
pub fn disconnect(session_id: u32) -> Result<(), String> {
    teardown_voice(session_id);
    // 7f: the display bridge tears down OUTSIDE the registry lock —
    // its Drop waits (bounded) for the consumer thread and posts work
    // to the platform thread; neither belongs under the lock.
    #[cfg(feature = "video")]
    let display = SESSIONS
        .lock()
        .unwrap()
        .get_mut(&session_id)
        .and_then(|s| s.display.take());
    #[cfg(feature = "video")]
    drop(display);
    let (conn, endpoint) = match SESSIONS.lock().unwrap().remove(&session_id) {
        Some(s) => (s._conn, s._endpoint),
        None => return Err(format!("no session {session_id}")),
    };
    #[cfg(feature = "video")]
    VIDEO_FEEDS.lock().unwrap().remove(&session_id);
    conn.close(0u32.into(), b"disconnect");
    drop(conn);
    drop(endpoint);
    Ok(())
}

/// Mute or unmute this session's microphone. Takes effect in the
/// capture callback on the next device period (~21 ms); the flag
/// outlives leave/join, so a muted user stays muted when they switch
/// channels.
pub fn set_mic_muted(session_id: u32, muted: bool) -> Result<(), String> {
    let sessions = SESSIONS.lock().unwrap();
    let s = sessions
        .get(&session_id)
        .ok_or(format!("no session {session_id}"))?;
    s.mic_muted.store(muted, Ordering::Relaxed);
    #[cfg(feature = "audio-device")]
    s.mic_level.store(0f32.to_bits(), Ordering::Relaxed);
    // Roster news: the server keeps it and broadcasts a VoiceState so the
    // OTHER clients can show the badge. A dead writer means the session
    // is over anyway — the local flag still took effect.
    let _ = s.cmd_tx.send(Command::SetMute { muted });
    Ok(())
}

/// Clamp a UI slider value to something the mixer can use: 0 = silent,
/// 1 = the sender's own level, 2 = twice as loud (before the mix clamp).
fn volume_or_err(gain: f64) -> f32 {
    if !gain.is_finite() {
        return 1.0;
    }
    gain.clamp(0.0, 2.0) as f32
}

/// Set one user's output volume (their voice and their share). Session-
/// scoped: a new login starts everyone at 1.0.
pub fn set_user_volume(session_id: u32, user_id: u32, gain: f64) -> Result<(), String> {
    let mixer = {
        let sessions = SESSIONS.lock().unwrap();
        let s = sessions
            .get(&session_id)
            .ok_or(format!("no session {session_id}"))?;
        s.mixer.clone()
    };
    mixer.lock().unwrap().set_gain(user_id, volume_or_err(gain));
    Ok(())
}

/// Trim the SCREEN SHARE stream of one user, on top of their volume.
/// Their voice is untouched: the two are separate receive pipelines.
pub fn set_share_volume(session_id: u32, user_id: u32, gain: f64) -> Result<(), String> {
    let mixer = {
        let sessions = SESSIONS.lock().unwrap();
        let s = sessions
            .get(&session_id)
            .ok_or(format!("no session {session_id}"))?;
        s.mixer.clone()
    };
    mixer
        .lock()
        .unwrap()
        .set_share_gain(user_id, volume_or_err(gain));
    Ok(())
}

/// Current per-sender peaks, for the roster's speaking glow. Missing a
/// sender means "silent" — an offline or quiet stream has no entry.
pub fn voice_levels(session_id: u32) -> Vec<VoiceLevel> {
    let mixer = {
        let sessions = SESSIONS.lock().unwrap();
        match sessions.get(&session_id) {
            Some(s) => s.mixer.clone(),
            None => return Vec::new(),
        }
    };
    let levels = mixer.lock().unwrap().levels();
    levels
        .into_iter()
        .map(|(user_id, voice, share)| VoiceLevel {
            user_id,
            voice: voice as f64,
            share: share as f64,
        })
        .collect()
}

/// Our own microphone peak (0..1), for the self speaking indicator.
/// Written by the capture callback; 0 while muted or out of voice.
pub fn mic_level(session_id: u32) -> f64 {
    #[cfg(feature = "audio-device")]
    {
        if let Some(s) = SESSIONS.lock().unwrap().get(&session_id) {
            return f32::from_bits(s.mic_level.load(Ordering::Relaxed)) as f64;
        }
    }
    #[cfg(not(feature = "audio-device"))]
    let _ = session_id;
    0.0
}

/// Request the channel listing (response: "channel_list" event).
pub fn list_channels(session_id: u32) -> Result<(), String> {
    send_command(session_id, Command::ListChannels)
}

/// Join a voice channel (self-confirmation: "voice_state" event —
/// which then enables the local voice devices; 7e).
pub fn join_voice(session_id: u32, channel_id: u64) -> Result<(), String> {
    send_command(session_id, Command::JoinVoice { channel_id })
}

/// Leave the current voice channel (self-confirmation: "voice_state"
/// event — which then disables the local voice devices; 7e).
pub fn leave_voice(session_id: u32) -> Result<(), String> {
    send_command(session_id, Command::LeaveVoice)
}

/// Send a chat message (echo confirmation: "chat" event).
pub fn send_chat(session_id: u32, channel_id: u64, text: String) -> Result<(), String> {
    send_command(session_id, Command::SendChat { channel_id, text })
}

/// Fetch chat history (response: "history" event, newest-last).
pub fn fetch_history(
    session_id: u32,
    channel_id: u64,
    limit: u32,
    before_message_id: u64,
) -> Result<(), String> {
    send_command(
        session_id,
        Command::FetchHistory {
            channel_id,
            limit,
            before_message_id,
        },
    )
}

/// Request the presence listing (response: "presence" event).
pub fn request_presence(session_id: u32) -> Result<(), String> {
    send_command(session_id, Command::Presence)
}

/// Request the voice-membership snapshot (response: "voice_states"
/// event): every user currently in a voice channel, with its channel
/// id. The UI uses it for the per-channel roster; joins/leaves that
/// happen while connected arrive as individual "voice_state" events.
pub fn request_voice_states(session_id: u32) -> Result<(), String> {
    send_command(session_id, Command::VoiceStates)
}

// --- Server management (answer: a `manage` event with the full state) ---

/// Refresh the users/channels tables. Also called when the page opens.
pub fn manage_state(session_id: u32) -> Result<(), String> {
    send_command(session_id, Command::ManageState)
}

pub fn manage_add_user(
    session_id: u32,
    username: String,
    password: String,
    role: String,
) -> Result<(), String> {
    send_command(
        session_id,
        Command::ManageAddUser {
            username,
            password,
            role,
        },
    )
}

/// Empty `username`/`password` mean unchanged. `role` is the row's
/// current value; only an admin may change it, and the server enforces
/// that.
pub fn manage_update_user(
    session_id: u32,
    user_id: u32,
    username: String,
    password: String,
    role: String,
) -> Result<(), String> {
    send_command(
        session_id,
        Command::ManageUpdateUser {
            user_id,
            username,
            password,
            role,
        },
    )
}

pub fn manage_remove_user(session_id: u32, user_id: u32) -> Result<(), String> {
    send_command(session_id, Command::ManageRemoveUser { user_id })
}

pub fn manage_create_channel(session_id: u32, name: String, mode: String) -> Result<(), String> {
    send_command(session_id, Command::ManageCreateChannel { name, mode })
}

pub fn manage_update_channel(
    session_id: u32,
    channel_id: u64,
    name: String,
    mode: String,
) -> Result<(), String> {
    send_command(
        session_id,
        Command::ManageUpdateChannel {
            channel_id,
            name,
            mode,
        },
    )
}

pub fn manage_delete_channel(session_id: u32, channel_id: u64) -> Result<(), String> {
    send_command(session_id, Command::ManageDeleteChannel { channel_id })
}

pub fn manage_kick(session_id: u32, user_id: u32, channel_id: u64) -> Result<(), String> {
    send_command(
        session_id,
        Command::ManageKick {
            user_id,
            channel_id,
        },
    )
}

/// 7k: senders whose SCREEN SHARE audio this client is playing.
/// Watching is a UI decision; this is its audio half. Empty by default —
/// starting a share never plays its sound into the room unasked.
#[cfg(feature = "audio-device")]
static SHARE_AUDIO_AUDIBLE: LazyLock<Mutex<std::collections::HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// Is this sender's share audio being played? Read by the media reader
/// (voice.rs) for every FLAG_SHARE_AUDIO datagram.
#[cfg(feature = "audio-device")]
pub(crate) fn share_audio_audible(user_id: u32) -> bool {
    SHARE_AUDIO_AUDIBLE.lock().unwrap().contains(&user_id)
}

/// Play (or stop playing) one sharer's screen audio. The UI calls this
/// with the share it is watching — the pane, the fullscreen viewer, or
/// nothing at all. Only the share's system audio is affected: the
/// sender's voice rides its own stream and keeps playing.
///
/// Featureless: reports the stub convention's error (no audio path).
pub fn set_share_audio(session_id: u32, user_id: u32, audible: bool) -> Result<(), String> {
    #[cfg(feature = "audio-device")]
    {
        let mixer = {
            let sessions = SESSIONS.lock().unwrap();
            let s = sessions
                .get(&session_id)
                .ok_or(format!("no session {session_id}"))?;
            s.mixer.clone()
        };
        {
            let mut set = SHARE_AUDIO_AUDIBLE.lock().unwrap();
            if audible {
                set.insert(user_id);
            } else {
                set.remove(&user_id);
            }
        }
        if !audible {
            // Flush the share's buffered tail — the voice stream is a
            // separate pipeline and is deliberately untouched.
            mixer.lock().unwrap().clear_share(user_id);
        }
        Ok(())
    }
    #[cfg(not(feature = "audio-device"))]
    {
        let _ = (session_id, user_id, audible);
        Err("audio-device feature off".into())
    }
}

/// Show (or stop showing) one sender's screen picture.
///
/// Watching is the UI's decision, and the UI never paints a share nobody
/// opened: an unopened tile is a name and a Watch button. This tells the
/// display bridge exactly that, so a closed share keeps its stream
/// decoded (opening one is then instant) but costs no per-frame pixel
/// conversion, texture upload or raster-thread hand-off — the expensive
/// half of showing a 1080p stream at 30 fps, per share, whether or not
/// anybody is looking.
///
/// Featureless: reports the stub convention's error (no video path).
pub fn set_screen_watched(session_id: u32, user_id: u32, watched: bool) -> Result<(), String> {
    #[cfg(feature = "video")]
    {
        let sessions = SESSIONS.lock().unwrap();
        let s = sessions
            .get(&session_id)
            .ok_or(format!("no session {session_id}"))?;
        if let Some(display) = s.display.as_ref() {
            display.set_watching(user_id, watched);
        }
        Ok(())
    }
    #[cfg(not(feature = "video"))]
    {
        let _ = (session_id, user_id, watched);
        Err("video feature off".into())
    }
}

/// Play this machine's join/leave cue: a short rising pair when somebody
/// enters this voice channel, the same pair falling when they leave. It
/// is mixed into the LOCAL render only — it never reaches the wire, the
/// AEC reference or the share-audio path.
///
/// Silent when no voice stream is open (there is nothing to mix into),
/// and a request made before one opens is dropped rather than replayed.
pub fn play_voice_chime(join: bool) {
    #[cfg(feature = "audio-device")]
    crate::api::voice::play_chime(join);
    #[cfg(not(feature = "audio-device"))]
    let _ = join;
}

// ===== 7g: in-app screen share =====
//
// The Dart side calls start_screen_share after joining a channel; the
// portal dialog opens (Hyprland's picker). Lifecycle events arrive as
// session events (share_started / share_ended). Stop is stop_screen_
// share OR the desktop's sharing indicator — both converge on Ended →
// StopShare AFTER teardown.

/// Toggle hardware (VAAPI) encode preference. Takes effect at the next
/// encoder creation — a running share keeps its backend; stop and
/// re-share to switch. Returns the new state. Featureless: no-op stub
/// (generated glue references this unconditionally).
pub fn set_hw_encode(enable: bool) -> bool {
    #[cfg(feature = "video")]
    {
        parley_core::video::set_hw_encode(enable);
    }
    #[cfg(not(feature = "video"))]
    {
        let _ = enable;
    }
    enable
}

/// Current hardware-encode preference (for the toggle's initial state).
/// Featureless: reports off — the stub convention (video_prepare's 7d
/// error string). Generated glue references this unconditionally.
pub fn hw_encode_enabled() -> bool {
    #[cfg(feature = "video")]
    {
        parley_core::video::hw_encode_enabled()
    }
    #[cfg(not(feature = "video"))]
    {
        false
    }
}

/// Whether this build can hardware-encode at all (the `hw-encode`
/// feature pulls the VAAPI backend). The Dart side hides the HW-encode
/// toggle when this is false — flipping the preference without the
/// backend would silently do nothing.
pub fn hw_encode_supported() -> bool {
    cfg!(feature = "hw-encode")
}

/// Whether this build can share a screen at all (the `screen-capture`
/// feature pulls the portal + PipeWire capture path, which is Linux
/// only). The Dart side hides the share entry points when this is false:
/// on such a build the call could only ever return "screen-capture
/// feature off". Viewing other people's shares is `video`, a different
/// feature — a client without this one still watches.
pub fn screen_share_supported() -> bool {
    cfg!(feature = "screen-capture")
}

/// Start sharing this machine's screen (portal dialog opens). The
/// share runs regardless of channel membership — viewers see it when
/// they're in the same voice channel (FLAG_VIDEO rides the media
/// path). Acceptance: share_started event, then the peer's Screens
/// panel shows this machine.
#[cfg(not(feature = "screen-capture"))]
pub fn start_screen_share(_session_id: u32, _max_height: u32, _max_fps: u32) -> Result<(), String> {
    Err("screen-capture feature off".into())
}

/// Real impl — see the stub above. `max_height` / `max_fps` are the
/// pre-share dialog's caps; 0 means "the capture's own" (never
/// upscaled, never sped up). A cap the running encoder cannot honour
/// is not an error: the share still starts, just larger or faster.
#[cfg(feature = "screen-capture")]
pub fn start_screen_share(session_id: u32, max_height: u32, max_fps: u32) -> Result<(), String> {
    // The own-share card reads the NEW share's shape, not the last one's
    // while the portal picker is still open.
    crate::api::video::clear_share_stats();
    let (event_tx, share_loss, share_viewer_fps, conn, poke_slot, ctrl_slot) = {
        let sessions = SESSIONS.lock().unwrap();
        let s = sessions
            .get(&session_id)
            .ok_or(format!("no session {session_id}"))?;
        if s.share_ctrl.lock().unwrap().is_some() {
            return Err("share already active".to_string());
        }
        (
            s.event_tx.clone(),
            s.share_loss.clone(),
            s.share_viewer_fps.clone(),
            s._conn.clone(),
            s.share_poke.clone(),
            s.share_ctrl.clone(),
        )
    };
    // Spawned (not awaited): the portal dialog blocks for seconds and
    // FRB calls must return fast. The owner task forwards lifecycle
    // events into the session's stream and clears the slots on Ended.
    let (ended_tx, mut ended_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
    let ended_event_tx = event_tx.clone();
    rt().spawn(async move {
        let event_tx = event_tx;
        let (poke_tx, mut poke_rx) =
            tokio::sync::mpsc::unbounded_channel::<parley_client::screen::ShareHandle>();
        let (share_event_tx, mut share_event_rx) =
            tokio::sync::mpsc::unbounded_channel::<parley_client::ShareEvent>();
        let owner = tokio::spawn(async move {
            // 7g share AUDIO (the 6.9 pipeline, wired for the app): the
            // sys capture starts alongside the video (same stop flag)
            // and the mic mux rides the media conn on its own music
            // stream — mic tee routes to it via SHARE_MIC_ROUTE while
            // the mux lives.
            #[cfg(all(feature = "audio-device", feature = "screen-capture"))]
            let (sys_tx, sys_rx) = {
                // 7k: the PipeWire monitor mix enters RAW and leaves the
                // echo filter with our own playback subtracted, then
                // reaches the mux. Two bounded channels: the capture
                // thread never blocks, and a canceller-init failure
                // degrades to a pass-through instead of silence.
                let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(8);
                let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(8);
                parley_client::share_echo::spawn_filter(raw_rx, tx.clone());
                (Some(raw_tx), Some(rx))
            };
            // No share audio in this build: the mux below is cfg'd out
            // with the same predicate, so the receiver is never read.
            #[cfg(not(all(feature = "audio-device", feature = "screen-capture")))]
            let (sys_tx, _sys_rx): (
                Option<std::sync::mpsc::SyncSender<Vec<f32>>>,
                Option<std::sync::mpsc::Receiver<Vec<f32>>>,
            ) = (None, None);
            // The app keeps the mapped-copy capture path: DMA-BUF capture
            // (`--dmabuf` on the CLI) needs the encoder on the capture
            // thread, which the bridge's share pipeline does not do yet.
            let options = parley_client::screen::ShareOptions {
                max_height: (max_height > 0).then_some(max_height),
                max_fps: (max_fps > 0).then(|| f64::from(max_fps)),
            };
            // The sharer's own preview: the tap is always installed and
            // checks the settings switch per frame, so toggling it mid-
            // share costs nothing and needs no restart.
            let preview = crate::api::video::share_tap();
            match parley_client::screen::start(
                conn.clone(),
                share_loss,
                share_viewer_fps,
                share_event_tx,
                sys_tx,
                false,
                options,
                preview,
            )
            .await
            {
                Ok((handle, ctrl)) => {
                    // Spawn the mux NOW (the terminal client's
                    // sequencing): mic + system audio ride one music
                    // stream on the share conn. Mic comes from the tee
                    // via the parked channel (None when voice isn't
                    // started yet — sys-only degradation).
                    #[cfg(all(feature = "audio-device", feature = "screen-capture"))]
                    if let Some(sys_rx) = sys_rx {
                        // 7k: system audio only. Bundling the mic into the
                        // share (the CLI's 6.9 shape) made the whole mux
                        // one stream, so a viewer could not have the
                        // sharer's voice without also getting the share's
                        // sound — and the app gates share sound on
                        // "watching". The mic keeps its own speech
                        // encoder, at speech quality, all share long.
                        let mic_rx = None;
                        let route = share_mic_route().clone();
                        let mux_conn = conn.clone();
                        std::thread::spawn(move || {
                            let returned = parley_client::screen::spawn_share_audio_mux(
                                mic_rx, sys_rx, mux_conn, route,
                            );
                            // Mux exited (share over or error): park
                            // the mic receiver back for reuse.
                            if let Ok(Some(rx)) = returned.join() {
                                *SHARE_MIC_MUX_SLOT.lock().unwrap() = Some(rx);
                            }
                        });
                    }
                    let _ = poke_tx.send(handle);
                    let _ = ctrl_slot.lock().unwrap().insert(ctrl);
                    let _ = event_tx.send(share_event(session_id, "share_started"));
                }
                Err(e) => {
                    let _ = event_tx.send(share_failed_event(session_id, e));
                }
            }
            // Forward the supervisor's Ended (external stop) and exit.
            // Internal stop (stop_screen_share) closes its side of the
            // Ended channel by dropping the ctrl; the supervisor still
            // sends Ended on the shared `events`... which we own, so
            // we just drain until it closes.
            while let Some(ev) = share_event_rx.recv().await {
                if let parley_client::ShareEvent::Ended { external } = ev {
                    let _ = ended_tx.send(external);
                    break;
                }
            }
        });
        // Hand the keyframe handle to the session slot when it lands.
        if let Some(handle) = poke_rx.recv().await {
            *poke_slot.lock().unwrap() = Some(handle);
        }
        // Wait for Ended (external OR our own stop), then signal the
        // server and clean the slots.
        let _ = ended_rx.recv().await;
        let _ = send_command(session_id, Command::StopShare);
        crate::api::video::clear_share_stats();
        {
            let sessions = SESSIONS.lock().unwrap();
            if let Some(s) = sessions.get(&session_id) {
                *s.share_ctrl.lock().unwrap() = None;
                *s.share_poke.lock().unwrap() = None;
            }
        }
        let _ = ended_event_tx.send(share_event(session_id, "share_ended"));
        let _ = owner.await;
    });
    Ok(())
}

/// Stop an active share. Graceful: the supervisor tears down capture
/// first, THEN StopShare rides the control stream (7a ordering rule).
#[cfg(not(feature = "screen-capture"))]
pub fn stop_screen_share(_session_id: u32) -> Result<(), String> {
    Err("screen-capture feature off".into())
}

/// Real impl — see the stub above.
#[cfg(feature = "screen-capture")]
pub fn stop_screen_share(session_id: u32) -> Result<(), String> {
    let sessions = SESSIONS.lock().unwrap();
    let s = sessions
        .get(&session_id)
        .ok_or(format!("no session {session_id}"))?;
    let ctrl = s.share_ctrl.lock().unwrap().take();
    drop(sessions);
    match ctrl {
        Some(c) => {
            c.request_stop();
            Ok(()) // Ended event follows (share_ended + server notice)
        }
        None => Err("no active share".to_string()),
    }
}

/// Poke a fresh keyframe for a newly joined viewer (the terminal
/// client's keyframe-on-join; the app's viewer joins mid-share).
/// Featureless: no-op stub (generated glue references this
/// unconditionally).
#[cfg(not(feature = "screen-capture"))]
pub fn poke_share_keyframe(_session_id: u32) -> Result<(), String> {
    Ok(())
}

/// Real impl — see the stub above.
#[cfg(feature = "screen-capture")]
pub fn poke_share_keyframe(session_id: u32) -> Result<(), String> {
    let sessions = SESSIONS.lock().unwrap();
    let s = sessions
        .get(&session_id)
        .ok_or(format!("no session {session_id}"))?;
    let poke = s.share_poke.lock().unwrap().clone();
    drop(sessions);
    if let Some(h) = poke {
        h.force_keyframe();
    }
    Ok(())
}

#[cfg(feature = "screen-capture")]
fn share_event(session_id: u32, kind: &str) -> SessionEvent {
    let mut e = closed_event(session_id);
    e.kind = kind.to_string();
    e
}

#[cfg(feature = "screen-capture")]
fn share_failed_event(session_id: u32, err: String) -> SessionEvent {
    let mut e = share_event(session_id, "share_failed");
    e.username = err; // reuse the string slot (dev client; documented)
    e
}

/// 7k: expire shares that stopped sending without telling anyone.
///
/// Tests below point `XDG_CONFIG_HOME` at a scratch directory so TOFU pins
/// never touch the developer's real `known_hosts`. The variable is
/// process-global, so those tests take this lock.
#[cfg(test)]
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// `ShareStopped` covers every clean end (stop button, leaving voice,
/// session death). What it cannot cover is a sender whose capture or
/// process died mid-share: the socket may stay up, the room hears
/// nothing, and every viewer keeps a card whose Watch button shows a
/// frozen last frame. Five seconds of silence on a stream that paced at
/// 25–30 fps is dead by any measure; the surface is HIDDEN rather than
/// marked stopped, so a sender that starts sending again (a stall, not
/// a death) re-announces the same texture without a reconnect.
#[cfg(feature = "video")]
fn start_share_watchdog(
    session_id: u32,
    hub: SharedVideoReceiveHub,
    events: UnboundedSender<SessionEvent>,
) {
    const IDLE: Duration = Duration::from_secs(5);
    rt().spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut expired: std::collections::HashSet<u32> = std::collections::HashSet::new();
        loop {
            tick.tick().await;
            let alive = SESSIONS.lock().unwrap().contains_key(&session_id);
            if !alive {
                break; // session gone: nothing left to expire
            }
            let stalled: std::collections::HashSet<u32> = hub.stalled(IDLE).into_iter().collect();
            let senders = hub.senders();
            for id in &senders {
                if !stalled.contains(id) {
                    // Frames are back: the consumer re-announces on the
                    // next one, so just forget that we expired them.
                    expired.remove(id);
                    continue;
                }
                if !expired.insert(*id) {
                    continue; // already announced — no per-second spam
                }
                if let Some(sess) = SESSIONS.lock().unwrap().get(&session_id) {
                    if let Some(d) = sess.display.as_ref() {
                        d.hide(*id);
                    }
                }
                let mut e = closed_event(session_id);
                e.kind = "video_stopped".to_string();
                e.user_id = *id;
                e.surface = Some(VideoSurfaceInfo {
                    user_id: *id,
                    width: 0,
                    height: 0,
                    fps: 0,
                    texture_id: 0,
                });
                println!(
                    "video: user {id} stopped sending frames — share expired after {} s of silence",
                    IDLE.as_secs()
                );
                let _ = events.send(e);
            }
            expired.retain(|id| senders.contains(id));
        }
    });
}

/// 7f (6e): start the viewer-side VideoReport reporter — worst
/// per-sender video gap rate every 5 s while streams are active;
/// feeds the sharer's VideoLadder. Runs for the session lifetime;
/// exits when the writer dies. (The hub returns None with no active
/// streams — nothing is sent then.)
#[cfg(feature = "video")]
fn start_video_reporter(
    session_id: u32,
    hub: SharedVideoReceiveHub,
    cmd_tx: UnboundedSender<Command>,
) {
    rt().spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        // One log line every 10 s: the viewer side's accounting must be
        // visible while a share is bad, not only after it recovers.
        let mut log_every_other = true;
        loop {
            tick.tick().await;
            // The reporter holds a command sender, which would otherwise
            // keep the session writer (and its control stream) alive
            // after a disconnect. Leave with the registry entry.
            if !SESSIONS.lock().unwrap().contains_key(&session_id) {
                break;
            }
            // Expire viewers whose report stopped arriving (they left, or
            // their app died): a departed viewer must not pin this
            // machine's share at its old rate forever. The same 15 s
            // window `to_event` uses, applied even when no new report
            // arrives to trigger it.
            #[cfg(feature = "screen-capture")]
            if let Some(s) = SESSIONS.lock().unwrap().get(&session_id) {
                let mut reports = s.viewer_fps_reports.lock().unwrap();
                reports.retain(|_, (_, when)| {
                    std::time::Instant::now().duration_since(*when) < Duration::from_secs(15)
                });
                let worst = reports
                    .values()
                    .map(|(fps, _)| *fps)
                    .filter(|&fps| fps > 0)
                    .min()
                    .unwrap_or(0);
                s.share_viewer_fps.store(worst, Ordering::Relaxed);
            }
            let Some(worst) = hub.gap_rate_permille() else {
                continue; // no active streams: nothing to report
            };
            // The viewer's own decode rate. 0 means "not measured yet"
            // and the sender keeps its profile pace — never "zero fps".
            let fps = hub.decoded_fps().unwrap_or(0);
            if log_every_other {
                println!(
                    "video: viewer sustaining {fps} fps, gap rate {worst}‰, senders {:?}",
                    hub.senders()
                );
            }
            log_every_other = !log_every_other;
            if cmd_tx
                .send(Command::VideoReport {
                    gap_rate_permille: worst,
                    viewer_fps: fps,
                })
                .is_err()
            {
                break; // writer gone: session over
            }
        }
    });
}

/// The bridge's half of the management contract: the page's tables and
/// its error banner are fed straight from these mappings.
#[cfg(test)]
mod manage_event_tests {
    use super::*;
    use parley_proto::{ManageResult, ManageState, ManagedChannel, ManagedUser};

    #[test]
    fn manage_result_maps_a_full_snapshot() {
        let msg = ServerMessage {
            body: Some(ServerBody::ManageResult(ManageResult {
                ok: true,
                error: String::new(),
                state: Some(ManageState {
                    users: vec![ManagedUser {
                        user_id: 7,
                        username: "alice".to_string(),
                        role: Role::Mod as i32,
                        online: true,
                        channel_id: 3,
                    }],
                    channels: vec![ManagedChannel {
                        channel_id: 3,
                        name: "general".to_string(),
                        mode: ChannelMode::Music as i32,
                        members: 1,
                    }],
                }),
            })),
        };

        let events = to_event(9, &msg);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.session_id, 9);
        assert_eq!(event.kind, "manage");
        assert_eq!(event.manage_error, "");
        let state = event.manage.as_ref().expect("snapshot");
        assert_eq!(state.users[0].username, "alice");
        assert_eq!(state.users[0].role, "mod");
        assert!(state.users[0].online);
        assert_eq!(state.users[0].channel_id, 3);
        assert_eq!(state.channels[0].name, "general");
        assert_eq!(state.channels[0].mode, "music");
        assert_eq!(state.channels[0].members, 1);
    }

    #[test]
    fn manage_error_keeps_the_tables_untouched() {
        let msg = ServerMessage {
            body: Some(ServerBody::ManageResult(ManageResult {
                ok: false,
                error: "only an admin can change permissions".to_string(),
                state: None,
            })),
        };

        let events = to_event(9, &msg);
        let event = &events[0];
        assert_eq!(event.kind, "manage");
        assert!(event.manage.is_none());
        assert_eq!(event.manage_error, "only an admin can change permissions");
    }
}

/// Regression: a manual disconnect must end the server-side session.
///
/// The bug this guards: `disconnect` used to drop the registry entry and
/// rely on the writer task noticing. The video reporter holds a command
/// sender for the session's lifetime, so the writer never noticed, the
/// control stream stayed open, and the server kept the account listed as
/// online — the next login (as another user) saw the previous user still
/// present. The fix is an explicit `conn.close`; this test asserts the
/// observable outcome against a real server.
#[cfg(test)]
mod disconnect_tests {
    use super::*;
    use parley_core::session as wire;
    use parley_proto::{
        AuthRequest, ClientBody, ClientMessage, Hello, HelloAck, PresenceRequest, ServerBody,
    };
    use parley_server::{Config, Server};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("parley-bridge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Ask the server for the online set over a throwaway raw connection
    /// (the CLI's wire, same pinned fingerprint as the bridge).
    async fn online_users(addr: std::net::SocketAddr, pin: String) -> Vec<String> {
        let (endpoint, _seen) = parley_client::cert::build_endpoint(Some(pin));
        let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();
        let (mut tx, mut rx) = conn.open_bi().await.unwrap();
        wire::write_message(
            &mut tx,
            &Hello {
                protocol_version: PROTOCOL_VERSION,
                caps: 0,
            },
        )
        .await
        .unwrap();
        let _: HelloAck = wire::read_message(&mut rx).await.unwrap();
        wire::write_message(
            &mut tx,
            &AuthRequest {
                username: "bob".into(),
                password: "pass_b".into(),
                session_token: String::new(),
            },
        )
        .await
        .unwrap();
        let _: AuthOk = wire::read_message(&mut rx).await.unwrap();
        wire::write_message(
            &mut tx,
            &ClientMessage {
                body: Some(ClientBody::PresenceRequest(PresenceRequest {})),
            },
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "no presence list within 2 s");
            let msg: ServerMessage = tokio::time::timeout(remaining, wire::read_message(&mut rx))
                .await
                .expect("presence response timed out")
                .unwrap();
            if let Some(ServerBody::PresenceList(list)) = msg.body {
                let names = list.users.into_iter().map(|u| u.username).collect();
                conn.close(0u32.into(), b"done");
                return names;
            }
        }
    }

    #[test]
    fn disconnect_ends_the_server_side_session() {
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("disconnect");
        // TOFU pinning writes here; keep the real user config untouched.
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        let config = Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: dir.join("data"),
            video: Default::default(),
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();
        {
            let db = parley_server::db::Db::open(&config.data_dir.join("parley.db")).unwrap();
            db.create_user("alice", "pass_a", false).unwrap();
            db.create_user("bob", "pass_b", false).unwrap();
        }
        // quinn's endpoint needs a tokio context to bind.
        let server = rt().block_on(async { Server::bind(&config).unwrap() });
        let addr = server.local_addr();
        let pin = server.fingerprint().to_string();
        rt().spawn(server.run());

        let alice = connect(
            format!("127.0.0.1:{}", addr.port()),
            "alice".into(),
            "pass_a".into(),
        )
        .expect("bridge connect");

        let before = rt().block_on(online_users(addr, pin.clone()));
        assert!(
            before.iter().any(|u| u == "alice"),
            "alice should be online: {before:?}"
        );

        disconnect(alice.session_id).expect("bridge disconnect");

        // The close is asynchronous on the wire; the server drops the
        // session as soon as it observes it. Poll briefly.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let after = rt().block_on(online_users(addr, pin.clone()));
            if !after.iter().any(|u| u == "alice") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "alice still online 5 s after disconnect: {after:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Auto-reconnect, end to end: the token a password login returns is the
/// credential the next launch uses — no password, same account, and an
/// unknown token is refused exactly like a wrong password.
#[cfg(test)]
mod resume_tests {
    use super::*;
    use parley_server::{Config, Server};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("parley-bridge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_saved_token_reconnects_without_the_password() {
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("resume");
        // The remembered login and the TOFU pin both live under this.
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        let config = Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: dir.join("data"),
            video: Default::default(),
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();
        {
            let db = parley_server::db::Db::open(&config.data_dir.join("parley.db")).unwrap();
            db.create_user("alice", "pass_a", false).unwrap();
        }
        let server = rt().block_on(async { Server::bind(&config).unwrap() });
        let addr = server.local_addr();
        rt().spawn(server.run());
        let address = format!("127.0.0.1:{}", addr.port());

        let first =
            connect(address.clone(), "alice".into(), "pass_a".into()).expect("password login");
        assert!(!first.session_token.is_empty(), "auth must issue a token");
        crate::api::login::save(&address, "alice", &first.session_token).expect("remember login");
        disconnect(first.session_id).unwrap();

        let saved = crate::api::login::load()
            .expect("read saved login")
            .expect("a saved login after a successful one");
        assert_eq!(saved.server, address);
        assert_eq!(saved.user, "alice");

        let resumed = connect_with_token(saved.server, saved.user, saved.token)
            .expect("token reconnect must work without the password");
        assert_eq!(resumed.user_id, first.user_id, "same account");
        disconnect(resumed.session_id).unwrap();

        // A token the server never issued is refused like a bad password.
        let err = connect_with_token(address, "alice".into(), "not-a-token".into())
            .expect_err("an unknown token must be refused");
        assert!(err.contains("auth denied"), "{err}");

        // Signing out deletes the file; the next launch shows the form.
        crate::api::login::forget().unwrap();
        assert!(crate::api::login::load().unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The user-facing TOFU recovery path, end to end against a real server:
/// a pin from an older identity must refuse the login with both digests,
/// leave the pin alone, and only replace it after `trust_server`.
#[cfg(test)]
mod tofu_tests {
    use super::*;
    use parley_server::{Config, Server};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("parley-bridge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn changed_certificate_is_reported_and_explicitly_trusted() {
        const OLD: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("tofu");
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        let config = Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: dir.join("data"),
            video: Default::default(),
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();
        {
            let db = parley_server::db::Db::open(&config.data_dir.join("parley.db")).unwrap();
            db.create_user("alice", "pass_a", false).unwrap();
        }
        let server = rt().block_on(async { Server::bind(&config).unwrap() });
        let addr = server.local_addr();
        let real = server.fingerprint().to_string();
        rt().spawn(server.run());

        let address = format!("127.0.0.1:{}", addr.port());
        let key = parley_client::endpoint::pin_key(&address).unwrap();

        // Exactly what a redeploy leaves behind: a pin from the old identity.
        let (_, path) = cert::load_pin(&key).unwrap();
        cert::save_pin(&path, &key, OLD).unwrap();

        let err = connect(address.clone(), "alice".into(), "pass_a".into())
            .expect_err("a changed certificate must refuse the login");
        assert!(err.contains("fingerprint mismatch for"), "{err}");
        assert!(err.contains(OLD), "the saved pin is shown: {err}");
        assert!(
            err.contains(&real),
            "the presented certificate is shown: {err}"
        );
        assert_eq!(
            cert::load_pin(&key).unwrap().0.as_deref(),
            Some(OLD),
            "a refused attempt must not re-pin"
        );

        // The probe the app runs to fill its prompt.
        let probe = probe_server(address.clone()).unwrap();
        assert!(!probe.matches);
        assert_eq!(probe.pinned.as_deref(), Some(OLD));
        assert_eq!(probe.seen, real);

        // Accepting the prompt replaces the pin, and the login works again.
        assert_eq!(trust_server(address.clone()).unwrap(), real);
        assert!(probe_server(address.clone()).unwrap().matches);
        let session = connect(address, "alice".into(), "pass_a".into())
            .expect("login after trusting the new certificate");
        disconnect(session.session_id).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
