//! Configuration: TOML file with defaults.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// UDP listen address for the Parley control QUIC endpoint.
    pub listen: String,
    /// UDP port for the MoQ media listener. Zero derives it from
    /// `listen` (control port + 2); a control port of 0 disables MoQ,
    /// which is what the in-process tests use.
    pub moq_port: u16,
    /// Directory for persistent state (cert, key, database).
    pub data_dir: PathBuf,
    /// Chat attachments. Empty (the default) means `<data_dir>/files`,
    /// which keeps them on the container's existing volume; a host with
    /// a separate disk can point this elsewhere.
    pub files_dir: PathBuf,
    /// Read-only HTTP presence endpoint (`GET /api/presence`) for a
    /// dashboard on the same host. Loopback by default; an empty string
    /// disables it. The container sets 0.0.0.0 and compose publishes the
    /// port to 127.0.0.1 on the host only.
    pub presence_listen: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:4443".to_string(),
            moq_port: 0,
            data_dir: "data".into(),
            files_dir: PathBuf::new(),
            presence_listen: "127.0.0.1:4444".to_string(),
        }
    }
}

impl Config {
    /// Where attachments live. Deriving from `data_dir` (rather than a
    /// second relative default) is load-bearing for the container: its
    /// data dir is `/data`, so attachments must land there too.
    pub fn files_dir(&self) -> PathBuf {
        if self.files_dir.as_os_str().is_empty() {
            self.data_dir.join("files")
        } else {
            self.files_dir.clone()
        }
    }

    /// The MoQ UDP port, or `None` when disabled. Auto-derivation uses
    /// control port + 2; the two listeners stay on separate sockets.
    pub fn moq_port(&self, control: std::net::SocketAddr) -> Option<u16> {
        if self.moq_port != 0 {
            return Some(self.moq_port);
        }
        // Port 0 means the OS picks the control port; deriving "2" from it
        // would try to bind a privileged socket. A port near 65535 cannot
        // be derived without colliding with (or wrapping onto) the control
        // listener either. Disabling the media plane is safer than silently
        // binding the wrong socket.
        let port = control.port();
        (port != 0).then(|| port.checked_add(2)).flatten()
    }

    /// Load from `path`, or defaults if the file doesn't exist.
    pub fn load_or_default(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| Error::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn moq_port_derives_from_the_control_port() {
        let config = Config::default();
        let addr: SocketAddr = "127.0.0.1:4443".parse().unwrap();
        assert_eq!(config.moq_port(addr), Some(4445));
    }

    #[test]
    fn ephemeral_or_overflowing_control_port_disables_moq() {
        let config = Config::default();
        assert_eq!(config.moq_port("127.0.0.1:0".parse().unwrap()), None);
        assert_eq!(config.moq_port("127.0.0.1:65534".parse().unwrap()), None);
        assert_eq!(config.moq_port("127.0.0.1:65535".parse().unwrap()), None);
    }

    #[test]
    fn explicit_moq_port_wins() {
        let config = Config {
            moq_port: 5555,
            ..Config::default()
        };
        assert_eq!(config.moq_port("127.0.0.1:0".parse().unwrap()), Some(5555));
    }
}
