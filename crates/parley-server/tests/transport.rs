//! A real QUIC client talks to a real QUIC server over loopback UDP.
//! This is the Phase 1a milestone: transport + TLS + a stream exchange.

use std::sync::Arc;

use parley_server::{Server, tls};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

use parley_server::Config;
use std::path::PathBuf;

use parley_core::session;
use parley_proto::{Hello, HelloAck, PROTOCOL_VERSION};

use parley_proto::{AuthDenied, AuthOk, AuthRequest};

fn test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("parley-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn setup_server_with_user(tag: &str) -> (Config, Server) {
    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        data_dir: test_dir(tag),
    };
    // Create a user directly in the DB before the server binds
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let db = parley_server::db::Db::open(&config.data_dir.join("parley.db")).unwrap();
    db.create_user("testuser", "testpass", false).unwrap();
    drop(db);

    let server = Server::bind(&config).unwrap();
    (config, server)
}

#[tokio::test]
async fn auth_success() {
    init_logs();

    let (config, server) = setup_server_with_user("auth-ok");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();

    let (mut tx, mut rx) = conn.open_bi().await.unwrap();

    // Handshake
    session::write_message(
        &mut tx,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await
    .unwrap();

    let ack: HelloAck = session::read_message(&mut rx).await.unwrap();
    assert_eq!(ack.protocol_version, PROTOCOL_VERSION);

    // Auth
    session::write_message(
        &mut tx,
        &AuthRequest {
            username: "testuser".to_string(),
            password: "testpass".to_string(),
        },
    )
    .await
    .unwrap();

    let ok: AuthOk = session::read_message(&mut rx).await.unwrap();
    assert!(!ok.is_admin);
    assert!(ok.user_id > 0);

    conn.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn auth_failure_wrong_password() {
    let (config, server) = setup_server_with_user("auth-denied");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();

    let (mut tx, mut rx) = conn.open_bi().await.unwrap();

    // Handshake
    session::write_message(
        &mut tx,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await
    .unwrap();
    let _ack: HelloAck = session::read_message(&mut rx).await.unwrap();

    // Auth with wrong password
    session::write_message(
        &mut tx,
        &AuthRequest {
            username: "testuser".to_string(),
            password: "wrongpass".to_string(),
        },
    )
    .await
    .unwrap();

    let denied: AuthDenied = session::read_message(&mut rx).await.unwrap();
    assert!(!denied.reason.is_empty());

    conn.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn fingerprint_survives_restart() {
    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        data_dir: test_dir("restart"),
    };

    let fp1 = Server::bind(&config).unwrap().fingerprint().to_string();
    let fp2 = Server::bind(&config).unwrap().fingerprint().to_string();

    assert_eq!(fp1, fp2, "fingerprint must be stable across restarts");

    let _ = std::fs::remove_dir_all(&config.data_dir);
}

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .try_init();
}

/// A client endpoint that verifies the server by pinned fingerprint.
fn client_endpoint(pinned_fingerprint: String) -> quinn::Endpoint {
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedFingerprint(pinned_fingerprint)))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![tls::alpn()];

    let config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("valid client config"),
    ));

    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(config);
    endpoint
}

/// Certificate verification = fingerprint check. The test knows the
/// server's fingerprint (same process), so this is real verification,
/// not "accept anything". 1b moves the pin into the client itself
/// (pin on first connect, warn on change).
#[derive(Debug)]
struct PinnedFingerprint(String);

impl ServerCertVerifier for PinnedFingerprint {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let seen = tls::fingerprint(end_entity);
        if seen == self.0 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(format!(
                "fingerprint mismatch: pinned {}, saw {seen}",
                self.0
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
