//! TLS identity: self-signed certificate, fingerprint, crypto provider.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

use crate::error::Error;

/// Server identity: certificate + private key, DER (binary) form.
pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

/// Generate a fresh self-signed identity (Ed25519).
/// 1b replaces this with load-or-generate from the data directory so the
/// fingerprint — the thing clients pin — survives restarts.
pub fn generate_identity() -> Result<Identity, Error> {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    Ok(Identity {
        cert: certified.cert.into(),
        // KeyPair → raw PKCS#8 DER bytes → owned key object
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.key_pair.serialize_der().to_vec(),
        )),
    })
}

/// SHA-256 of the certificate's DER bytes, hex-encoded.
/// This is the value humans share out-of-band and clients pin (TOFU).
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(cert.as_ref()))
}

/// ALPN protocol tag, derived from the protocol version constant —
/// single source of truth, shared by server and clients.
pub fn alpn() -> Vec<u8> {
    format!("parley/{}", parley_proto::handshake::PROTOCOL_VERSION).into_bytes()
}

/// Install `ring` as rustls's process-wide crypto provider.
/// Which backend gets used can otherwise depend on cargo feature
/// unification across the dependency graph. We choose, once, explicitly.
pub fn init_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}
