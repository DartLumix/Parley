//! Read-only HTTP presence snapshot for external dashboards.
//!
//! The client protocol is QUIC over UDP (port 4443), which no HTTP client
//! can speak, and scraping container logs is not a contract. This module
//! adds a second, deliberately tiny listener: `GET /api/presence` renders
//! the live session and voice state as JSON.
//!
//! Read-only by construction: the handler has no code path that mutates
//! the registry, the voice manager or the database, so polling it cannot
//! change what it reports. It binds loopback by default; the container
//! binds its own interface and compose publishes that port to 127.0.0.1
//! on the host only. `PARLEY_PRESENCE_TOKEN` optionally adds a bearer gate
//! for deployments that choose to expose it further.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::db::DbHandle;
use crate::error::Error;
use crate::session::SessionRegistry;
use crate::voice::VoiceManager;

/// Cap on the request head we are willing to buffer. A dashboard sends a
/// handful of headers; anything larger is not a request we serve.
const MAX_HEAD: usize = 8 * 1024;
/// A client that connects and says nothing must not pin a task forever.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

const PATH: &str = "/api/presence";

/// The bound listener, before a runtime picks it up. Binding happens in
/// `Server::bind` so a taken port fails startup instead of vanishing
/// inside a background task.
pub struct PresenceServer {
    listener: std::net::TcpListener,
    token: Option<String>,
}

impl PresenceServer {
    /// Bind `listen` (e.g. `127.0.0.1:4444`). An empty `token` disables
    /// the bearer check; the caller reads it from the environment.
    pub fn bind(listen: &str, token: Option<String>) -> Result<Self, Error> {
        let listener = std::net::TcpListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener, token })
    }

    /// The port actually bound (`:0` resolves to a real one).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until the listener fails. Callers spawn this next to the
    /// QUIC acceptor; it owns no shutdown path of its own.
    pub async fn serve(
        self,
        db: DbHandle,
        registry: Arc<SessionRegistry>,
        voice: Arc<VoiceManager>,
    ) -> Result<(), Error> {
        let listener = TcpListener::from_std(self.listener)?;
        let state = Arc::new(State {
            db,
            registry,
            voice,
            token: self.token,
        });
        loop {
            let (stream, _peer) = listener.accept().await?;
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, state).await {
                    // A dashboard hanging up mid-request is routine; this
                    // only fires for protocol errors or timeouts.
                    debug!(error = %e, "presence request failed");
                }
            });
        }
    }
}

struct State {
    db: DbHandle,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
    token: Option<String>,
}

struct RequestHead {
    method: String,
    path: String,
    authorization: Option<String>,
}

async fn handle(mut stream: TcpStream, state: Arc<State>) -> io::Result<()> {
    let Some(head) = read_head(&mut stream).await? else {
        return Ok(());
    };

    if head.path != PATH {
        return respond(
            &mut stream,
            404,
            "Not Found",
            b"{\"error\":\"not found\"}",
            &[],
        )
        .await;
    }
    if head.method != "GET" {
        return respond(
            &mut stream,
            405,
            "Method Not Allowed",
            b"{\"error\":\"method not allowed\"}",
            &[("Allow", "GET")],
        )
        .await;
    }
    if let Some(expected) = &state.token {
        let authorized = head
            .authorization
            .as_deref()
            .and_then(bearer)
            .is_some_and(|presented| constant_time_eq(presented, expected));
        if !authorized {
            return respond(
                &mut stream,
                401,
                "Unauthorized",
                b"{\"error\":\"unauthorized\"}",
                &[("WWW-Authenticate", "Bearer")],
            )
            .await;
        }
    }

    match snapshot(&state.db, &state.registry, &state.voice).await {
        Ok(body) => respond(&mut stream, 200, "OK", body.as_bytes(), &[]).await,
        Err(e) => {
            warn!(error = %e, "presence snapshot failed");
            respond(
                &mut stream,
                503,
                "Service Unavailable",
                b"{\"error\":\"snapshot unavailable\"}",
                &[],
            )
            .await
        }
    }
}

/// Entity keys are prefixed so a dashboard cannot confuse a numeric user
/// id with a channel id; both are stable SQLite primary keys.
fn user_key(id: u32) -> String {
    format!("u-{id}")
}

fn channel_key(id: u64) -> String {
    format!("ch-{id}")
}

/// `Bearer <token>`, scheme compared case-insensitively as HTTP requires.
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Token comparison that does not return early on the first differing
/// byte. The endpoint is loopback-only by default; this costs nothing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

async fn read_head(stream: &mut TcpStream) -> io::Result<Option<RequestHead>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = match timeout(IO_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => return Ok(None),
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "request head timed out",
                ));
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return parse_head(&buf[..end]).map(Some);
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
    }
}

fn parse_head(head: &[u8]) -> io::Result<RequestHead> {
    let text = std::str::from_utf8(head)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed request line",
        ));
    }
    let mut authorization = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("authorization")
        {
            authorization = Some(value.trim().to_string());
        }
    }
    Ok(RequestHead {
        method: method.to_string(),
        path: target.split('?').next().unwrap_or_default().to_string(),
        authorization,
    })
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

#[derive(Serialize)]
struct Snapshot {
    server: ServerMeta,
    channels: Vec<ChannelEntry>,
    users: Vec<UserEntry>,
}

#[derive(Serialize)]
struct ServerMeta {
    name: &'static str,
    user_count: usize,
}

#[derive(Serialize)]
struct ChannelEntry {
    id: String,
    name: String,
    parent_id: Option<String>,
    position: usize,
    #[serde(rename = "type")]
    kind: &'static str,
    user_limit: u32,
    /// Parley extension: a channel runs the speech or the music profile.
    mode: &'static str,
}

#[derive(Serialize)]
struct UserEntry {
    id: String,
    display_name: String,
    username: String,
    avatar_url: Option<String>,
    channel_id: Option<String>,
    muted: bool,
    deafened: bool,
    afk: bool,
    afk_message: Option<String>,
    screen_sharing: bool,
    is_bot: bool,
    /// Parley extension: `user`, `mod` or `admin`.
    role: &'static str,
}

/// One online user, already flattened out of the two managers. Split from
/// [`snapshot`] so the JSON contract is testable without a live session.
pub(crate) struct RosterEntry {
    pub id: u32,
    pub username: String,
    pub role: &'static str,
    pub channel_id: Option<u64>,
    pub muted: bool,
    pub status: String,
    pub sharing: bool,
}

/// Render the contract. Users are ordered by id so repeated polls of an
/// unchanged server produce identical bytes (dashboards may diff them).
fn render(
    channels: &[(u64, String, bool)],
    mut roster: Vec<RosterEntry>,
) -> Result<String, serde_json::Error> {
    roster.sort_by_key(|user| user.id);
    let snapshot = Snapshot {
        server: ServerMeta {
            name: "Parley",
            user_count: roster.len(),
        },
        channels: channels
            .iter()
            .enumerate()
            .map(|(position, (id, name, music))| ChannelEntry {
                id: channel_key(*id),
                name: name.clone(),
                // Parley has no channel tree: one flat list, no categories.
                parent_id: None,
                position,
                kind: "voice",
                user_limit: 0,
                mode: if *music { "music" } else { "speech" },
            })
            .collect(),
        users: roster
            .into_iter()
            .map(|user| {
                let afk = !user.status.is_empty();
                UserEntry {
                    id: user_key(user.id),
                    display_name: user.username.clone(),
                    username: user.username,
                    avatar_url: None,
                    channel_id: user.channel_id.map(channel_key),
                    muted: user.muted,
                    deafened: false,
                    afk,
                    afk_message: afk.then_some(user.status),
                    screen_sharing: user.sharing,
                    is_bot: false,
                    role: user.role,
                }
            })
            .collect(),
    };
    serde_json::to_string(&snapshot)
}

/// Point-in-time snapshot: the registry and the voice manager are locked
/// separately, so a join landing between the two reads can make this one
/// poll show a user who has no channel yet. The next poll (5 s) settles.
pub(crate) async fn snapshot(
    db: &DbHandle,
    registry: &SessionRegistry,
    voice: &VoiceManager,
) -> Result<String, Error> {
    let channels = db.list_channels().await?;
    let mut live: HashMap<u32, (u64, bool, String)> = voice
        .snapshot()
        .into_iter()
        .map(|(id, channel, muted, status)| (id, (channel, muted, status)))
        .collect();
    let sharing: HashSet<u32> = voice.sharing_snapshot();
    let roster = registry
        .roster()
        .into_iter()
        .map(|user| {
            let (channel_id, muted, status) = match live.remove(&user.id) {
                Some((channel, muted, status)) => (Some(channel), muted, status),
                // Online but not in a voice channel.
                None => (None, false, String::new()),
            };
            RosterEntry {
                id: user.id,
                username: user.username,
                role: user.role.as_str(),
                channel_id,
                muted,
                status,
                sharing: sharing.contains(&user.id),
            }
        })
        .collect();
    render(&channels, roster).map_err(|e| Error::Internal(format!("presence json: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::path::PathBuf;

    fn test_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "parley-presence-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn render_matches_the_dashboard_contract() {
        let channels = vec![
            (1, "Lobby".to_string(), false),
            (2, "Music".to_string(), true),
        ];
        let roster = vec![
            RosterEntry {
                id: 7,
                username: "bob".to_string(),
                role: "user",
                channel_id: Some(1),
                muted: true,
                status: String::new(),
                sharing: true,
            },
            RosterEntry {
                id: 3,
                username: "alice".to_string(),
                role: "admin",
                channel_id: None,
                muted: false,
                status: "Coming back later".to_string(),
                sharing: false,
            },
        ];

        let json = render(&channels, roster).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["server"]["name"], "Parley");
        assert_eq!(value["server"]["user_count"], 2);

        assert_eq!(value["channels"][0]["id"], "ch-1");
        assert_eq!(value["channels"][0]["name"], "Lobby");
        assert_eq!(value["channels"][0]["parent_id"], serde_json::Value::Null);
        assert_eq!(value["channels"][0]["position"], 0);
        assert_eq!(value["channels"][0]["type"], "voice");
        assert_eq!(value["channels"][0]["user_limit"], 0);
        assert_eq!(value["channels"][0]["mode"], "speech");
        assert_eq!(value["channels"][1]["mode"], "music");

        // Ordered by user id, not by the order the manager produced.
        assert_eq!(value["users"][0]["id"], "u-3");
        assert_eq!(value["users"][0]["username"], "alice");
        assert_eq!(value["users"][0]["display_name"], "alice");
        assert_eq!(value["users"][0]["avatar_url"], serde_json::Value::Null);
        assert_eq!(value["users"][0]["channel_id"], serde_json::Value::Null);
        assert_eq!(value["users"][0]["afk"], true);
        assert_eq!(value["users"][0]["afk_message"], "Coming back later");
        assert_eq!(value["users"][0]["role"], "admin");
        assert_eq!(value["users"][0]["deafened"], false);
        assert_eq!(value["users"][0]["is_bot"], false);

        assert_eq!(value["users"][1]["channel_id"], "ch-1");
        assert_eq!(value["users"][1]["muted"], true);
        assert_eq!(value["users"][1]["afk"], false);
        assert_eq!(value["users"][1]["afk_message"], serde_json::Value::Null);
        assert_eq!(value["users"][1]["screen_sharing"], true);
    }

    #[tokio::test]
    async fn http_surface_serves_get_and_rejects_the_rest() {
        let dir = test_dir("http");
        std::fs::create_dir_all(&dir).unwrap();
        let db = DbHandle::spawn(Db::open(&dir.join("parley.db")).unwrap());
        let registry = Arc::new(SessionRegistry::new());
        let voice = Arc::new(VoiceManager::new());

        let server = PresenceServer::bind("127.0.0.1:0", Some("sekret".to_string())).unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(server.serve(db.clone(), registry.clone(), voice.clone()));

        // No token: rejected.
        let (status, body) = request(addr, "/api/presence", &[]).await;
        assert_eq!(status, 401);
        assert_eq!(body["error"], "unauthorized");

        // Wrong token: rejected.
        let (status, _) = request(addr, "/api/presence", &[("Authorization", "Bearer nope")]).await;
        assert_eq!(status, 401);

        // Right token: the seeded channel and an empty roster.
        let (status, body) =
            request(addr, "/api/presence", &[("Authorization", "Bearer sekret")]).await;
        assert_eq!(status, 200);
        assert_eq!(body["server"]["user_count"], 0);
        assert_eq!(body["users"].as_array().unwrap().len(), 0);
        assert_eq!(body["channels"][0]["name"], "general");

        // A query string is ignored, not routed to a second handler.
        let (status, _) = request(
            addr,
            "/api/presence?t=1",
            &[("Authorization", "Bearer sekret")],
        )
        .await;
        assert_eq!(status, 200);

        // Anything else is not a route.
        let (status, _) = request(addr, "/", &[("Authorization", "Bearer sekret")]).await;
        assert_eq!(status, 404);

        // Read-only: only GET is served.
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"POST /api/presence HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        assert!(raw.starts_with("HTTP/1.1 405"), "{raw}");
        assert!(raw.contains("Allow: GET"));

        // Reading changed nothing.
        assert!(registry.roster().is_empty());
        assert!(voice.snapshot().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn request(
        addr: SocketAddr,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (u16, serde_json::Value) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut raw = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
        for (name, value) in headers {
            raw.push_str(&format!("{name}: {value}\r\n"));
        }
        raw.push_str("\r\n");
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let (head, body) = response
            .split_once("\r\n\r\n")
            .expect("a complete response");
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .expect("a status code")
            .parse()
            .expect("a numeric status");
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: application/json"),
            "responses are JSON: {head}"
        );
        let body = serde_json::from_str(body).expect("a JSON body");
        (status, body)
    }
}
