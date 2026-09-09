//! TLS identity: self-signed certificate, fingerprint, crypto provider.
use std::path::Path;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

use crate::error::Error;

/// Server identity: certificate + private key, DER (binary) form.
pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

/// Load identity from `data_dir`, or generate and persist a new one.
/// Returns (identity, was_newly_generated).
pub fn load_or_generate(data_dir: &Path) -> Result<(Identity, bool), Error> {
    let cert_path = data_dir.join("cert.der");
    let key_path = data_dir.join("key.pk8");

    let (cert_bytes, key_bytes, generated) = if cert_path.exists() && key_path.exists() {
        (std::fs::read(&cert_path)?, std::fs::read(&key_path)?, false)
    } else {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
        let cert_der: CertificateDer<'static> = certified.cert.into();
        let cert_bytes = cert_der.as_ref().to_vec();
        let key_bytes = certified.key_pair.serialize_der().to_vec();

        std::fs::create_dir_all(data_dir)?;
        std::fs::write(&cert_path, &cert_bytes)?;
        std::fs::write(&key_path, &key_bytes)?;
        (cert_bytes, key_bytes, true)
    };

    Ok((
        Identity {
            cert: CertificateDer::from(cert_bytes),
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes)),
        },
        generated,
    ))
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
