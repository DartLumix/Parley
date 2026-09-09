//! The QUIC listener: bind, accept, dispatch.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use quinn::crypto::rustls::QuicServerConfig;
use tracing::{info, warn};

use crate::config::Config;
use crate::db::Db;
use crate::error::Error;
use crate::presence::PresenceManager;
use crate::tls;
use parley_core::session;
use parley_proto::{
    AuthDenied, AuthOk, AuthRequest, ClientBody, ClientMessage, HeartbeatAck, Hello, HelloAck,
    PROTOCOL_VERSION, PresenceList, PresenceUser, ServerBody, ServerMessage,
};

pub struct Server {
    endpoint: quinn::Endpoint,
    fingerprint: String,
    db: Arc<Mutex<Db>>,
    presence: Arc<PresenceManager>,
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
            presence: Arc::new(PresenceManager::new()),
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
                    let presence = self.presence.clone();
                    tokio::spawn(handle_connection(conn, db, presence));
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
    presence: Arc<PresenceManager>,
) {
    let peer = conn.remote_address();
    match serve(&conn, &db, &presence).await {
        Ok(()) => info!(%peer, "session ended"),
        Err(e) => warn!(%peer, error = %e, "session failed"),
    }
    conn.closed().await;
}

async fn serve(
    conn: &quinn::Connection,
    db: &Arc<Mutex<Db>>,
    presence: &Arc<PresenceManager>,
) -> Result<(), Error> {
    let (mut tx, mut rx) = conn.accept_bi().await?;

    // --- Handshake ---
    let hello: Hello = session::read_message(&mut rx).await?;
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(Error::Session(parley_core::Error::Protocol(
            "unsupported protocol version",
        )));
    }

    let ack = HelloAck {
        protocol_version: PROTOCOL_VERSION,
        server_caps: 0,
        heartbeat_ms: 5_000,
    };
    session::write_message(&mut tx, &ack).await?;

    // --- Auth (password or session token) ---
    let auth: AuthRequest = session::read_message(&mut rx).await?;
    info!(username = %auth.username, "auth request");

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
            let denied = AuthDenied {
                reason: "invalid credentials".to_string(),
            };
            session::write_message(&mut tx, &denied).await?;
            info!(username = %auth.username, "auth denied");
            tx.finish()?;
            return Ok(());
        }
    };

    // Issue session token
    let token = {
        let guard = db
            .lock()
            .map_err(|_| Error::Internal("database lock poisoned".into()))?;
        guard.create_session(info.id)?
    };

    // Register presence BEFORE writing AuthOk, so the user is visible
    // to other clients as soon as this client sees the ack.
    presence.add_user(info.id, auth.username.clone());

    session::write_message(
        &mut tx,
        &AuthOk {
            user_id: info.id,
            is_admin: info.is_admin,
            session_token: token,
        },
    )
    .await?;
    info!(username = %auth.username, user_id = info.id, "session started");

    // --- Session loop (until client disconnects or stream fails) ---
    let session_result = run_session(&mut tx, &mut rx, presence).await;

    // Always clean up presence
    presence.remove_user(info.id);
    info!(user_id = info.id, "session ended");

    let _ = tx.finish();
    session_result
}

async fn run_session(
    tx: &mut quinn::SendStream,
    rx: &mut quinn::RecvStream,
    presence: &Arc<PresenceManager>,
) -> Result<(), Error> {
    loop {
        let msg: ClientMessage = session::read_message(rx).await?;
        match msg.body {
            Some(ClientBody::Heartbeat(hb)) => {
                let ack = ServerMessage {
                    body: Some(ServerBody::HeartbeatAck(HeartbeatAck {
                        client_time_ms: hb.client_time_ms,
                        server_time_ms: now_ms(),
                    })),
                };
                session::write_message(tx, &ack).await?;
            }
            Some(ClientBody::PresenceRequest(_)) => {
                let users = presence.online_users();
                let list = ServerMessage {
                    body: Some(ServerBody::PresenceList(PresenceList {
                        users: users
                            .into_iter()
                            .map(|(id, name)| PresenceUser {
                                user_id: id,
                                username: name,
                            })
                            .collect(),
                    })),
                };
                session::write_message(tx, &list).await?;
            }
            None => {
                return Err(Error::Session(parley_core::Error::Protocol(
                    "empty message body",
                )));
            }
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
