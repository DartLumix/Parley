//! The QUIC listener: bind, accept, dispatch, session loop.

use std::net::SocketAddr;
use std::path::PathBuf;
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
    AuthDenied, AuthOk, AuthRequest, AvatarData, Channel as PbChannel, ChannelList, ChannelMode,
    ClientBody, ClientMessage, DeviceKind, HeartbeatAck, Hello, HelloAck, PROTOCOL_VERSION, Poked,
    PresenceList, PresenceUser, ProfileResult, ReceiverReport, ServerBody, ServerMessage,
    ShareStopped, VideoReport, VoiceState as PbVoiceState, VoiceStateList,
};

pub struct Server {
    endpoint: quinn::Endpoint,
    fingerprint: String,
    db: Db,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
    transcode: Arc<TranscodeManager>,
    /// Chat attachment root (sharded by file id).
    files_dir: PathBuf,
    /// Read-only HTTP snapshot for a dashboard; `None` when disabled.
    presence: Option<PresenceServer>,
}

impl Server {
    pub fn bind(config: &Config) -> Result<Self, Error> {
        tls::init_provider();

        let db = Db::open(&config.data_dir.join("parley.db"))?;
        db.bootstrap_admin()?;
        let files_dir = config.files_dir();
        std::fs::create_dir_all(&files_dir)?;
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

        // 6e/7m: outgoing datagram headroom — quinn's default send
        // buffer is small relative to HD video; a full buffer runs the
        // drop-oldest path, whose accounting has a known panic under
        // saturation on 0.11.17 (see §8). 8 MiB covers a 1440p60
        // keyframe plus the frames queued behind it at the 30 Mbps
        // profile ceiling.
        transport.datagram_send_buffer_size(8 * 1024 * 1024);
        // 6.9: receive side of the 6e buffer fix — the server receives
        // the sharer's firehose and forwards it; default overflows
        // under load.
        transport.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
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
            files_dir,
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
            let purge_files = self.files_dir.clone();
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
                    // Attachment bytes follow their message out. The row
                    // is deleted first, so a crash between the two leaves
                    // at worst an unreferenced file, never a dead row.
                    match purge_db.orphan_files().await {
                        Ok(ids) => {
                            for id in ids {
                                let path = crate::blob::file_path(&purge_files, id);
                                if let Err(e) = tokio::fs::remove_file(&path).await {
                                    warn!(file_id = id, error = %e, "attachment cleanup");
                                }
                            }
                        }
                        Err(e) => warn!(error = %e, "attachment cleanup failed"),
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
                    let files_dir = self.files_dir.clone();
                    tokio::spawn(handle_connection(
                        conn, db, registry, voice, transcode, files_dir,
                    ));
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
    files_dir: PathBuf,
) {
    let peer = conn.remote_address();
    match serve(conn.clone(), &db, &registry, &voice, &transcode, &files_dir).await {
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
    files_dir: &std::path::Path,
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

    // Mutable: a profile update can rename the live session without a
    // reconnect, so every later chat stamp and poke uses the new name.
    let mut info = match user_info {
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

    // A second login no longer replaces the first. Each connection gets
    // its own session + media identity; the phone and desktop can stay
    // online together and both can transmit. The session generation still
    // guards cleanup against a closed connection racing a newer one.
    let device = match DeviceKind::try_from(auth.device_kind) {
        Ok(DeviceKind::DeviceMobile) => DeviceKind::DeviceMobile as i32,
        _ => DeviceKind::DeviceDesktop as i32,
    };
    let device_name = if auth.device_name.trim().is_empty() {
        "desktop".to_string()
    } else {
        auth.device_name.trim().chars().take(64).collect()
    };
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let sound_versions = db.sound_versions(info.id).await?;
    let registered = registry.register(ClientHandle {
        user_id: info.id,
        username: info.username.clone(),
        avatar_version: info.avatar_version,
        sound_versions,
        role: info.role,
        conn: conn.clone(),
        tx: event_tx,
        session: 0,  // assigned by register()
        media_id: 0, // assigned by register()
        device,
        device_name,
    });
    let session_gen = registered.session;
    let media_id = registered.media;
    let other_sessions = registry.other_sessions(info.id, session_gen);

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
            session_id: session_gen,
            media_id,
            other_sessions,
        },
    )
    .await?;

    // Bulk transfers (chat attachments, personal sounds) ride their own
    // streams on this connection. The acceptor dies with the connection.
    crate::blob::spawn_acceptor(
        conn.clone(),
        info.id,
        db.clone(),
        registry.clone(),
        files_dir.to_path_buf(),
    );

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
                Some(d) => route_datagram(&d, &info, media_id, registry, voice, transcode),
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
                        m,
                        &mut info,
                        session_gen,
                        media_id,
                        device,
                        &mut tx,
                        db,
                        registry,
                        voice,
                        transcode,
                    )
                    .await
                    {
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
    // Capture the device membership before dropping it: the ShareStopped
    // notice needs the media/session identity, and a leave needs the
    // old channel.
    let died_member = if still_current {
        voice.member(media_id)
    } else {
        None
    };
    if still_current {
        // Presence still shows the account while another device remains;
        // the device badge list changes either way.
        registry.broadcast(presence_message(registry));
        voice.clear_muted(media_id);
        if registry.session_count(info.id) == 0 {
            voice.clear_status(info.id);
        }
    }
    if still_current && let Some(member) = died_member {
        voice.leave(media_id);
        registry.broadcast_voice(
            info.id,
            session_gen,
            voice_state_message(voice, &member, 0, false),
        );
    }
    if let Some(member) = died_member
        && voice.stop_sharing(media_id)
    {
        notify_share_stopped(member, member.channel_id, registry, voice);
    }
    if still_current {
        transcode.remove_share(media_id);
        transcode.remove_viewer(media_id);
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
    info: &mut crate::db::UserInfo,
    session_gen: u64,
    media_id: u32,
    device: i32,
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
            // A stale client can send into a channel deleted while it was
            // connected. Answer with silence, exactly like an unknown
            // JoinVoice: the INSERT would otherwise fail on the foreign
            // key and end the whole session.
            if !db.channel_exists(chat.channel_id).await? {
                warn!(
                    user_id = info.id,
                    channel_id = chat.channel_id,
                    "chat for a channel that does not exist"
                );
                return Ok(true);
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
                    ..Default::default()
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
                                attachment_id: m.attachment_id,
                                attachment_name: m.attachment_name,
                                attachment_size: m.attachment_size,
                                attachment_mime: m.attachment_mime,
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
                        attachment_id: m.attachment_id,
                        attachment_name: m.attachment_name,
                        attachment_size: m.attachment_size,
                        attachment_mime: m.attachment_mime,
                    })),
                });
            }
        }

        Some(ClientBody::JoinVoice(j)) => {
            let exists = db.channel_exists(j.channel_id).await?;
            // This connection must still be registered; its own media id
            // is the voice identity, so another device on the account is
            // unaffected.
            if exists && registry.is_registered(info.id, session_gen) {
                let next = crate::voice::VoiceMember {
                    media_id,
                    user_id: info.id,
                    session_id: session_gen,
                    channel_id: j.channel_id,
                    device,
                };
                if let Some(old) = voice.join(next)
                    && old.channel_id != j.channel_id
                {
                    // A switch is leave + join for THIS device only. The
                    // leave carries the channel it is leaving, so clients
                    // can clear the right roster entry.
                    if voice.stop_sharing(media_id) {
                        notify_share_stopped(old, old.channel_id, registry, voice);
                    }
                    transcode.remove_share(media_id);
                    transcode.remove_viewer(media_id);
                    registry.broadcast_voice(
                        info.id,
                        session_gen,
                        voice_state_message(voice, &old, old.channel_id, false),
                    );
                }
                if let Some(member) = voice.member(media_id) {
                    registry.broadcast_voice(
                        info.id,
                        session_gen,
                        voice_state_message(voice, &member, j.channel_id, true),
                    );
                }
            }
            // joining a nonexistent channel is ignored
        }

        Some(ClientBody::LeaveVoice(_)) => {
            if registry.is_registered(info.id, session_gen)
                && let Some(member) = voice.leave(media_id)
            {
                if voice.stop_sharing(media_id) {
                    notify_share_stopped(member, member.channel_id, registry, voice);
                }
                transcode.remove_share(media_id);
                transcode.remove_viewer(media_id);
                registry.broadcast_voice(
                    info.id,
                    session_gen,
                    voice_state_message(voice, &member, 0, false),
                );
            }
        }

        Some(ClientBody::SetMute(m)) => {
            // Mute is per device: muting the phone must not silence the
            // desktop's microphone. It is still roster news, broadcast to
            // every connected client.
            if voice.set_muted(media_id, m.muted)
                && let Some(member) = voice.member(media_id)
            {
                registry.broadcast_voice(
                    info.id,
                    session_gen,
                    voice_state_message(voice, &member, member.channel_id, true),
                );
            }
        }

        Some(ClientBody::Poke(p)) => {
            // A poke is a nudge between two people, not channel chatter:
            // relay it to the named user only, and never echo it to the
            // sender. The target must be connected; an offline target is
            // simply not disturbed (the UI only offers online rows).
            if p.user_id != 0 && p.user_id != info.id {
                let delivered = registry.send_to_user(
                    p.user_id,
                    ServerMessage {
                        body: Some(ServerBody::Poked(Poked {
                            from_user_id: info.id,
                            from_username: info.username.clone(),
                            channel_id: voice.channel_of(media_id).unwrap_or(0),
                        })),
                    },
                );
                if !delivered {
                    warn!(
                        user_id = info.id,
                        target = p.user_id,
                        "poke target is offline"
                    );
                }
            }
        }

        Some(ClientBody::SetStatus(s)) => {
            // Away text belongs to the ACCOUNT, so changing it updates
            // every device's voice row. Unchanged text is not re-broadcast.
            if voice.set_status(info.id, &s.text) {
                for id in voice.media_ids_of_user(info.id) {
                    if let Some(member) = voice.member(id) {
                        registry.broadcast_voice(
                            info.id,
                            session_gen,
                            voice_state_message(voice, &member, member.channel_id, true),
                        );
                    }
                }
            }
        }

        Some(ClientBody::UpdateProfile(p)) => {
            // The current password authorizes every profile change,
            // including an avatar-only one: a token alone must not let a
            // shared machine rewrite the account.
            if db
                .verify_user(&info.username, &p.current_password)
                .await?
                .is_none()
            {
                reply_profile(tx, false, "current password is incorrect", "").await?;
                return Ok(true);
            }

            let new_username = p.new_username.trim();
            if new_username.chars().count() > 32 || new_username.chars().any(char::is_control) {
                reply_profile(tx, false, "username must be 1-32 characters", "").await?;
                return Ok(true);
            }
            if p.new_password.len() > 256 {
                reply_profile(tx, false, "password is too long", "").await?;
                return Ok(true);
            }
            if !p.avatar.is_empty() {
                if p.avatar.len() > MAX_AVATAR_BYTES {
                    reply_profile(tx, false, "picture is too large (48 KiB max)", "").await?;
                    return Ok(true);
                }
                if !avatar_mime_ok(&p.avatar_mime) {
                    reply_profile(tx, false, "unsupported picture format", "").await?;
                    return Ok(true);
                }
            }

            let rename =
                (!new_username.is_empty() && new_username != info.username).then_some(new_username);
            let repass = (!p.new_password.is_empty()).then_some(p.new_password.as_str());
            let avatar_change = !p.avatar.is_empty() || p.clear_avatar;
            if rename.is_none() && repass.is_none() && !avatar_change {
                reply_profile(tx, false, "nothing to change", &info.username).await?;
                return Ok(true);
            }

            if (rename.is_some() || repass.is_some())
                && let Err(e) = db.update_user(info.id, rename, repass, None).await
            {
                reply_profile(tx, false, &e.to_string(), "").await?;
                return Ok(true);
            }
            if let Some(name) = rename {
                info.username = name.to_string();
            }

            if !p.avatar.is_empty() {
                match db
                    .set_avatar(info.id, Some(&p.avatar), &p.avatar_mime)
                    .await
                {
                    Ok(version) => info.avatar_version = version,
                    Err(e) => {
                        reply_profile(tx, false, &e.to_string(), "").await?;
                        return Ok(true);
                    }
                }
            } else if p.clear_avatar {
                match db.set_avatar(info.id, None, "").await {
                    Ok(version) => info.avatar_version = version,
                    Err(e) => {
                        reply_profile(tx, false, &e.to_string(), "").await?;
                        return Ok(true);
                    }
                }
            }

            // The live entry carries the new name/version for chat stamps,
            // pokes and the roster; the broadcast tells every other client
            // to refresh its cached name and avatar version.
            registry.update_identity(info.id, info.username.clone(), info.avatar_version);
            registry.broadcast(presence_message(registry));
            reply_profile(tx, true, "", &info.username).await?;
        }

        Some(ClientBody::RequestAvatar(r)) => {
            // Empty bytes + version 0 is a valid, cacheable answer for
            // an account with no picture (or an unknown id): the client
            // then stops asking until the version moves.
            let (avatar, avatar_mime, avatar_version) = db
                .avatar(r.user_id)
                .await?
                .unwrap_or_else(|| (Vec::new(), String::new(), 0));
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::AvatarData(AvatarData {
                        user_id: r.user_id,
                        avatar_version,
                        avatar_mime,
                        avatar,
                    })),
                },
            )
            .await?;
        }

        Some(ClientBody::VoiceStatesRequest(_)) => {
            let states = voice.snapshot();
            session::write_message(
                tx,
                &ServerMessage {
                    body: Some(ServerBody::VoiceStateList(VoiceStateList {
                        states: states
                            .into_iter()
                            .map(|member| PbVoiceState {
                                user_id: member.user_id,
                                channel_id: member.channel_id,
                                joined: true,
                                muted: voice.is_muted(member.media_id),
                                status: voice.status_of(member.user_id),
                                session_id: member.session_id,
                                media_id: member.media_id,
                                device_kind: member.device,
                            })
                            .collect(),
                    })),
                },
            )
            .await?;
        }

        Some(ClientBody::ReceiverReport(rep)) => {
            // Loss feedback is routed like media: to the reporter's
            // voice-channel devices, reporter device excluded.
            if let Some(channel_id) = voice.channel_of(media_id) {
                let msg = ServerMessage {
                    body: Some(ServerBody::ReceiverReport(ReceiverReport {
                        reporter_id: info.id,
                        reporter_media_id: media_id,
                        gap_rate_permille: rep.gap_rate_permille,
                    })),
                };
                for member in voice.members_of(channel_id) {
                    if member.media_id != media_id {
                        registry.send_to_media(member.media_id, msg.clone());
                    }
                }
            }
        }

        Some(ClientBody::StopShare(_)) => {
            // Scoped to THIS device's share. A desktop share must not be
            // stopped by the phone's state machine ending its own share.
            let member = voice.member(media_id);
            voice.stop_sharing(media_id);
            transcode.remove_share(media_id);
            match member {
                Some(member) => notify_share_stopped(member, member.channel_id, registry, voice),
                None => registry.broadcast(ServerMessage {
                    body: Some(ServerBody::ShareStopped(ShareStopped {
                        user_id: info.id,
                        session_id: session_gen,
                        media_id,
                    })),
                }),
            }
        }

        Some(ClientBody::LogOutOtherDevices(_)) => {
            // The token stays valid on the other devices by design: this
            // is a live-session disconnect, not a credential revocation.
            registry.log_out_other_sessions(info.id, session_gen);
        }

        Some(ClientBody::Manage(req)) => {
            // Management is a request/response on the same control stream.
            // Permission failures are ordinary results, not session
            // errors: a regular client that asks still gets an answer.
            let result =
                manage::handle(req, info, session_gen, db, registry, voice, transcode).await;
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
            if let Some(channel_id) = voice.channel_of(media_id) {
                // A request-only report is not a decode-capability
                // sample: forwarding it must not create a transcode job.
                if !rep.keyframe_request
                    && transcode.on_report(media_id, channel_id, rep.viewer_fps)
                        == ReportAction::Suppress
                {
                    return Ok(true);
                }
                let msg = ServerMessage {
                    body: Some(ServerBody::VideoReport(VideoReport {
                        reporter_id: info.id,
                        reporter_media_id: media_id,
                        gap_rate_permille: rep.gap_rate_permille,
                        viewer_fps: rep.viewer_fps,
                        keyframe_request: rep.keyframe_request,
                    })),
                };
                for member in voice.members_of(channel_id) {
                    if member.media_id != media_id {
                        registry.send_to_media(member.media_id, msg.clone());
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
    sharer: crate::voice::VoiceMember,
    channel_id: u64,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
) {
    let msg = ServerMessage {
        body: Some(ServerBody::ShareStopped(ShareStopped {
            user_id: sharer.user_id,
            session_id: sharer.session_id,
            media_id: sharer.media_id,
        })),
    };
    for member in voice.members_of(channel_id) {
        if member.media_id != sharer.media_id {
            registry.send_to_media(member.media_id, msg.clone());
        }
    }
}

/// Forward a media datagram from `sender` to their voice channel.
/// The sender field in the header is overwritten with this connection's
/// media id, so a receiver can tell the phone from the desktop.
fn route_datagram(
    data: &Bytes,
    sender: &crate::db::UserInfo,
    media_id: u32,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
    transcode: &Arc<TranscodeManager>,
) {
    let decoded = match parley_proto::media::decode(data) {
        Ok(d) => d,
        Err(e) => {
            warn!(
                user_id = sender.id,
                media_id,
                error = e,
                "malformed media datagram"
            );
            return;
        }
    };

    let Some(channel_id) = voice.channel_of(media_id) else {
        return; // this device is not in a voice channel: drop
    };
    let is_video = decoded.flags & parley_proto::media::FLAG_VIDEO != 0;
    if is_video {
        voice.mark_sharing(media_id);
        transcode.note_video(media_id, channel_id, decoded.payload);
    }

    let forwarded = Bytes::from(parley_proto::media::encode(
        decoded.flags,
        decoded.seq,
        decoded.timestamp,
        media_id,
        decoded.payload,
    ));

    // Same-account devices do not receive each other's media. On a phone
    // and a desktop sitting in the same room that is the echo loop; on
    // different channels the routing already keeps them apart.
    let targets: Vec<u32> = voice
        .members_of(channel_id)
        .into_iter()
        .filter(|member| member.media_id != media_id && member.user_id != sender.id)
        .filter(|member| !is_video || transcode.should_forward(media_id, member.media_id))
        .map(|member| member.media_id)
        .collect();
    registry.send_datagram_many(&targets, &forwarded);
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
    member: &crate::voice::VoiceMember,
    channel_id: u64,
    joined: bool,
) -> ServerMessage {
    ServerMessage {
        body: Some(ServerBody::VoiceState(PbVoiceState {
            user_id: member.user_id,
            channel_id,
            joined,
            muted: voice.is_muted(member.media_id),
            status: voice.status_of(member.user_id),
            session_id: member.session_id,
            media_id: member.media_id,
            device_kind: member.device,
        })),
    }
}

pub(crate) fn presence_message(registry: &SessionRegistry) -> ServerMessage {
    let users = registry.online_users();
    ServerMessage {
        body: Some(ServerBody::PresenceList(PresenceList {
            users: users
                .into_iter()
                .map(|user| PresenceUser {
                    user_id: user.id,
                    username: user.username,
                    avatar_version: user.avatar_version,
                    sound_join_version: user.sound_versions.0,
                    sound_leave_version: user.sound_versions.1,
                    sound_afk_version: user.sound_versions.2,
                    devices: user.devices,
                })
                .collect(),
        })),
    }
}

/// Pictures are resized client-side (a square thumbnail) before they hit
/// the wire; this is the server's backstop, not the normal size. It stays
/// well below `parley_proto::frame::MAX_SIZE` (64 KiB) so an avatar can
/// never turn a control message into a session-ending frame.
const MAX_AVATAR_BYTES: usize = 48 * 1024;
// A control frame is length-prefixed and capped at 64 KiB. The avatar is
// the only variable-size field in a profile message; keep a wide margin
// for the frame prefix and the protobuf tags so this can never kill a
// session by construction.
const _: () = assert!(MAX_AVATAR_BYTES + 8 * 1024 < parley_proto::frame::MAX_SIZE);

fn avatar_mime_ok(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    )
}

/// One profile-result reply. Every failure is an ordinary answer, never a
/// dropped session: a wrong old password is a form error, not a protocol
/// error.
async fn reply_profile(
    tx: &mut quinn::SendStream,
    ok: bool,
    error: &str,
    username: &str,
) -> Result<(), Error> {
    session::write_message(
        tx,
        &ServerMessage {
            body: Some(ServerBody::ProfileResult(ProfileResult {
                ok,
                error: error.to_string(),
                username: username.to_string(),
            })),
        },
    )
    .await?;
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
