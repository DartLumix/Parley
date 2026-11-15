//! Zero-transcode MoQ relay.
//!
//! Screen video, camera video and share audio are published by a client
//! and fanned out by [`moq_net::origin`]. The server never decodes,
//! re-encodes or inspects a media frame. Authentication reuses the
//! Parley session token from the control handshake; the token, media id
//! and requested room are checked before a MoQ session is accepted.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use moq_net::path::{Pattern, Patterns};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::db::{DbHandle, UserInfo};
use crate::error::Error;
use crate::server::{announce_share, notify_share_stopped};
use crate::session::SessionRegistry;
use crate::voice::VoiceManager;

pub struct MoqRelay {
    pub port: u16,
    pub fingerprint: String,
    server: moq_tokio::server::Server,
}

impl MoqRelay {
    /// Bind the media UDP listener. The identity is separate from the
    /// control channel's certificate; its fingerprint travels inside the
    /// authenticated `HelloAck`, never over an unauthenticated channel.
    pub fn bind(addr: SocketAddr) -> Result<Self, Error> {
        crate::tls::init_provider();
        let identity = moq_tokio::tls::Identity::generate(["parley"])
            .map_err(|e| Error::Config(format!("moq identity: {e}")))?;
        let fingerprint = identity.fingerprint().to_string();

        let mut listen = moq_tokio::listen::Config::default();
        listen.bind = Some(moq_tokio::listen::Bind::Addr(addr));
        listen.tls.identity = Some(identity);
        let mut config = moq_tokio::server::Config::default();
        config.listen = listen;
        let server = moq_tokio::server::Server::new(config)
            .map_err(|e| Error::Config(format!("moq listener: {e}")))?;
        let port = server
            .local_addr()
            .map_err(|e| Error::Config(format!("moq local address: {e}")))?
            .port();
        info!(%addr, port, "moq media listener bound");
        Ok(Self {
            port,
            fingerprint,
            server,
        })
    }

    /// Run the accept loop. The shared origin is the only state the
    /// relay keeps; all per-viewer queues are inside moq-net's group
    /// cache and expire through the one-second subscription budget.
    pub fn spawn(
        self,
        db: DbHandle,
        registry: Arc<SessionRegistry>,
        voice: Arc<VoiceManager>,
    ) -> JoinHandle<()> {
        let origin = moq_tokio::origin::spawn();
        tokio::spawn(async move {
            let mut server = match self.server.listen().await {
                Ok(server) => server,
                Err(e) => {
                    warn!(error = %e, "moq listener stopped");
                    return;
                }
            };
            while let Some(request) = server.accept().await {
                let db = db.clone();
                let registry = registry.clone();
                let voice = voice.clone();
                let origin = origin.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(request, db, registry, voice, origin).await {
                        warn!(error = %e, "moq session rejected");
                    }
                });
            }
        })
    }
}

async fn handle(
    request: moq_tokio::server::Request,
    db: DbHandle,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
    origin: moq_net::origin::Producer,
) -> Result<(), Error> {
    let path = request.path().trim_matches('/').to_string();
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 || parts[0] != "room" {
        let _ = request.reject(moq_tokio::server::Reject::Forbidden).await;
        return Ok(());
    }
    let Ok(channel_id) = parts[1].parse::<u64>() else {
        let _ = request.reject(moq_tokio::server::Reject::Forbidden).await;
        return Ok(());
    };
    let publisher = parts.len() == 5 && parts[2] == "user";
    if publisher && !matches!(parts[4], "screen" | "camera") {
        let _ = request.reject(moq_tokio::server::Reject::Forbidden).await;
        return Ok(());
    }
    let path_user = if publisher {
        parts[3].parse::<u32>().ok()
    } else {
        None
    };

    let mut token = String::new();
    let mut media_id = 0u32;
    if let Some(query) = request.query() {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "token" => token = value.into_owned(),
                "media" => media_id = value.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    if token.is_empty() {
        let _ = request
            .reject(moq_tokio::server::Reject::Unauthorized)
            .await;
        return Ok(());
    }
    let Some(user) = db.verify_token(&token).await? else {
        let _ = request
            .reject(moq_tokio::server::Reject::Unauthorized)
            .await;
        return Ok(());
    };

    // The media id is issued by the control plane and is the only
    // identity allowed to publish under this account's path.
    let Some(owner) = registry.user_of_media(media_id) else {
        let _ = request.reject(moq_tokio::server::Reject::Forbidden).await;
        return Ok(());
    };
    if owner != user.id
        // A publisher path must name the token's account. `None` here means
        // the user segment was malformed, not that this is a viewer URL.
        || (publisher && path_user != Some(user.id))
        || voice.channel_of(media_id) != Some(channel_id)
        || !registry.moq_media_of(media_id)
    {
        let _ = request.reject(moq_tokio::server::Reject::Forbidden).await;
        return Ok(());
    }

    let publish_root = format!("room/{channel_id}/user/{}", user.id);
    let room_root = format!("room/{channel_id}");
    let mut publish_patterns = Patterns::new();
    for kind in ["screen", "camera"] {
        publish_patterns.insert(
            Pattern::literal(&format!("{publish_root}/{kind}"))
                .map_err(|e| Error::Config(format!("moq publish scope: {e}")))?,
        );
    }
    let room_pattern =
        Pattern::subtree(&room_root).map_err(|e| Error::Config(format!("moq scope: {e}")))?;
    let publisher_scope = origin
        .scope("", &publish_patterns)
        .map_err(|e| Error::Config(format!("moq publish scope: {e}")))?;
    let subscriber_scope = origin
        .scope("", &Patterns::from(room_pattern))
        .map_err(|e| Error::Config(format!("moq subscribe scope: {e}")))?;

    let mut announcements = subscriber_scope.consume().announced();
    // One direction per session. A publisher feeds its own account path
    // into the shared origin; a viewer is served the room scope. The
    // naming is from the peer's side: the server's `with_subscriber`
    // ingests what the peer published, and the server's `with_publisher`
    // serves what the peer subscribes to.
    let accepted = if publisher {
        request.with_subscriber(publisher_scope)
    } else {
        request.with_publisher(subscriber_scope.consume())
    };
    let session = accepted
        .ok()
        .await
        .map_err(|e| Error::Config(format!("moq accept: {e}")))?;

    if publisher {
        let user: UserInfo = user;
        let registry_task = registry.clone();
        let voice_task = voice.clone();
        let username = user.username.clone();
        let user_id = user.id;
        let this_media = media_id;
        tokio::spawn(async move {
            let mut active: HashSet<String> = HashSet::new();
            // The epoch of the share this session announced. It is captured
            // after `announce_share`, so an old publisher's late cleanup
            // cannot retire a replacement share that reused the same media
            // id after a quick stop/start.
            let mut share_epoch: Option<u64> = None;
            // A media session is authorized against the voice membership at
            // SETUP. Keep checking it: a client that leaves or switches
            // channels must not keep publishing into the old room.
            let mut channel_check = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = channel_check.tick() => {
                        // `active` is empty until the first broadcast
                        // announcement; only then can sharing state be
                        // checked. A later StopShare must retire this
                        // session as well as notifying the room.
                        if voice_task.channel_of(this_media) != Some(channel_id)
                            || (!active.is_empty() && !voice_task.is_sharing(this_media))
                        {
                            break;
                        }
                    }
                    update = announcements.next() => {
                        let Some(update) = update else { break };
                        let path = update.prefix.as_str().to_string();
                        if !path.starts_with(&format!("{publish_root}/")) {
                            continue;
                        }
                        if update.kind.is_active() {
                            active.insert(path);
                            if active.len() == 1 {
                                announce_share(
                                    user_id,
                                    &username,
                                    this_media,
                                    channel_id,
                                    &registry_task,
                                    &voice_task,
                                );
                                share_epoch = voice_task.sharing_epoch(this_media);
                            }
                        } else {
                            active.remove(&path);
                            let retired = active.is_empty()
                                && share_epoch.is_some_and(|epoch| {
                                    voice_task.stop_sharing_if(this_media, epoch)
                                });
                            if retired
                                && let Some(member) = voice_task.member(this_media)
                            {
                                notify_share_stopped(
                                    member,
                                    member.channel_id,
                                    &registry_task,
                                    &voice_task,
                                );
                            }
                        }
                    }
                    _ = session.closed() => break,
                }
            }
            // Only a session that still owns an active broadcast and the
            // same share epoch may retire the share here. A normal close or
            // retraction has already emptied `active` through the
            // inactive-announcement path (and that path has already sent
            // ShareStopped). A late cleanup from an old publisher must not
            // stop a newer share that reused the same media id after a
            // quick stop/start.
            let retired = !active.is_empty()
                && share_epoch.is_some_and(|epoch| voice_task.stop_sharing_if(this_media, epoch));
            if retired && let Some(member) = voice_task.member(this_media) {
                notify_share_stopped(member, member.channel_id, &registry_task, &voice_task);
            }
        });
    } else {
        // A subscriber needs nothing beyond keeping the accepted session
        // alive; dropping it would close every track cursor immediately.
        // The channel check closes the session when this device leaves the
        // room, instead of leaving an authorized cursor on the old room.
        tokio::spawn(async move {
            let mut channel_check = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = session.closed() => break,
                    _ = channel_check.tick() => {
                        if voice.channel_of(media_id) != Some(channel_id) {
                            break;
                        }
                    }
                }
            }
        });
    }
    Ok(())
}
