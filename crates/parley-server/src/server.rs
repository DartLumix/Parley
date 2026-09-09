//! The QUIC listener: bind, accept, dispatch.

use std::net::SocketAddr;
use std::sync::Arc;

use quinn::crypto::rustls::QuicServerConfig;
use tracing::{info, warn};

use crate::config::Config;
use crate::error::Error;
use crate::tls;
use parley_core::session;
use parley_proto::{Hello, HelloAck, PROTOCOL_VERSION};

pub struct Server {
    endpoint: quinn::Endpoint,
    fingerprint: String,
}

impl Server {
    /// Bind + TLS setup. Synchronous: it's CPU work, no I/O to await.
    pub fn bind(config: &Config) -> Result<Self, Error> {
        tls::init_provider();

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
        })
    }

    /// The address actually bound (meaningful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint.local_addr().expect("socket is bound")
    }

    /// The certificate fingerprint clients pin.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Accept connections forever.
    pub async fn run(self) -> Result<(), Error> {
        info!(addr = %self.local_addr(), fingerprint = %self.fingerprint, "parley listening");
        while let Some(incoming) = self.endpoint.accept().await {
            match incoming.await {
                Ok(conn) => {
                    tokio::spawn(handle_connection(conn));
                }
                Err(e) => warn!(error = %e, "handshake failed"),
            }
        }
        Ok(())
    }
}

async fn handle_connection(conn: quinn::Connection) {
    let peer = conn.remote_address();
    match serve(&conn).await {
        Ok(()) => info!(%peer, "connection served"),
        Err(e) => warn!(%peer, error = %e, "connection failed"),
    }
    conn.closed().await;
}

async fn serve(conn: &quinn::Connection) -> Result<(), Error> {
    let (mut tx, mut rx) = conn.accept_bi().await?;

    let hello: Hello = session::read_message(&mut rx).await?;
    info!(version = hello.protocol_version, caps = hello.caps, "hello");

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
    tx.finish()?;
    Ok(())
}
