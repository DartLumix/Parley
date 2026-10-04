//! Step 1: connection/session state. Connect + auth + TOFU + heartbeat,
//! surfaced to Flutter as plain types via a handle registry (session_id).
//! Deliberately no flutter_rust_bridge-specific attributes — plain
//! strings/ints/structs only, everything else stays refactorable.
//!
//! Session semantics match the terminal client: heartbeats at the
//! server-advertised interval (default 5 s; the server reaps at 10 s
//! QUIC idle), session lifetime = control-stream lifetime, and the
//! quinn Endpoint is stored alongside the connection (dropping it
//! closes the UDP socket and kills the session — the terminal client
//! keeps it alive implicitly in main's locals).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prost::Message as _;
use tokio::runtime::Runtime;

use parley_client::cert;
use parley_core::session;
use parley_proto::{
    AuthDenied, AuthOk, AuthRequest, ClientBody, ClientMessage, Heartbeat, Hello, HelloAck,
    ServerMessage, PROTOCOL_VERSION,
};

/// One tokio runtime for the whole bridge (FRB calls run on its own
/// worker threads; block_on there never touches the UI isolate).
static RT: OnceLock<Runtime> = OnceLock::new();
fn rt() -> &'static Runtime {
    RT.get_or_init(|| Runtime::new().expect("tokio runtime"))
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

struct LiveSession {
    /// Kept alive: dropping it closes the UDP socket (see module docs).
    _endpoint: quinn::Endpoint,
    _conn: quinn::Connection,
    /// Flipped false by the drain reader when the control stream ends.
    alive: Arc<AtomicBool>,
}

static SESSIONS: LazyLock<Mutex<HashMap<u32, LiveSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Connected-session snapshot returned to Dart.
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

/// Read one length-prefixed frame raw — mirrors the terminal client's
/// auth-phase helper (auth messages are bare, pre-oneof, so the auth
/// reader must try AuthOk OR AuthDenied).
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
        return Err("unexpected message during auth".to_string());
    };

    // TOFU: pin on first successful auth (same rule as the terminal
    // client — only pin a server we fully authenticated against).
    let fingerprint = seen
        .lock()
        .unwrap()
        .clone()
        .ok_or("server presented no certificate")?;
    if pin.is_none() {
        cert::save_pin(&hosts_path, &server, &fingerprint)?;
    }

    // Heartbeat writer: owns tx. Interval from the server's HelloAck
    // (0 → 5 s default); the server reaps silent clients at 10 s.
    let hb_ms = if ack.heartbeat_ms == 0 {
        5000
    } else {
        ack.heartbeat_ms as u64
    };
    tokio::spawn(async move {
        let mut tx = tx;
        let mut tick = tokio::time::interval(Duration::from_millis(hb_ms));
        loop {
            tick.tick().await;
            let msg = ClientMessage {
                body: Some(ClientBody::Heartbeat(Heartbeat {
                    client_time_ms: now_ms(),
                })),
            };
            if session::write_message(&mut tx, &msg).await.is_err() {
                break; // stream gone: session over
            }
        }
    });

    // Drain reader: owns rx. Unread control data would eventually stall
    // the server's broadcasts (stream flow control); step 2 replaces
    // the silent drain with real event surfacing (chat/presence/voice).
    let alive = Arc::new(AtomicBool::new(true));
    let reader_alive = alive.clone();
    tokio::spawn(async move {
        let mut rx = rx;
        loop {
            match session::read_message::<ServerMessage>(&mut rx).await {
                Ok(_msg) => {}
                Err(_) => {
                    reader_alive.store(false, Ordering::Relaxed);
                    break;
                }
            }
        }
    });

    let session_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    SESSIONS.lock().unwrap().insert(
        session_id,
        LiveSession {
            _endpoint: endpoint,
            _conn: conn,
            alive,
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

/// Liveness: the drain reader's flag (control stream still open).
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
