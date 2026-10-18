//! Server address handling shared by the terminal client and the
//! Flutter bridge: `host[:port]`, DNS names included.
//!
//! The address string the user typed is also the TOFU pin key, so this
//! module only resolves — it never rewrites the pin identity.

use std::net::{SocketAddr, ToSocketAddrs};

/// QUIC port used when the user types a bare host name.
pub const DEFAULT_PORT: u16 = 4443;

/// Split `host[:port]` (or `[v6][:port]`) into its parts, applying
/// [`DEFAULT_PORT`] when no port is given.
pub fn split(server: &str) -> Result<(String, u16), String> {
    let s = server.trim();
    if s.is_empty() {
        return Err("empty server address".into());
    }
    if let Some(rest) = s.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("unclosed '[' in server address: {s}"))?;
        let port = match tail {
            "" => DEFAULT_PORT,
            tail => match tail.strip_prefix(':') {
                Some(p) => parse_port(p, s)?,
                None => return Err(format!("unexpected text after ']' in {s}")),
            },
        };
        return Ok((host.to_string(), port));
    }
    if s.matches(':').count() > 1 {
        return Err(format!(
            "IPv6 addresses must be bracketed, e.g. [::1]:{DEFAULT_PORT} (got {s})"
        ));
    }
    match s.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => Ok((host.to_string(), parse_port(port, s)?)),
        Some(_) => Err(format!("missing host in {s}")),
        None => Ok((s.to_string(), DEFAULT_PORT)),
    }
}

fn parse_port(port: &str, whole: &str) -> Result<u16, String> {
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("bad port in {whole:?}")),
        Ok(p) => Ok(p),
    }
}

/// Resolve a user-typed server string to a socket address and the TLS
/// server name. Prefers IPv4: a name that answers with both is more
/// likely reachable over the `A` record from a home network.
pub fn resolve(server: &str) -> Result<(SocketAddr, String), String> {
    let (host, port) = split(server)?;
    let addrs: Vec<SocketAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .collect();
    let addr = addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| format!("{host}:{port} resolved to no addresses"))?;
    Ok((addr, host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_without_port_uses_the_default() {
        assert_eq!(
            split("parley.example.com").unwrap(),
            ("parley.example.com".into(), 4443)
        );
        assert_eq!(split("192.0.2.10").unwrap(), ("192.0.2.10".into(), 4443));
    }

    #[test]
    fn explicit_port_is_kept() {
        assert_eq!(
            split("parley.example.com:443").unwrap(),
            ("parley.example.com".into(), 443)
        );
        assert_eq!(
            split(" 192.0.2.10:1234 ").unwrap(),
            ("192.0.2.10".into(), 1234)
        );
    }

    #[test]
    fn bracketed_ipv6_is_accepted() {
        assert_eq!(
            split("[2001:db8::1]:443").unwrap(),
            ("2001:db8::1".into(), 443)
        );
        assert_eq!(
            split("[2001:db8::1]").unwrap(),
            ("2001:db8::1".into(), 4443)
        );
    }

    #[test]
    fn rejects_bad_forms() {
        assert!(split("").is_err());
        assert!(split("host:0").is_err());
        assert!(split("host:notaport").is_err());
        assert!(split("2001:db8::1").is_err()); // unbracketed IPv6
        assert!(split(":443").is_err());
        assert!(split("[::1").is_err());
    }

    #[test]
    fn resolves_loopback() {
        let (addr, name) = resolve("127.0.0.1:4443").unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(name, "127.0.0.1");
        let (addr, name) = resolve("localhost:4443").unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(name, "localhost");
    }
}
