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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prost::Message as _;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedSender;

use parley_client::cert;
use parley_core::session;
use parley_proto::{
    AuthDenied, AuthOk, AuthRequest, ChannelMode, ClientBody, ClientMessage, FetchHistory,
    Heartbeat, Hello, HelloAck, JoinVoice, LeaveVoice, ListChannels, PresenceRequest, SendChat,
    ServerBody, ServerMessage, PROTOCOL_VERSION,
};

use crate::frb_generated::StreamSink;

/// One tokio runtime for the whole bridge (FRB calls run on their own
/// worker threads; block_on there never touches the UI isolate).
static RT: OnceLock<Runtime> = OnceLock::new();
fn rt() -> &'static Runtime {
    RT.get_or_init(|| Runtime::new().expect("tokio runtime"))
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// Commands the session writer accepts (the heartbeat it generates
/// itself).
#[derive(Debug)]
enum Command {
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
    }
}

/// Map one ServerMessage to a SessionEvent (with empty defaults for
/// the fields this kind doesn't use).
fn to_event(session_id: u32, msg: &ServerMessage) -> Option<SessionEvent> {
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
    };
    match msg.body.as_ref()? {
        ServerBody::ChannelList(list) => {
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
            Some(e)
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
            Some(e)
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
            Some(e)
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
            Some(e)
        }
        ServerBody::VoiceState(v) => {
            let mut e = ev("voice_state");
            e.user_id = v.user_id;
            e.channel_id = v.channel_id;
            e.joined = v.joined;
            Some(e)
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
            Some(e)
        }
        // HeartbeatAck / ReceiverReport / ShareStopped / VideoReport:
        // later steps surface these (share lifecycle, loss ladders).
        _ => None,
    }
}

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
                        if let Some(ev) = to_event(session_id, &msg) {
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
        let event_tx = event_tx;
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

    SESSIONS.lock().unwrap().insert(
        session_id,
        LiveSession {
            _endpoint: endpoint,
            _conn: conn,
            cmd_tx,
            alive,
            sinks,
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

/// Disconnect: dropping the registry entry closes the streams (and the
/// endpoint's socket) — the server ends the session on control-stream
/// close (§5 semantics). Writer/reader tasks error out and exit.
pub fn disconnect(session_id: u32) -> Result<(), String> {
    match SESSIONS.lock().unwrap().remove(&session_id) {
        Some(_) => Ok(()),
        None => Err(format!("no session {session_id}")),
    }
}

/// Request the channel listing (response: "channel_list" event).
pub fn list_channels(session_id: u32) -> Result<(), String> {
    send_command(session_id, Command::ListChannels)
}

/// Join a voice channel (self-confirmation: "voice_state" event).
pub fn join_voice(session_id: u32, channel_id: u64) -> Result<(), String> {
    send_command(session_id, Command::JoinVoice { channel_id })
}

/// Leave the current voice channel.
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
