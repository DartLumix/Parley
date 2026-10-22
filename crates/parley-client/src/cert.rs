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
    // Windows sets neither of the XDG variables below; %APPDATA% is the
    // per-user root there (%APPDATA%\parley\known_hosts). Falling through
    // would put the pins in the current directory, which is not a place
    // a trust decision should live.
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("APPDATA").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir).join("parley").join("known_hosts");
    }
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

/// The pin for this server, if we've connected before. `server` is a pin
/// key, not the raw typed address: [`parley_client::endpoint::pin_key`]
/// normalizes it, so `host` and `host:4443` are one trust decision.
pub fn load_pin(server: &str) -> Result<(Option<String>, PathBuf), String> {
    let path = known_hosts_path();
    Ok((read_pin(&path, server), path))
}

/// Last match wins: a hand-appended correction at the bottom of the file
/// is the pin the user meant.
fn read_pin(path: &Path, server: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(host), Some(fp)) if host == server => Some(fp.to_string()),
                _ => None,
            }
        })
        .next_back()
}

pub fn save_pin(path: &Path, server: &str, fingerprint: &str) -> Result<(), String> {
    rewrite_pins(path, server, Some(fingerprint))
}

/// Drop this server's pin (every copy) so the next connect trusts what the
/// server presents now. Only do that after comparing the fingerprint with
/// the one the server prints at startup — forgetting a pin is exactly the
/// moment an impostor would like you to reach for.
pub fn forget_pin(path: &Path, server: &str) -> Result<(), String> {
    rewrite_pins(path, server, None)
}

/// Rewrite the host's line to `fingerprint` (deduping stale copies), or
/// remove it when there is no fingerprint. Append-only + first-match-wins
/// meant a stale first line could never be displaced by a legitimate
/// re-pin.
fn rewrite_pins(path: &Path, server: &str, fingerprint: Option<&str>) -> Result<(), String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = String::new();
    let mut found = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        if parts.next() == Some(server) {
            if !found && let Some(fp) = fingerprint {
                out.push_str(&format!("{server} {fp}\n"));
            }
            found = true;
            continue; // drop duplicate/stale lines for this host
        }
        out.push_str(line);
        out.push('\n');
    }
    if !found {
        match fingerprint {
            Some(fp) => out.push_str(&format!("{server} {fp}\n")),
            // Nothing on file: do not create one just to forget it.
            None => return Ok(()),
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Why a handshake failed, in the terms the person on the other end needs,
/// when the certificate is the reason. `None` when the pin still holds (or
/// there was none) — the failure was something else.
///
/// The app keys its "trust new certificate" prompt off the leading
/// `fingerprint mismatch for` marker, so keep that prefix.
pub fn mismatch_report(
    path: &Path,
    server: &str,
    pinned: Option<&str>,
    seen: Option<&str>,
) -> Option<String> {
    let (Some(pinned), Some(seen)) = (pinned, seen) else {
        return None;
    };
    if pinned == seen {
        return None;
    }
    Some(format!(
        "fingerprint mismatch for {server}: the certificate changed.\n  \
         pinned: {pinned}\n  \
         seen:   {seen}\n\n\
         Expected after a redeploy or a recreated volume. Compare `seen` with \
         the fingerprint the server prints at startup: the app offers to trust \
         it, and from a shell you can delete this server's line in {}",
        path.display()
    ))
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

    let mut transport = quinn::TransportConfig::default();
    // 6e: outgoing datagram headroom for 2.5 Mbps video under
    // congestion backpressure — a full buffer runs quinn's drop-oldest
    // path, whose accounting has a known panic under saturation on
    // quinn-proto 0.11.17 (§8). 2 MiB ≈ 6 s at Clean, ~16 s at Robust.
    // Mirrors the server's Server::bind transport config.
    transport.datagram_send_buffer_size(2 * 1024 * 1024);
    // 6.9: the 6e lesson's missing half — the RECEIVE side. Default
    // (STREAM_RWND) overflows under video+audio load; overflow drops
    // inbound datagrams, and bursty small audio packets suffer most
    // (observed: 26% gap rate on loopback → PLC distortion).
    transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    let mut config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("valid client config"),
    ));
    config.transport_config(Arc::new(transport));
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().expect("valid bind addr"))
        .expect("bind client endpoint");
    endpoint.set_default_client_config(config);
    (endpoint, seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("parley-cert-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("known_hosts")
    }

    #[test]
    fn pin_roundtrip_replaces_and_dedupes() {
        let path = temp_file("roundtrip");
        assert_eq!(read_pin(&path, "a:4443"), None);

        save_pin(&path, "a:4443", "first").unwrap();
        save_pin(&path, "b:4443", "other").unwrap();
        save_pin(&path, "a:4443", "second").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "a:4443 second\nb:4443 other\n");
        assert_eq!(read_pin(&path, "a:4443").as_deref(), Some("second"));
        assert_eq!(read_pin(&path, "b:4443").as_deref(), Some("other"));

        // Hand-appended corrections are what the user meant.
        std::fs::write(&path, "a:4443 old\na:4443 new\n").unwrap();
        assert_eq!(read_pin(&path, "a:4443").as_deref(), Some("new"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn forget_removes_only_that_host() {
        let path = temp_file("forget");
        save_pin(&path, "a:4443", "one").unwrap();
        save_pin(&path, "b:4443", "two").unwrap();

        forget_pin(&path, "a:4443").unwrap();
        assert_eq!(read_pin(&path, "a:4443"), None);
        assert_eq!(read_pin(&path, "b:4443").as_deref(), Some("two"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b:4443 two\n");

        // Forgetting a host that was never there must not create the file
        // (or rewrite it) — a no-op stays a no-op.
        let absent = path.parent().unwrap().join("never");
        forget_pin(&absent, "a:4443").unwrap();
        assert!(!absent.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn mismatch_is_reported_only_when_the_certificate_changed() {
        let path = Path::new("/home/u/.config/parley/known_hosts");

        let report = mismatch_report(path, "host:4443", Some("aa"), Some("bb")).unwrap();
        assert!(
            report.starts_with("fingerprint mismatch for host:4443"),
            "{report}"
        );
        assert!(report.contains("pinned: aa") && report.contains("seen:   bb"));
        assert!(report.contains(path.to_str().unwrap()));

        assert!(mismatch_report(path, "host:4443", Some("aa"), Some("aa")).is_none());
        assert!(mismatch_report(path, "host:4443", None, Some("aa")).is_none());
        assert!(mismatch_report(path, "host:4443", Some("aa"), None).is_none());
    }
}
