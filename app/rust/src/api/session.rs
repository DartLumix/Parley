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
use std::net::SocketAddr;
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
#[cfg(feature = "audio-device")]
use parley_proto::ReceiverReport;
#[cfg(feature = "video")]
use parley_proto::VideoReport;
use parley_proto::{
    AuthDenied, AuthOk, AuthRequest, ChannelMode, ClientBody, ClientMessage, FetchHistory,
    Heartbeat, Hello, HelloAck, JoinVoice, LeaveVoice, ListChannels, PresenceRequest, SendChat,
    ServerBody, ServerMessage, PROTOCOL_VERSION,
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
    },
    /// 7g: after the local share's supervisor reported Ended, tell the
    /// server (never before Ended — no datagram may follow the notice).
    #[cfg(feature = "screen-capture")]
    StopShare,
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
            #[cfg(feature = "audio-device")]
            Command::ReceiverReport { gap_rate_permille } => {
                ClientBody::ReceiverReport(ReceiverReport {
                    gap_rate_permille,
                    reporter_id: 0, // server stamps the authenticated id
                })
            }
            #[cfg(feature = "video")]
            Command::VideoReport { gap_rate_permille } => {
                ClientBody::VideoReport(VideoReport {
                    gap_rate_permille,
                    reporter_id: 0, // server stamps the authenticated id
                })
            }
            #[cfg(feature = "screen-capture")]
            Command::StopShare => ClientBody::StopShare(parley_proto::StopShare {}),
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
}

/// 7f: one remote share's display surface. `texture_id` feeds a plain
/// Flutter `Texture` widget (pixels come from the Rust side); width /
/// height are the decoded stream's dimensions.
#[derive(Debug, Clone)]
pub struct VideoSurfaceInfo {
    pub user_id: u32,
    pub width: u64,
    pub height: u64,
    pub texture_id: i64,
}

/// Everything the server can tell the UI, as one flat event struct.
/// `kind` is one of: "channel_list", "presence", "chat", "history",
/// "voice_state", "voice_states", "session_closed".
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
    /// voice_states (the full snapshot)
    pub states: Vec<VoiceEntry>,
    /// 7f: video_started / video_stopped (the surface)
    pub surface: Option<VideoSurfaceInfo>,
}

/// Connected-session snapshot returned at connect time.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: u32,
    pub user_id: u32,
    pub session_token: String,
    pub server: String,
    pub fingerprint: String,
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
        states: Vec::new(),
        surface: None,
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
        states: Vec::new(),
        surface: None,
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
                    texture_id: 0,
                });
                vec![e]
            }
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
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u32, Vec<u8>)>();
    rt().spawn(async move {
        while let Some((sender_id, payload)) = rx.recv().await {
            hub.push_datagram(sender_id, &payload);
        }
    });
    VIDEO_FEEDS.lock().unwrap().insert(session_id, tx);
}

/// 7f: one session's video wire (reader → hub task).
#[cfg(feature = "video")]
type VideoFeed = tokio::sync::mpsc::UnboundedSender<(u32, Vec<u8>)>;

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
    let (mixer, conn, my_id, loss_report, cmd_tx, video_tx) = {
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
    rt().block_on(connect_async(server, user, password))
}

async fn connect_async(
    server: String,
    user: String,
    password: String,
) -> Result<SessionInfo, String> {
    let addr: SocketAddr = server
        .parse()
        .map_err(|_| format!("server must be IP:port (got \"{server}\")"))?;
    let server_name = server
        .split_once(':')
        .map(|(h, _)| h.to_string())
        .unwrap_or_else(|| server.clone());

    // TOFU + endpoint (cert.rs — same path as the terminal client).
    let (pin, hosts_path) = cert::load_pin(&server)?;
    let (endpoint, seen) = cert::build_endpoint(pin.clone());
    let conn = endpoint
        .connect(addr, &server_name)
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| format!("connect: {e}"))?;

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

    session::write_message(
        &mut tx,
        &AuthRequest {
            username: user,
            password,
            session_token: String::new(),
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
        cert::save_pin(&hosts_path, &server, &fingerprint)?;
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
    let session_hub: SharedVideoReceiveHub = parley_core::video_receive::VideoReceiveHub::shared();
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
    start_video_reporter(session_hub.clone(), cmd_tx.clone());

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
            #[cfg(feature = "screen-capture")]
            event_tx: event_tx.clone(),
            #[cfg(feature = "screen-capture")]
            share_ctrl: Arc::new(Mutex::new(None)),
            #[cfg(feature = "screen-capture")]
            share_poke: Arc::new(Mutex::new(None)),
            #[cfg(feature = "screen-capture")]
            share_loss: Arc::new(AtomicU32::new(0)),
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

    Ok(SessionInfo {
        session_id,
        user_id: ok.user_id,
        session_token: ok.session_token,
        server,
        fingerprint,
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
/// the registry entry drops — closing the streams (and the endpoint's
/// socket). The server ends the session on control-stream close (§5
/// semantics). Writer/reader tasks error out and exit.
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
    match SESSIONS.lock().unwrap().remove(&session_id) {
        Some(_) => Ok(()),
        None => Err(format!("no session {session_id}")),
    }
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

/// Start sharing this machine's screen (portal dialog opens). The
/// share runs regardless of channel membership — viewers see it when
/// they're in the same voice channel (FLAG_VIDEO rides the media
/// path). Acceptance: share_started event, then the peer's Screens
/// panel shows this machine.
#[cfg(not(feature = "screen-capture"))]
pub fn start_screen_share(_session_id: u32) -> Result<(), String> {
    Err("screen-capture feature off".into())
}

/// Real impl — see the stub above.
#[cfg(feature = "screen-capture")]
pub fn start_screen_share(session_id: u32) -> Result<(), String> {
    let (event_tx, share_loss, conn, poke_slot, ctrl_slot) = {
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
                let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(8);
                (Some(tx), Some(rx))
            };
            #[cfg(not(all(feature = "audio-device", feature = "screen-capture")))]
            let (sys_tx, sys_rx): (
                Option<std::sync::mpsc::SyncSender<Vec<f32>>>,
                Option<std::sync::mpsc::Receiver<Vec<f32>>>,
            ) = (None, None);
            match parley_client::screen::start(conn.clone(), share_loss, share_event_tx, sys_tx)
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
                        let mic_rx = SHARE_MIC_MUX_SLOT.lock().unwrap().take();
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

/// 7f (6e): start the viewer-side VideoReport reporter — worst
/// per-sender video gap rate every 5 s while streams are active;
/// feeds the sharer's VideoLadder. Runs for the session lifetime;
/// exits when the writer dies. (The hub returns None with no active
/// streams — nothing is sent then.)
#[cfg(feature = "video")]
fn start_video_reporter(hub: SharedVideoReceiveHub, cmd_tx: UnboundedSender<Command>) {
    rt().spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let Some(worst) = hub.gap_rate_permille() else {
                continue; // no active streams: nothing to report
            };
            if cmd_tx
                .send(Command::VideoReport {
                    gap_rate_permille: worst,
                })
                .is_err()
            {
                break; // writer gone: session over
            }
        }
    });
}
