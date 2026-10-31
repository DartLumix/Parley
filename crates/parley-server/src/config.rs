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
    /// Read-only HTTP presence endpoint (`GET /api/presence`) for a
    /// dashboard on the same host. Loopback by default; an empty string
    /// disables it. The container sets 0.0.0.0 and compose publishes the
    /// port to 127.0.0.1 on the host only.
    pub presence_listen: String,
    /// Screen-share transcode policy. Off means the server stays a pure
    /// datagram relay.
    pub video: VideoConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:4443".to_string(),
            data_dir: "data".into(),
            presence_listen: "127.0.0.1:4444".to_string(),
            video: VideoConfig::default(),
        }
    }
}

/// Server-side transcode for viewers that cannot decode the sharer's
/// original stream. The sender still encodes once; a job decodes that
/// stream and re-encodes only the tiers a weak viewer needs, so strong
/// viewers keep the untouched original.
///
/// Off by default: a small VPS cannot decode a 1440p60 share and encode
/// a 720p tier in real time, and the resulting 5 fps tier is worse than
/// handing the viewer back to the sender's own ladder. Enable it only on
/// a host with spare CPU, and watch the `transcode:` log lines.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    /// Enable per-viewer transcode. A pure relay when false.
    pub transcode: bool,
    /// How many shares may be transcoded at once. Each job is one
    /// worker thread; 0 disables.
    pub max_transcodes: usize,
    /// A viewer reporting below this rate is a transcode candidate.
    pub min_viewer_fps: u32,
    /// First tier height (720p by default).
    pub first_height: u32,
    /// Second tier, used if the viewer is still slow at the first.
    pub second_height: u32,
    /// Encoder threads per job. 1 keeps a job near one core; raising it
    /// makes the software encoder keep up more easily at 720p.
    pub threads: usize,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            transcode: false,
            max_transcodes: 1,
            // A 60 fps share decoded at 30 is not a failure; starting a
            // second encode for it used to turn a healthy viewer into a
            // 720p30 one. Only a genuinely stalled decoder is a
            // candidate, and the server gets two threads to keep up.
            min_viewer_fps: 15,
            first_height: 720,
            second_height: 480,
            threads: 2,
        }
    }
}

impl VideoConfig {
    /// Clamp values that would make the worker divide by zero or grow a
    /// tier instead of shrinking it.
    pub fn normalized(mut self) -> Self {
        let even = |v: u32| (v & !1).max(2);
        self.first_height = even(self.first_height);
        self.second_height = even(self.second_height).min(self.first_height);
        self.min_viewer_fps = self.min_viewer_fps.max(1);
        self.threads = self.threads.clamp(1, 16);
        self
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcode_is_opt_in_and_tiers_stay_ordered() {
        let video = VideoConfig::default();
        assert!(!video.transcode, "a small VPS must stay a pure relay");
        let normalized = VideoConfig {
            transcode: true,
            first_height: 481,
            second_height: 900,
            ..VideoConfig::default()
        }
        .normalized();
        assert_eq!(normalized.first_height, 480);
        assert_eq!(
            normalized.second_height, 480,
            "second tier never exceeds first"
        );
    }
}
