//! Configuration: TOML file with defaults.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// UDP listen address for QUIC.
    pub listen: String,
    /// Directory for persistent state (cert, key, database).
    pub data_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:4443".to_string(),
            data_dir: "data".into(),
        }
    }
}

impl Config {
    /// Load from `path`, or defaults if the file doesn't exist.
    pub fn load_or_default(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| Error::Config(e.to_string()))
    }
}
