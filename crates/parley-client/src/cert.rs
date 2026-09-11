//! TOFU certificate pinning: known_hosts file + a rustls verifier.
//! Deliberately does NOT depend on parley-server — the fingerprint and
//! ALPN derivations are duplicated here (see note in build_endpoint).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};

use parley_proto::PROTOCOL_VERSION;

/// SHA-256 of the certificate DER, hex — the value the server prints.
fn fingerprint(cert: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(cert.as_ref()))
}

fn known_hosts_path() -> PathBuf {
    let config = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    config.join("parley").join("known_hosts")
}

/// The pin for this server, if we've connected before.
pub fn load_pin(server: &str) -> Result<(Option<String>, PathBuf), String> {
    let path = known_hosts_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok((None, path));
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        if let (Some(host), Some(fp)) = (parts.next(), parts.next())
            && host == server
        {
            return Ok((Some(fp.to_string()), path));
        }
    }
    Ok((None, path))
}

pub fn save_pin(path: &Path, server: &str, fingerprint: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{server} {fingerprint}\n"));
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Pin when we have a stored fingerprint; trust-on-first-use (and record
/// what we saw) when we don't. Key possession is always verified.
#[derive(Debug)]
struct TofuVerifier {
    pinned: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let fp = fingerprint(end_entity);
        *self.seen.lock().unwrap() = Some(fp.clone());
        match &self.pinned {
            Some(p) if *p != fp => Err(TlsError::General(format!(
                "FINGERPRINT MISMATCH for this server!\n  pinned: {p}\n  seen  : {fp}\nSomeone may be impersonating the server — verify out of band."
            ))),
            _ => Ok(ServerCertVerified::assertion()),
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

/// Client endpoint + the slot the verifier drops the presented
/// fingerprint into (read it after connecting).
pub fn build_endpoint(pinned: Option<String>) -> (quinn::Endpoint, Arc<Mutex<Option<String>>>) {
    let seen = Arc::new(Mutex::new(None));
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TofuVerifier {
            pinned,
            seen: seen.clone(),
        }))
        .with_no_client_auth();
    // Mirrors the server's tls::alpn() ("parley/<PROTOCOL_VERSION>") —
    // duplicated so the client doesn't link the server crate. A drift here
    // fails the handshake loudly, never silently.
    crypto.alpn_protocols = vec![format!("parley/{PROTOCOL_VERSION}").into_bytes()];

    let config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("valid client config"),
    ));
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().expect("valid bind addr"))
        .expect("bind client endpoint");
    endpoint.set_default_client_config(config);
    (endpoint, seen)
}
