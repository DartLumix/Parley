//! Persisted client preferences.
//!
//! Path: `$XDG_CONFIG_HOME/parley/settings.toml` — the same directory as
//! the remembered login and the TOFU pins, but a different file: this one
//! holds no credential, so losing it costs the user their preferences and
//! nothing else. A file that does not parse falls back to the defaults
//! (an older build's file must not lock anyone out of the app) and is
//! rewritten on the next save.
//!
//! The values are read where they take effect, not mirrored into a dozen
//! statics: [`show_share_preview`] is checked by the capture tap on every
//! encoded frame, [`echo_cancel`] at voice start, and `save` applies the
//! hardware-encode preference through the same call the old in-bar toggle
//! used. A change therefore needs no restart to apply to the NEXT share
//! or voice connection; a running one keeps what it started with (the
//! encoder's backend and the AEC are fixed at creation).

use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

use serde::{Deserialize, Serialize};

/// Everything the settings page edits. `#[serde(default)]` is on the
/// container so a file written by an older build (fewer fields) loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Prefer the VAAPI encoder. Applied by `save`; a share in flight
    /// keeps the backend it was created with.
    pub hw_encode: bool,
    /// Show the sharer their own frame while sharing. Off = the capture
    /// tap does no work at all.
    pub show_share_preview: bool,
    /// Default resolution cap for a new share, in pixels of height.
    /// 0 = the capture's own size (never upscaled).
    pub share_height: u32,
    /// Default frame-rate cap for a new share. 0 = the source rate.
    pub share_fps: u32,
    /// Run echo cancellation on speech channels. Music channels never
    /// use it (transparency wins there), whatever this says.
    pub echo_cancel: bool,
    /// Capture device name, or empty for the system default. A saved
    /// device that has since disappeared falls back to the default.
    pub input_device: String,
    /// Render device name, or empty for the system default.
    pub output_device: String,
    /// Apply automatic gain to the microphone before encoding. Off by
    /// default: it changes the signal, and a user with a good interface
    /// should not have that done silently.
    pub mic_autogain: bool,
    /// Target RMS level for automatic gain, in dBFS (-36..-6).
    pub mic_autogain_target_db: i32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            hw_encode: false,
            // On by default: the first thing a first-time sharer asks is
            // "did I grab the right window?".
            show_share_preview: true,
            // Source resolution/rate: the dialog's "Source" option, and
            // the behaviour every release before this one had.
            share_height: 0,
            share_fps: 0,
            echo_cancel: true,
            input_device: String::new(),
            output_device: String::new(),
            mic_autogain: false,
            mic_autogain_target_db: -18,
        }
    }
}

/// The device names the settings page offers. Plain strings because they
/// cross the Flutter bridge; the configured value is the exact name cpal
/// reports, and an empty string means "system default".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

/// Enumerate the default host's devices. A build without `audio-device`
/// has no cpal and returns the same kind of readable error as voice start.
pub fn audio_devices() -> Result<AudioDevices, String> {
    #[cfg(feature = "audio-device")]
    {
        let (inputs, outputs) =
            parley_core::audio::device::list_devices().map_err(|e| e.to_string())?;
        Ok(AudioDevices { inputs, outputs })
    }
    #[cfg(not(feature = "audio-device"))]
    {
        Err("audio-device support is not built in".to_string())
    }
}

/// A consistent snapshot of the audio preferences for one voice start.
/// Read once, on the device thread, so a save cannot change half of the
/// stream setup while it is being built.
#[cfg(feature = "audio-device")]
#[derive(Debug, Clone)]
pub(crate) struct AudioPrefs {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub mic_autogain: bool,
    pub mic_autogain_target_db: i32,
}

#[cfg(feature = "audio-device")]
pub(crate) fn audio_prefs() -> AudioPrefs {
    let settings = CURRENT.read().unwrap();
    let named = |name: &str| {
        let name = name.trim();
        (!name.is_empty()).then(|| name.to_string())
    };
    AudioPrefs {
        input_device: named(&settings.input_device),
        output_device: named(&settings.output_device),
        mic_autogain: settings.mic_autogain,
        mic_autogain_target_db: settings.mic_autogain_target_db.clamp(-36, -6),
    }
}

fn config_dir() -> PathBuf {
    // Windows has neither XDG_CONFIG_HOME nor HOME; %APPDATA% is the
    // per-user root there (same rule as login.rs).
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("APPDATA").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

fn settings_path() -> PathBuf {
    config_dir().join("parley").join("settings.toml")
}

/// The live copy: one read at first use, then every save updates it.
/// Readers on the capture path must never touch the disk.
///
/// The first read also applies the persisted hardware-encode preference:
/// it lives in a process-global in `parley_core`, so a preference saved
/// last session would otherwise be forgotten on every launch.
static CURRENT: LazyLock<RwLock<AppSettings>> = LazyLock::new(|| {
    let settings = read().unwrap_or_default();
    #[cfg(feature = "video")]
    parley_core::video::set_hw_encode(settings.hw_encode);
    RwLock::new(settings)
});

/// The settings as the UI should show them.
pub fn load() -> AppSettings {
    CURRENT.read().unwrap().clone()
}

/// Persist and apply. Atomic write (tmp + rename) like the login file;
/// no `chmod` needed — there is nothing secret here.
pub fn save(settings: AppSettings) -> Result<(), String> {
    write(&settings_path(), &settings)?;
    install(settings);
    Ok(())
}

/// Apply a saved value to the process: the live copy plus the
/// hardware-encode global. Split from [`save`] so the disk half and the
/// in-memory wiring are testable on their own (no `XDG_CONFIG_HOME`
/// juggling in a test binary whose other tests use it).
fn install(settings: AppSettings) {
    #[cfg(feature = "video")]
    let hw = settings.hw_encode;
    // Touch the live copy FIRST: on a first-use `install` that read
    // initializes from the file and applies the file's preference, so
    // setting the global before it would be overwritten by the old value.
    *CURRENT.write().unwrap() = settings;
    #[cfg(feature = "video")]
    parley_core::video::set_hw_encode(hw);
}

/// Interior reads for the hot paths. Gated like their only callers: a
/// build without the capture tap has nothing to ask.
#[cfg(feature = "screen-capture")]
pub(crate) fn show_share_preview() -> bool {
    CURRENT.read().unwrap().show_share_preview
}

#[cfg(feature = "audio-device")]
pub(crate) fn echo_cancel() -> bool {
    CURRENT.read().unwrap().echo_cancel
}

fn read() -> Result<AppSettings, String> {
    read_at(&settings_path())
}

fn read_at(path: &std::path::Path) -> Result<AppSettings, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AppSettings::default()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn write(path: &std::path::Path, settings: &AppSettings) -> Result<(), String> {
    let text = toml::to_string(settings).map_err(|e| format!("encode settings: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("install {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_file_loads_with_defaults() {
        let text = "hw_encode = true\n";
        let parsed: AppSettings = toml::from_str(text).expect("partial settings");
        assert!(parsed.hw_encode);
        assert_eq!(parsed.share_height, AppSettings::default().share_height);
        assert!(parsed.show_share_preview, "default keeps the preview on");
        assert!(!parsed.mic_autogain, "autogain stays opt-in");
        assert_eq!(parsed.mic_autogain_target_db, -18);
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("parley-settings-{}", std::process::id()));
        let path = dir.join("parley").join("settings.toml");
        let settings = AppSettings {
            hw_encode: true,
            show_share_preview: false,
            share_height: 720,
            share_fps: 30,
            echo_cancel: false,
            input_device: "Studio Mic".to_string(),
            output_device: "Headphones".to_string(),
            mic_autogain: true,
            mic_autogain_target_db: -20,
        };
        write(&path, &settings).expect("write");
        assert_eq!(read_at(&path).expect("read"), settings);

        // The file the app actually uses lives beside the login and the
        // TOFU pins.
        assert!(
            settings_path().ends_with("parley/settings.toml"),
            "unexpected path: {}",
            settings_path().display()
        );

        // A corrupt file is an error the caller falls back from, not a
        // startup failure.
        std::fs::write(&path, "not = [valid").unwrap();
        assert!(read_at(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wiring a running app reads: `save` → [`install`] → the hot
    /// paths. No disk, no environment.
    #[test]
    fn install_applies_to_the_live_copy() {
        let settings = AppSettings {
            hw_encode: true,
            show_share_preview: false,
            share_height: 1080,
            share_fps: 60,
            echo_cancel: false,
            input_device: String::new(),
            output_device: String::new(),
            mic_autogain: true,
            mic_autogain_target_db: -22,
        };
        install(settings.clone());
        assert_eq!(load(), settings);
        #[cfg(feature = "screen-capture")]
        assert!(!show_share_preview());
        #[cfg(feature = "audio-device")]
        assert!(!echo_cancel());
        #[cfg(feature = "audio-device")]
        {
            let audio = audio_prefs();
            assert_eq!(audio.input_device, None);
            assert_eq!(audio.output_device, None);
            assert!(audio.mic_autogain);
            assert_eq!(audio.mic_autogain_target_db, -22);
        }
        // `install` is the only writer of the process-global preference,
        // so this is what the next share depends on.
        #[cfg(feature = "video")]
        assert!(
            parley_core::video::hw_encode_enabled(),
            "saving hardware encode must apply it"
        );
        // Leave the process on the defaults, so this test cannot colour a
        // later one that reads the live copy.
        install(AppSettings::default());
    }
}
