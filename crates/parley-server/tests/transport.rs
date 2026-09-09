//! A real QUIC client talks to a real QUIC server over loopback UDP.
//! This is the Phase 1a milestone: transport + TLS + a stream exchange.

use std::sync::Arc;

use parley_server::{Server, tls};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_connects_server_echoes() {
    // Watch it happen: RUST_LOG=info cargo test -p parley-server \
    //   --test transport -- --nocapture
    init_logs();

    // Port 0 = "OS, pick a free port" — tests never collide.
    let server = tokio::spawn(async { Server::bind("127.0.0.1:0".parse().unwrap()).unwrap() })
        .await
        .unwrap();
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();

    let (mut tx, mut rx) = conn.open_bi().await.unwrap();
    tx.write_all(b"hello parley").await.unwrap();
    tx.finish().unwrap(); // half-close: server sees EOF
    let reply = rx.read_to_end(1024).await.unwrap();
    assert_eq!(reply, b"parley: hello parley");

    conn.close(0u32.into(), b"done");
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
