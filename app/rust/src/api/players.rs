//! The pre-share audio picker's data source: which applications are
//! playing right now, so a share can carry exactly those and never this
//! app's own voice.
//!
//! Plain types only — nothing here may need a Dart wrapper (the bridge
//! rule in voice.rs).

/// One selectable application, as the share dialog shows it.
#[derive(Debug, Clone)]
pub struct ShareAudioPlayer {
    /// Capture target, handed back to `start_screen_share` unchanged.
    /// Linux: the PipeWire global id of the app's playback node
    /// (unique per stream). Windows: the process id of the audio
    /// session's owner (unique per process, which is the unit WASAPI
    /// process loopback captures).
    pub id: u32,
    /// Application name (the picker's first line).
    pub app: String,
    /// Stream description when the app sets one (second line).
    pub description: String,
}

/// Snapshot of the applications playing right now.
///
/// Empty — never an error — when this build has no screen-capture
/// feature or the platform audio graph cannot be reached: the dialog
/// then offers no per-app audio instead of failing the share. The list
/// is a snapshot, so a player that starts after the dialog opened is
/// simply not in it. Parley's own playback is never listed: capturing
/// it is the echo this feature exists to avoid.
#[cfg(feature = "screen-capture")]
pub fn list_share_audio_players() -> Vec<ShareAudioPlayer> {
    match parley_client::screen::list_audio_players() {
        Ok(players) => players
            .into_iter()
            .map(|p| ShareAudioPlayer {
                id: p.id,
                app: p.app,
                description: p.description,
            })
            .collect(),
        Err(e) => {
            eprintln!("share-audio: player list failed: {e}");
            Vec::new()
        }
    }
}

#[cfg(not(feature = "screen-capture"))]
pub fn list_share_audio_players() -> Vec<ShareAudioPlayer> {
    Vec::new()
}
