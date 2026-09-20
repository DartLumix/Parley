//! The QUIC listener: bind, accept, dispatch, session loop.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use quinn::crypto::rustls::QuicServerConfig;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::Config;
use crate::db::Db;
use crate::error::Error;
use crate::session::{ClientHandle, SessionRegistry};
use crate::tls;
use crate::voice::VoiceManager;
use parley_core::session;
use parley_proto::{
    AuthDenied, AuthOk, AuthRequest, Channel as PbChannel, ChannelList, ClientBody, ClientMessage,
    HeartbeatAck, Hello, HelloAck, PROTOCOL_VERSION, PresenceList, PresenceUser, ReceiverReport,
    ServerBody, ServerMessage, ShareStopped, VoiceState as PbVoiceState, VoiceStateList,
};

pub struct Server {
    endpoint: quinn::Endpoint,
    fingerprint: String,
    db: Arc<Mutex<Db>>,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
}

impl Server {
    pub fn bind(config: &Config) -> Result<Self, Error> {
        tls::init_provider();

        let db = Db::open(&config.data_dir.join("parley.db"))?;
        db.bootstrap_admin()?;

        let (identity, _new) = tls::load_or_generate(&config.data_dir)?;
        let fingerprint = tls::fingerprint(&identity.cert);

        let addr: SocketAddr = config.listen.parse()?;

        let mut rustls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![identity.cert], identity.key)?;
        rustls_config.alpn_protocols = vec![tls::alpn()];

        let quinn_config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(rustls_config)
                .map_err(|e| Error::QuinnTls(format!("{e:?}")))?,
        ));

        let endpoint = quinn::Endpoint::server(quinn_config, addr)?;
        Ok(Self {
            endpoint,
            fingerprint,
            db: Arc::new(Mutex::new(db)),
            registry: Arc::new(SessionRegistry::new()),
            voice: Arc::new(VoiceManager::new()),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint.local_addr().expect("socket is bound")
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub async fn run(self) -> Result<(), Error> {
        info!(addr = %self.local_addr(), fingerprint = %self.fingerprint, "parley listening");
        while let Some(incoming) = self.endpoint.accept().await {
            match incoming.await {
                Ok(conn) => {
                    let db = self.db.clone();
                    let registry = self.registry.clone();
                    let voice = self.voice.clone();
                    tokio::spawn(handle_connection(conn, db, registry, voice));
                }
                Err(e) => warn!(error = %e, "handshake failed"),
            }
        }
        Ok(())
    }
}

async fn handle_connection(
    conn: quinn::Connection,
    db: Arc<Mutex<Db>>,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
) {
    let peer = conn.remote_address();
    match serve(conn.clone(), &db, &registry, &voice).await {
        Ok(()) => info!(%peer, "session ended"),
        Err(e) => warn!(%peer, error = %e, "session failed"),
    }
    conn.closed().await;
}

async fn serve(
    conn: quinn::Connection,
    db: &Arc<Mutex<Db>>,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
) -> Result<(), Error> {
    let (mut tx, mut rx) = conn.accept_bi().await?;

    // --- Handshake ---
    let hello: Hello = session::read_message(&mut rx).await?;
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(Error::Session(parley_core::Error::Protocol(
            "unsupported protocol version",
        )));
    }
    session::write_message(
        &mut tx,
        &HelloAck {
            protocol_version: PROTOCOL_VERSION,
            server_caps: 0,
            heartbeat_ms: 5_000,
        },
    )
    .await?;

    // --- Auth (password or session token) ---
    let auth: AuthRequest = session::read_message(&mut rx).await?;
    let user_info = {
        let guard = db
            .lock()
            .map_err(|_| Error::Internal("database lock poisoned".into()))?;
        if !auth.session_token.is_empty() {
            guard.verify_session_token(&auth.session_token)?
        } else {
            guard.verify_user(&auth.username, &auth.password)?
        }
    };

    let info = match user_info {
        Some(info) => info,
        None => {
            session::write_message(
                &mut tx,
                &AuthDenied {
                    reason: "invalid credentials".to_string(),
                },
            )
            .await?;
            info!(username = %auth.username, "auth denied");
            tx.finish()?;
            return Ok(());
        }
    };

    let token = {
        let guard = db
            .lock()
            .map_err(|_| Error::Internal("database lock poisoned".into()))?;
        guard.create_session(info.id)?
    };

    info!(username = %info.username, user_id = info.id, "session started");

    // Register with the broadcast registry before acking.
    // NOTE: keyed by user_id — a second login by the same user replaces
    // the first (its event channel closes, that session ends).
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    registry.register(ClientHandle {
        user_id: info.id,
        username: info.username.clone(),
        conn: conn.clone(),
        tx: event_tx,
    });

    session::write_message(
        &mut tx,
        &AuthOk {
            user_id: info.id,
            is_admin: info.is_admin,
            session_token: token,
        },
    )
    .await?;

    // --- Dedicated reader tasks ---
    // Each reader owns its source, so an in-flight read is never
    // cancelled by select — partial reads can never be lost.
    // (A cancelled read_exact drops already-read bytes; that would
    // desynchronize the framed stream.)
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(m) = session::read_message::<ClientMessage>(&mut rx).await {
            if msg_tx.send(m).is_err() {
                break;
            }
        }
        // stream closed or malformed: reader ends, msg_rx closes below
    });

    let (dg_tx, mut dg_rx) = mpsc::unbounded_channel();
    let dg_conn = conn.clone();
    tokio::spawn(async move {
        while let Ok(d) = dg_conn.read_datagram().await {
            if dg_tx.send(d).is_err() {
                break;
            }
        }
        // connection dead: reader ends, dg_rx closes below
    });

    // --- Session loop: datagrams (hot path) > broadcasts > client msgs ---
    let result = loop {
        tokio::select! {
            biased;

            dg = dg_rx.recv() => match dg {
                Some(d) => route_datagram(&d, &info, registry, voice),
                None => break Ok(()),
            },

            ev = event_rx.recv() => match ev {
                Some(m) => {
                    if let Err(e) = session::write_message(&mut tx, &m).await {
                        break Err(e.into());
                    }
                }
                None => break Ok(()),
            },

            msg = msg_rx.recv() => match msg {
                Some(m) => {
                    match handle_client_message(
                        m, &info, &mut tx, db, registry, voice,
                    ).await {
                        Ok(true) => {}
                        Ok(false) => break Ok(()),
                        Err(e) => break Err(e),
                    }
                }
                None => break Ok(()),
            },
        }
    };

    // Cleanup: leave voice if member, broadcast the departure.
    if voice.leave(info.id).is_some() {
        registry.broadcast(ServerMessage {
            body: Some(ServerBody::VoiceState(PbVoiceState {
                user_id: info.id,
                channel_id: 0,
                joined: false,
            })),
        });
    }
    registry.unregister(info.id);
    info!(user_id = info.id, "session ended");
    let _ = tx.finish();
    if result.is_err() {
        conn.close(1u32.into(), b"session error");
    }
    result
}

/// Handle one client control message. Ok(false) = end session gracefully.
async fn handle_client_message(
    msg: ClientMessage,
    info: &crate::db::UserInfo,
    tx: &mut quinn::SendStream,
    db: &Arc<Mutex<Db>>,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
) -> Result<bool, Error> {
    match msg.body {
        Some(ClientBody::Heartbeat(hb)) => {
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::HeartbeatAck(HeartbeatAck {
                        client_time_ms: hb.client_time_ms,
                        server_time_ms: now_ms(),
                    })),
                },
            )
            .await?;
        }

        Some(ClientBody::PresenceRequest(_)) => {
            let users = registry.online_users();
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::PresenceList(PresenceList {
                        users: users
                            .into_iter()
                            .map(|(id, name)| PresenceUser {
                                user_id: id,
                                username: name,
                            })
                            .collect(),
                    })),
                },
            )
            .await?;
        }

        Some(ClientBody::ListChannels(_)) => {
            let channels = {
                let guard = db
                    .lock()
                    .map_err(|_| Error::Internal("database lock poisoned".into()))?;
                guard.list_channels()?
            };
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::ChannelList(ChannelList {
                        channels: channels
                            .into_iter()
                            .map(|(id, name)| PbChannel { id, name })
                            .collect(),
                    })),
                },
            )
            .await?;
        }

        Some(ClientBody::SendChat(chat)) => {
            let text = chat.text.trim().to_string();
            if text.is_empty() || text.len() > 2000 {
                return Ok(true); // ignore empty/oversized silently
            }
            let (msg_id, ts, _) = {
                let guard = db
                    .lock()
                    .map_err(|_| Error::Internal("database lock poisoned".into()))?;
                guard.insert_message(chat.channel_id, info.id, &text)?
            };
            registry.broadcast(ServerMessage {
                body: Some(ServerBody::ChatMessage(parley_proto::ChatMessage {
                    message_id: msg_id,
                    channel_id: chat.channel_id,
                    user_id: info.id,
                    username: info.username.clone(),
                    text,
                    timestamp_ms: ts,
                })),
            });
        }

        Some(ClientBody::FetchHistory(fetch)) => {
            let limit = fetch.limit.clamp(1, 100);
            let rows = {
                let guard = db
                    .lock()
                    .map_err(|_| Error::Internal("database lock poisoned".into()))?;
                guard.fetch_history(fetch.channel_id, limit, fetch.before_message_id)?
            };
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::HistoryResult(parley_proto::HistoryResult {
                        messages: rows
                            .into_iter()
                            .map(
                                |(id, user_id, username, text, ts)| parley_proto::ChatMessage {
                                    message_id: id,
                                    channel_id: fetch.channel_id,
                                    user_id,
                                    username,
                                    text,
                                    timestamp_ms: ts,
                                },
                            )
                            .collect(),
                    })),
                },
            )
            .await?;
        }

        Some(ClientBody::JoinVoice(j)) => {
            let exists = {
                let guard = db
                    .lock()
                    .map_err(|_| Error::Internal("database lock poisoned".into()))?;
                guard.channel_exists(j.channel_id)?
            };
            if exists {
                voice.join(info.id, j.channel_id);
                registry.broadcast(ServerMessage {
                    body: Some(ServerBody::VoiceState(PbVoiceState {
                        user_id: info.id,
                        channel_id: j.channel_id,
                        joined: true,
                    })),
                });
            }
            // joining a nonexistent channel is ignored
        }

        Some(ClientBody::LeaveVoice(_)) => {
            if voice.leave(info.id).is_some() {
                registry.broadcast(ServerMessage {
                    body: Some(ServerBody::VoiceState(PbVoiceState {
                        user_id: info.id,
                        channel_id: 0,
                        joined: false,
                    })),
                });
            }
        }

        Some(ClientBody::VoiceStatesRequest(_)) => {
            let states = voice.snapshot();
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::VoiceStateList(VoiceStateList {
                        states: states
                            .into_iter()
                            .map(|(user_id, channel_id)| PbVoiceState {
                                user_id,
                                channel_id,
                                joined: true,
                            })
                            .collect(),
                    })),
                },
            )
            .await?;
        }

        Some(ClientBody::ReceiverReport(rep)) => {
            // 4d: loss feedback routed like media — stamped with the
            // authenticated reporter id, delivered to voice-channel
            // peers. v1 uses broadcast (correct while one channel);
            // multi-channel wants member-targeted sends (registry needs
            // a send-to-user control method first).
            if let Some(channel_id) = voice.channel_of(info.id)
                && voice.members_of(channel_id).len() > 1
            {
                registry.broadcast(ServerMessage {
                    body: Some(ServerBody::ReceiverReport(ReceiverReport {
                        reporter_id: info.id,
                        gap_rate_permille: rep.gap_rate_permille,
                    })),
                });
            }
        }

        Some(ClientBody::StopShare(_)) => {
            // 6a: broadcast-wide like VoiceState (6b narrows to channel
            // members once report routing lands). Viewers tear down on
            // it; the sharer ignores its own echo by user_id.
            registry.broadcast(ServerMessage {
                body: Some(ServerBody::ShareStopped(ShareStopped { user_id: info.id })),
            });
        }

        None => {
            // body: None is what prost decodes UNKNOWN oneof variants
            // to (a newer client's message on this older server). The
            // protocol is additive by design — ignore, don't kill the
            // session.
            warn!(user_id = info.id, "ignoring empty/unknown message body");
        }
    }
    Ok(true)
}

/// Forward a media datagram from `sender` to their voice channel.
/// The sender id in the header is overwritten with the authenticated id.
fn route_datagram(
    data: &Bytes,
    sender: &crate::db::UserInfo,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
) {
    let decoded = match parley_proto::media::decode(data) {
        Ok(d) => d,
        Err(e) => {
            warn!(user_id = sender.id, error = e, "malformed media datagram");
            return;
        }
    };

    let Some(channel_id) = voice.channel_of(sender.id) else {
        return; // sender is not in a voice channel: drop
    };

    let forwarded = Bytes::from(parley_proto::media::encode(
        decoded.flags,
        decoded.seq,
        decoded.timestamp,
        sender.id,
        decoded.payload,
    ));

    for member in voice.members_of(channel_id) {
        if member != sender.id {
            registry.send_datagram(member, forwarded.clone());
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
