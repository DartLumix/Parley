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
        let cert = std::fs::read(&cert_path)?;
        let key = std::fs::read(&key_path)?;
        // A key written by an older build may still carry the umask
        // default (0644); tighten it now that we know it is ours.
        restrict_key(&key_path);
        (cert, key, false)
    } else {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
        let cert_der: CertificateDer<'static> = certified.cert.into();
        let cert_bytes = cert_der.as_ref().to_vec();
        let key_bytes = certified.key_pair.serialize_der().to_vec();

        std::fs::create_dir_all(data_dir)?;
        std::fs::write(&cert_path, &cert_bytes)?;
        write_private_key(&key_path, &key_bytes)?;
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

/// The private key is the server's identity: owner-readable only.
/// `std::fs::write` would create it with the process umask (typically
/// 0644), exposing it to every local account.
fn write_private_key(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// Tighten a key that already exists. Best effort: a read-only data
/// volume must not stop the server from starting, and a fresh key is
/// already created 0600 by [`write_private_key`].
fn restrict_key(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not restrict private key permissions"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
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

#[cfg(all(test, unix))]
mod tests {
    use super::load_or_generate;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn generated_private_key_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("parley-tls-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (identity, generated) = load_or_generate(&dir).unwrap();
        assert!(generated);
        let mode = std::fs::metadata(dir.join("key.pk8"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the private key must not be group/world readable"
        );
        drop(identity);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
