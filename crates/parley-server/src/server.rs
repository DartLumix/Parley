//! The QUIC listener: bind, accept, dispatch, session loop.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use quinn::crypto::rustls::QuicServerConfig;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::Config;
use crate::db::{Db, DbHandle};
use crate::error::Error;
use crate::manage;
use crate::presence::PresenceServer;
use crate::session::{ClientHandle, SessionRegistry};
use crate::tls;
use crate::transcode::{ReportAction, TranscodeManager};
use crate::voice::VoiceManager;
use parley_core::session;
use parley_proto::{
    AuthDenied, AuthOk, AuthRequest, Channel as PbChannel, ChannelList, ChannelMode, ClientBody,
    ClientMessage, HeartbeatAck, Hello, HelloAck, PROTOCOL_VERSION, PresenceList, PresenceUser,
    ReceiverReport, ServerBody, ServerMessage, ShareStopped, VideoReport,
    VoiceState as PbVoiceState, VoiceStateList,
};

pub struct Server {
    endpoint: quinn::Endpoint,
    fingerprint: String,
    db: Db,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
    transcode: Arc<TranscodeManager>,
    /// Read-only HTTP snapshot for a dashboard; `None` when disabled.
    presence: Option<PresenceServer>,
}

impl Server {
    pub fn bind(config: &Config) -> Result<Self, Error> {
        tls::init_provider();

        let db = Db::open(&config.data_dir.join("parley.db"))?;
        db.bootstrap_admin()?;
        db.purge_stale_sessions()?;
        // Expired ephemeral chat is also filtered on read; sweeping at
        // boot keeps the file from carrying yesterday's rows until the
        // first periodic pass.
        if let Ok(removed) = db.purge_expired_messages()
            && removed > 0
        {
            info!(removed, "purged expired chat messages");
        }

        let (identity, _new) = tls::load_or_generate(&config.data_dir)?;
        let fingerprint = tls::fingerprint(&identity.cert);

        let addr: SocketAddr = config.listen.parse()?;

        let mut rustls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![identity.cert], identity.key)?;
        rustls_config.alpn_protocols = vec![tls::alpn()];

        let mut quinn_config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(rustls_config)
                .map_err(|e| Error::QuinnTls(format!("{e:?}")))?,
        ));
        // 6c: a silently-dead client (kill -9, network black hole) is
        // reaped by the QUIC idle timeout — 10 s ≈ 2 missed heartbeats
        // (heartbeat_ms: 5 s below). Live connections stay clear of
        // it: heartbeats ride the reliable control stream, so even
        // under hostile loss QUIC's retransmissions keep producing
        // packets. QUIC negotiates the minimum of both endpoints'
        // timeouts, so this applies regardless of client config.
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            std::time::Duration::from_secs(10)
                .try_into()
                .expect("10 s is a valid IdleTimeout"),
        ));

        // 6e: outgoing datagram headroom — quinn's default send buffer
        // is small relative to 2.5 Mbps video; a full buffer runs the
        // drop-oldest path, whose accounting has a known panic under
        // saturation on 0.11.17 (see §8). 2 MiB ≈ 6 s at Clean, ~16 s
        // at Robust — the drop path becomes unreachable short of a
        // pathological stall.
        transport.datagram_send_buffer_size(2 * 1024 * 1024);
        // 6.9: receive side of the 6e buffer fix — the server receives
        // the sharer's firehose and forwards it; default overflows
        // under load.
        transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
        quinn_config.transport_config(Arc::new(transport));

        let endpoint = quinn::Endpoint::server(quinn_config, addr)?;
        let registry = Arc::new(SessionRegistry::new());
        let voice = Arc::new(VoiceManager::new());
        // Built once: every session shares the same job table, so the
        // `max_transcodes` cap is host-wide rather than per connection.
        let transcode = Arc::new(TranscodeManager::new(
            config.video.clone(),
            registry.clone(),
            voice.clone(),
        ));
        // Bound here, not in `run`, so a taken port is a startup error
        // rather than a listener that silently died in a task.
        let presence = if config.presence_listen.trim().is_empty() {
            None
        } else {
            let token = std::env::var("PARLEY_PRESENCE_TOKEN")
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty());
            Some(PresenceServer::bind(&config.presence_listen, token)?)
        };
        Ok(Self {
            endpoint,
            fingerprint,
            db,
            registry,
            voice,
            transcode,
            presence,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint.local_addr().expect("socket is bound")
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The presence endpoint's bound address, if it is enabled. Tests use
    /// it with `presence_listen = "127.0.0.1:0"`.
    pub fn presence_addr(&self) -> Option<SocketAddr> {
        self.presence
            .as_ref()
            .and_then(|presence| presence.local_addr().ok())
    }

    pub async fn run(mut self) -> Result<(), Error> {
        info!(addr = %self.local_addr(), fingerprint = %self.fingerprint, "parley listening");
        let presence = self.presence.take();
        // 6c: the db actor owns the Connection on a dedicated thread —
        // no Mutex, no lock poisoning, and blocking work (Argon2
        // verifies, WAL checkpoints) never touches a tokio worker.
        let db = DbHandle::spawn(self.db);
        // Ephemeral chat: one sweep every ten minutes. Read-time
        // filtering makes the exact cadence invisible to clients.
        {
            let purge_db = db.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
                tick.tick().await; // the first tick fires immediately
                loop {
                    tick.tick().await;
                    match purge_db.purge_expired_messages().await {
                        Ok(0) => {}
                        Ok(n) => info!(removed = n, "purged expired chat messages"),
                        Err(e) => warn!(error = %e, "chat purge failed"),
                    }
                }
            });
        }
        if let Some(presence) = presence {
            if let Ok(addr) = presence.local_addr() {
                info!(%addr, "presence endpoint listening");
            }
            let registry = self.registry.clone();
            let voice = self.voice.clone();
            let presence_db = db.clone();
            tokio::spawn(async move {
                if let Err(e) = presence.serve(presence_db, registry, voice).await {
                    warn!(error = %e, "presence endpoint stopped");
                }
            });
        }
        while let Some(incoming) = self.endpoint.accept().await {
            match incoming.await {
                Ok(conn) => {
                    let db = db.clone();
                    let registry = self.registry.clone();
                    let voice = self.voice.clone();
                    let transcode = self.transcode.clone();
                    tokio::spawn(handle_connection(conn, db, registry, voice, transcode));
                }
                Err(e) => warn!(error = %e, "handshake failed"),
            }
        }
        Ok(())
    }
}

async fn handle_connection(
    conn: quinn::Connection,
    db: DbHandle,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
    transcode: Arc<TranscodeManager>,
) {
    let peer = conn.remote_address();
    match serve(conn.clone(), &db, &registry, &voice, &transcode).await {
        Ok(()) => info!(%peer, "session ended"),
        Err(e) => warn!(%peer, error = %e, "session failed"),
    }
    conn.closed().await;
}

async fn serve(
    conn: quinn::Connection,
    db: &DbHandle,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
    transcode: &Arc<TranscodeManager>,
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
    let user_info = if !auth.session_token.is_empty() {
        db.verify_token(&auth.session_token).await?
    } else {
        db.verify_user(&auth.username, &auth.password).await?
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

    let token = db.create_session(info.id).await?;

    info!(username = %info.username, user_id = info.id, "session started");

    // Register with the broadcast registry before acking. Keyed by
    // user_id: a second login by the same user replaces the first —
    // the old event channel closes and that session ends. The returned
    // generation guards the OLD session's cleanup against tearing down
    // its successor (6c).
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let session_gen = registry.register(ClientHandle {
        user_id: info.id,
        username: info.username.clone(),
        role: info.role,
        conn: conn.clone(),
        tx: event_tx,
        session: 0, // assigned by register()
    });

    // Presence push on join. The new client's own copy queues in
    // event_rx and is only written by the session loop below — which
    // starts after AuthOk — so its wire ordering stays auth-then-
    // presence; everyone else gets it immediately.
    registry.broadcast(presence_message(registry));

    session::write_message(
        &mut tx,
        &AuthOk {
            user_id: info.id,
            // `is_admin` stays the old binary gate (admin only); `role`
            // is the additive three-tier value a current client reads.
            is_admin: info.role.is_admin(),
            session_token: token,
            role: match info.role {
                crate::db::UserRole::Admin => parley_proto::Role::Admin as i32,
                crate::db::UserRole::Mod => parley_proto::Role::Mod as i32,
                crate::db::UserRole::User => parley_proto::Role::User as i32,
            },
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
                Some(d) => route_datagram(&d, &info, registry, voice, transcode),
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
                        m, &info, session_gen, &mut tx, db, registry, voice, transcode,
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

    // Cleanup is generation-guarded (6c): if a same-user successor
    // registered after us, the registry entry and the voice slot
    // belong to IT — our death must not unregister or boot them.
    // (Residual micro-race: a register landing between unregister_if
    // and voice.leave would be booted from voice; coupling the two
    // locks isn't worth it.)
    let still_current = registry.unregister_if(info.id, session_gen);
    // 7k: the channel a dying session was in, captured before the leave
    // below forgets it — the ShareStopped notice goes to the room.
    let died_in_channel = if still_current {
        voice.channel_of(info.id)
    } else {
        None
    };
    if still_current {
        // Presence push on leave: the fresh list (dying user already
        // unregistered) reaches the survivors. Generation-guarded — a
        // replaced session's death must not un-list its successor.
        registry.broadcast(presence_message(registry));
        // Mute is a SESSION preference (the client resets it at startup):
        // forgetting it here keeps the server's roster in step with a
        // client that signs back in unmuted.
        voice.clear_muted(info.id);
        // Same for the away status: it described that client's session.
        voice.clear_status(info.id);
    }
    if still_current && voice.leave(info.id).is_some() {
        registry.broadcast(voice_state_message(voice, info.id, 0, false));
    }
    // 7k: the session (and with it any share) is over — the channel it
    // was routed to must hear that, or a crash/disconnect strands a
    // frozen card on every viewer. `voice.leave` above already dropped
    // the membership, so the surviving members are all that remain.
    if let Some(channel_id) = died_in_channel
        && voice.stop_sharing(info.id)
    {
        notify_share_stopped(info.id, channel_id, registry, voice);
    }
    if still_current {
        // A dead session cannot keep owning a transcode job or a seat
        // in someone else's job.
        transcode.remove_share(info.id);
        transcode.remove_viewer(info.id);
    }
    info!(user_id = info.id, "session ended");
    let _ = tx.finish();
    if result.is_err() {
        conn.close(1u32.into(), b"session error");
    }
    result
}

/// Handle one client control message. Ok(false) = end session gracefully.
// The argument list is the session's whole environment (writer, db,
// registry, voice, transcode); splitting it into a struct would only
// move the same list one line up at the single call site.
#[allow(clippy::too_many_arguments)]
async fn handle_client_message(
    msg: ClientMessage,
    info: &crate::db::UserInfo,
    session_gen: u64,
    tx: &mut quinn::SendStream,
    db: &DbHandle,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
    transcode: &Arc<TranscodeManager>,
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
            session::write_message(tx, &presence_message(registry)).await?;
        }

        Some(ClientBody::ListChannels(_)) => {
            let channels = db.list_channels().await?;
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::ChannelList(ChannelList {
                        channels: channels
                            .into_iter()
                            .map(|(id, name, music, ephemeral)| PbChannel {
                                id,
                                name,
                                mode: if music {
                                    ChannelMode::Music as i32
                                } else {
                                    ChannelMode::Speech as i32
                                },
                                ephemeral,
                            })
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
            let (msg_id, ts, _) = db.insert_message(chat.channel_id, info.id, &text).await?;
            registry.broadcast(ServerMessage {
                body: Some(ServerBody::ChatMessage(parley_proto::ChatMessage {
                    message_id: msg_id,
                    channel_id: chat.channel_id,
                    user_id: info.id,
                    username: info.username.clone(),
                    text,
                    timestamp_ms: ts,
                    pinned: false,
                })),
            });
        }

        Some(ClientBody::FetchHistory(fetch)) => {
            let limit = fetch.limit.clamp(1, 100);
            let rows = db
                .fetch_history(fetch.channel_id, limit, fetch.before_message_id)
                .await?;
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::HistoryResult(parley_proto::HistoryResult {
                        messages: rows
                            .into_iter()
                            .map(|m| parley_proto::ChatMessage {
                                message_id: m.id,
                                channel_id: m.channel_id,
                                user_id: m.user_id,
                                username: m.username,
                                text: m.text,
                                timestamp_ms: m.timestamp_ms,
                                pinned: m.pinned,
                            })
                            .collect(),
                    })),
                },
            )
            .await?;
        }

        Some(ClientBody::PinMessage(pin)) => {
            // A pin is a chat update, not a new message: the row keeps
            // its id and text so every open client can replace it in
            // place. Any authenticated user may pin; the server has no
            // per-message ownership to enforce (the author may be gone).
            if let Some(m) = db.set_message_pinned(pin.message_id, pin.pinned).await? {
                registry.broadcast(ServerMessage {
                    body: Some(ServerBody::ChatMessage(parley_proto::ChatMessage {
                        message_id: m.id,
                        channel_id: m.channel_id,
                        user_id: m.user_id,
                        username: m.username,
                        text: m.text,
                        timestamp_ms: m.timestamp_ms,
                        pinned: m.pinned,
                    })),
                });
            }
        }

        Some(ClientBody::JoinVoice(j)) => {
            let exists = db.channel_exists(j.channel_id).await?;
            // is_current: a replaced session's stray messages must not
            // touch the successor's voice membership (6c).
            if exists && registry.is_current(info.id, session_gen) {
                // A switch is leave + join: peers in the OLD channel
                // must see a departure or their viewers/mixers freeze
                // (6b).
                if let Some(old) = voice.join(info.id, j.channel_id)
                    && old != j.channel_id
                {
                    // A channel switch is leave + join: the share died
                    // with the old membership.
                    if voice.stop_sharing(info.id) {
                        notify_share_stopped(info.id, old, registry, voice);
                    }
                    transcode.remove_share(info.id);
                    transcode.remove_viewer(info.id);
                    registry.broadcast(voice_state_message(voice, info.id, old, false));
                }
                registry.broadcast(voice_state_message(voice, info.id, j.channel_id, true));
            }
            // joining a nonexistent channel is ignored
        }

        Some(ClientBody::LeaveVoice(_)) => {
            if registry.is_current(info.id, session_gen)
                && let Some(old_channel) = voice.leave(info.id)
            {
                // 7k: a share cannot outlive channel membership — the
                // media route it rides is gone. Tell the room, or its
                // viewers hold a card whose Watch button shows a frozen
                // last frame forever (the reported 7j bug).
                if voice.stop_sharing(info.id) {
                    notify_share_stopped(info.id, old_channel, registry, voice);
                }
                transcode.remove_share(info.id);
                transcode.remove_viewer(info.id);
                registry.broadcast(voice_state_message(voice, info.id, 0, false));
            }
        }

        Some(ClientBody::SetMute(m)) => {
            // A mute is roster news, not channel-scoped: the whole session
            // sees it, so "muted" is right on the first join broadcast and
            // a state poll can never disagree with the last SetMute.
            if voice.set_muted(info.id, m.muted) {
                let channel = voice.channel_of(info.id);
                registry.broadcast(voice_state_message(
                    voice,
                    info.id,
                    channel.unwrap_or(0),
                    channel.is_some(),
                ));
            }
        }

        Some(ClientBody::SetStatus(s)) => {
            // Away text is roster news exactly like mute: whole session,
            // unchanged text is not re-broadcast.
            if voice.set_status(info.id, &s.text) {
                let channel = voice.channel_of(info.id);
                registry.broadcast(voice_state_message(
                    voice,
                    info.id,
                    channel.unwrap_or(0),
                    channel.is_some(),
                ));
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
                            .map(|(user_id, channel_id, muted, status)| PbVoiceState {
                                user_id,
                                channel_id,
                                joined: true,
                                muted,
                                status,
                            })
                            .collect(),
                    })),
                },
            )
            .await?;
        }

        Some(ClientBody::ReceiverReport(rep)) => {
            // 4d/6b: loss feedback routed like media — to the reporter's
            // voice-channel members only, reporter excluded.
            if let Some(channel_id) = voice.channel_of(info.id) {
                let msg = ServerMessage {
                    body: Some(ServerBody::ReceiverReport(ReceiverReport {
                        reporter_id: info.id,
                        gap_rate_permille: rep.gap_rate_permille,
                    })),
                };
                for member in voice.members_of(channel_id) {
                    if member != info.id {
                        registry.send_to_user(member, msg.clone());
                    }
                }
            }
        }

        Some(ClientBody::StopShare(_)) => {
            // 6a/6b: scoped exactly like the video datagrams it
            // terminates — members of the sharer's channel, sharer
            // excluded (their own state machine drove the stop).
            // An explicit stop always notifies: the client only sends
            // it after teardown, and a duplicate is a no-op downstream.
            voice.stop_sharing(info.id);
            transcode.remove_share(info.id);
            match voice.channel_of(info.id) {
                Some(channel_id) => notify_share_stopped(info.id, channel_id, registry, voice),
                // 7k: the sharer already left voice (or the leave and
                // the stop raced). Nobody is in the old channel by our
                // books, but a viewer may still hold the card — a
                // broadcast is harmless (a second stop is a no-op).
                None => registry.broadcast(ServerMessage {
                    body: Some(ServerBody::ShareStopped(ShareStopped { user_id: info.id })),
                }),
            }
        }

        Some(ClientBody::Manage(req)) => {
            // Management is a request/response on the same control stream.
            // Permission failures are ordinary results, not session
            // errors: a regular client that asks still gets an answer.
            let result = manage::handle(req, info, db, registry, voice, transcode).await;
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::ManageResult(result)),
                },
            )
            .await?;
        }

        Some(ClientBody::VideoReport(rep)) => {
            // 6e: video loss feedback, routed exactly like ReceiverReport
            // — reporter's channel members, reporter excluded. When the
            // server is re-encoding for this viewer, the report steers
            // the server's tier instead: forwarding it would also make
            // the sharer lower its own stream, and the viewer would get
            // a double-degraded picture.
            if let Some(channel_id) = voice.channel_of(info.id) {
                if transcode.on_report(info.id, channel_id, rep.viewer_fps)
                    == ReportAction::Suppress
                {
                    return Ok(true);
                }
                let msg = ServerMessage {
                    body: Some(ServerBody::VideoReport(VideoReport {
                        reporter_id: info.id,
                        gap_rate_permille: rep.gap_rate_permille,
                        viewer_fps: rep.viewer_fps,
                    })),
                };
                for member in voice.members_of(channel_id) {
                    if member != info.id {
                        registry.send_to_user(member, msg.clone());
                    }
                }
            }
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

/// Tell one channel's members that `user_id` is no longer sharing.
///
/// Every path that kills the share's route calls this: an explicit
/// stop, leaving voice, switching channels, and session end. Before
/// 7k only an explicit stop *while still in the channel* notified
/// anyone, so a sharer who closed their app (or hopped channels) left
/// viewers with a card that never went away.
pub(crate) fn notify_share_stopped(
    user_id: u32,
    channel_id: u64,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
) {
    let msg = ServerMessage {
        body: Some(ServerBody::ShareStopped(ShareStopped { user_id })),
    };
    for member in voice.members_of(channel_id) {
        if member != user_id {
            registry.send_to_user(member, msg.clone());
        }
    }
}

/// Forward a media datagram from `sender` to their voice channel.
/// The sender id in the header is overwritten with the authenticated id.
fn route_datagram(
    data: &Bytes,
    sender: &crate::db::UserInfo,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
    transcode: &Arc<TranscodeManager>,
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
    // 7k: a video datagram means a share is live for this user. The
    // teardown paths consult this flag so they only announce a stop
    // for someone who had something to stop.
    let is_video = decoded.flags & parley_proto::media::FLAG_VIDEO != 0;
    if is_video {
        voice.mark_sharing(sender.id);
        // Feed the transcode job (if one is active) and meter the
        // sharer's own rate. Audio never takes this path.
        transcode.note_video(sender.id, channel_id, decoded.payload);
    }

    let forwarded = Bytes::from(parley_proto::media::encode(
        decoded.flags,
        decoded.seq,
        decoded.timestamp,
        sender.id,
        decoded.payload,
    ));

    for member in voice.members_of(channel_id) {
        if member != sender.id && (!is_video || transcode.should_forward(sender.id, member)) {
            registry.send_datagram(member, forwarded.clone());
        }
    }
}

/// Presence push: the PresenceList for the current online set,
/// broadcast on every session start/end so connected clients never
/// hold a stale user list. Presence was pull-only until the Flutter
/// app became the first consumer and found the gap.
/// One user's roster row, stamped with the server-held mute and away
/// text. Every VoiceState the server emits goes through here, so a new
/// per-user field cannot be forgotten at one of the construction sites.
pub(crate) fn voice_state_message(
    voice: &VoiceManager,
    user_id: u32,
    channel_id: u64,
    joined: bool,
) -> ServerMessage {
    ServerMessage {
        body: Some(ServerBody::VoiceState(PbVoiceState {
            user_id,
            channel_id,
            joined,
            muted: voice.is_muted(user_id),
            status: voice.status_of(user_id),
        })),
    }
}

fn presence_message(registry: &SessionRegistry) -> ServerMessage {
    let users = registry.online_users();
    ServerMessage {
        body: Some(ServerBody::PresenceList(PresenceList {
            users: users
                .into_iter()
                .map(|(id, name)| PresenceUser {
                    user_id: id,
                    username: name,
                })
                .collect(),
        })),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
