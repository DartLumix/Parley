//! Personal join/leave/AFK cues.
//!
//! The source of truth is a folder next to the app binary:
//!
//!     <app dir>/sounds/join.mp3
//!     <app dir>/sounds/leave.mp3
//!     <app dir>/sounds/afk.mp3
//!
//! The profile page's Soundboard section uploads the enabled files to
//! the server, which stores them per account and advertises a revision
//! in presence. Every connected client downloads a revision once into
//! its cache, then plays it locally when that user joins, leaves or
//! goes AFK — the audio is never mixed into the voice stream. Missing
//! files, malformed MP3s or a failed download simply leave the default
//! chime (or silence, for AFK) in place.

// The `frb(ignore)` marker expands to a cfg the compiler does not know
// about; the codegen still reads it, hence the module-wide allow.
#![allow(unexpected_cfgs)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use parley_core::session as wire;
use parley_proto::{BlobDownload, BlobKind, BlobReply, BlobRequest, BlobUpload, SoundRef};
use serde::{Deserialize, Serialize};
use tokio::time::{timeout, Duration};

/// 0 = join, 1 = leave, 2 = afk. Matches the BlobKind values - 1.
pub(crate) const KINDS: [&str; 3] = ["join", "leave", "afk"];

#[flutter_rust_bridge::frb(ignore)]
fn kind_index(name: &str) -> Option<usize> {
    match name {
        "join" => Some(0),
        "leave" => Some(1),
        "afk" => Some(2),
        _ => None,
    }
}

#[flutter_rust_bridge::frb(ignore)]
fn blob_kind(index: usize) -> BlobKind {
    match index {
        0 => BlobKind::SoundJoin,
        1 => BlobKind::SoundLeave,
        _ => BlobKind::SoundAfk,
    }
}

/// Which toggles the user last applied. Lives beside the files so it
/// travels with a copied sounds folder. Internal to the bridge: none of
/// its type crosses to Dart.
#[flutter_rust_bridge::frb(ignore)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Prefs {
    #[serde(default)]
    join: bool,
    #[serde(default)]
    leave: bool,
    #[serde(default)]
    afk: bool,
}

/// Where the files live: next to the executable (the Flutter bundle
/// root, and the per-user install), falling back to the working
/// directory for `flutter run`, where the executable is buried under
/// `build/`.
pub(crate) fn sounds_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join("sounds");
            if beside.is_dir() {
                return beside;
            }
            if let Ok(cwd) = std::env::current_dir() {
                let local = cwd.join("sounds");
                if local.is_dir() {
                    return local;
                }
            }
            // Neither exists yet: the app directory is the right place
            // to create it, and the profile page tells the user where.
            return beside;
        }
    }
    PathBuf::from("sounds")
}

#[flutter_rust_bridge::frb(ignore)]
fn prefs_path() -> PathBuf {
    sounds_dir().join("board.toml")
}

#[flutter_rust_bridge::frb(ignore)]
fn read_prefs() -> Prefs {
    std::fs::read_to_string(prefs_path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

#[flutter_rust_bridge::frb(ignore)]
fn write_prefs(prefs: &Prefs) -> Result<(), String> {
    let dir = sounds_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let text = toml::to_string_pretty(prefs).map_err(|e| e.to_string())?;
    std::fs::write(prefs_path(), text)
        .map_err(|e| format!("cannot write {}: {e}", prefs_path().display()))
}

#[flutter_rust_bridge::frb(ignore)]
fn local_file(index: usize) -> PathBuf {
    sounds_dir().join(format!("{}.mp3", KINDS[index]))
}

/// What the profile page renders: which files exist and which toggles
/// were last applied. The directory is shown verbatim so there is no
/// guessing where to drop the MP3s.
#[derive(Debug, Clone)]
pub struct SoundboardStatus {
    pub dir: String,
    pub join_file: bool,
    pub leave_file: bool,
    pub afk_file: bool,
    pub join_enabled: bool,
    pub leave_enabled: bool,
    pub afk_enabled: bool,
}

pub fn soundboard_status() -> SoundboardStatus {
    // Make the drop folder exist before the page names it, so the
    // "missing" hint is actionable rather than pointing at nothing.
    let _ = std::fs::create_dir_all(sounds_dir());
    let prefs = read_prefs();
    SoundboardStatus {
        dir: sounds_dir().display().to_string(),
        join_file: local_file(0).is_file(),
        leave_file: local_file(1).is_file(),
        afk_file: local_file(2).is_file(),
        join_enabled: prefs.join,
        leave_enabled: prefs.leave,
        afk_enabled: prefs.afk,
    }
}

/// Upload the enabled files (and remove the disabled ones) for this
/// account, then remember the toggles locally. All-or-nothing on the
/// local file checks: a toggle whose MP3 is missing is a form error,
/// not a silent downgrade.
pub fn apply_soundboard(session_id: u32, join: bool, leave: bool, afk: bool) -> Result<(), String> {
    let wanted = [join, leave, afk];
    let mut payloads: [Option<Vec<u8>>; 3] = [None, None, None];
    for (index, on) in wanted.iter().enumerate() {
        if !on {
            continue;
        }
        let path = local_file(index);
        let bytes = std::fs::read(&path).map_err(|_| {
            format!(
                "{} not found — put an MP3 named {}.mp3 in {}",
                path.display(),
                KINDS[index],
                sounds_dir().display()
            )
        })?;
        if bytes.len() > 1024 * 1024 {
            return Err(format!("{}.mp3 is larger than 1 MB", KINDS[index]));
        }
        payloads[index] = Some(bytes);
    }

    let conn = crate::api::session::connection(session_id)?;
    crate::api::session::rt().block_on(async {
        for (index, enabled) in wanted.iter().enumerate() {
            let (mut tx, mut rx) = crate::api::blob::open_blob(
                &conn,
                BlobRequest {
                    body: Some(parley_proto::pb::blob_request::Body::Upload(BlobUpload {
                        kind: blob_kind(index) as i32,
                        channel_id: 0,
                        file_name: String::new(),
                        mime: if *enabled {
                            "audio/mpeg".into()
                        } else {
                            String::new()
                        },
                        size: payloads[index]
                            .as_ref()
                            .map(|b| b.len() as u64)
                            .unwrap_or(0),
                        caption: String::new(),
                    })),
                },
            )
            .await?;
            if let Some(bytes) = &payloads[index] {
                tx.write_all(bytes)
                    .await
                    .map_err(|e| format!("upload: {e}"))?;
            }
            let _ = tx.finish();
            let reply: BlobReply = timeout(Duration::from_secs(20), wire::read_message(&mut rx))
                .await
                .map_err(|_| {
                    "the server did not answer (is it older than this client?)".to_string()
                })?
                .map_err(|e| format!("reply: {e}"))?;
            if !reply.ok {
                return Err(if reply.error.is_empty() {
                    format!("the server refused the {} sound", KINDS[index])
                } else {
                    reply.error
                });
            }
        }
        Ok(())
    })?;

    write_prefs(&Prefs { join, leave, afk })
}

// ---- cache: one copy per (user, kind, revision) ----

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("parley").join("sounds")
}

fn cache_path(user_id: u32, index: usize, version: u64) -> PathBuf {
    cache_dir().join(format!("{user_id}-{}-{version}.mp3", KINDS[index]))
}

/// (join, leave, afk) revisions for one account.
type Versions = (u64, u64, u64);

/// Latest revisions seen in presence, so a join event can find the file
/// without another round trip.
static VERSIONS: LazyLock<Mutex<HashMap<u32, Versions>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Revision downloads already running, keyed by (user, kind, version).
static IN_FLIGHT: LazyLock<Mutex<HashSet<(u32, usize, u64)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Called on every presence broadcast. Downloads any revision this
/// machine has not cached yet, on the session's runtime. Cheap when
/// nothing changed — the common case for a 5-second poll.
pub(crate) fn observe_presence(session_id: u32, users: &[(u32, Versions)]) {
    {
        let mut versions = VERSIONS.lock().unwrap();
        for (id, v) in users {
            versions.insert(*id, *v);
        }
    }
    let Ok(conn) = crate::api::session::connection(session_id) else {
        return;
    };
    for (user_id, versions) in users {
        for (index, version) in [versions.0, versions.1, versions.2].into_iter().enumerate() {
            if version == 0 {
                continue;
            }
            let path = cache_path(*user_id, index, version);
            if path.is_file() {
                continue;
            }
            if !IN_FLIGHT.lock().unwrap().insert((*user_id, index, version)) {
                continue;
            }
            let conn = conn.clone();
            let user_id = *user_id;
            let path = path.clone();
            crate::api::session::rt().spawn(async move {
                let result = download_sound(conn, user_id, index, version, &path).await;
                IN_FLIGHT.lock().unwrap().remove(&(user_id, index, version));
                if let Err(e) = result {
                    eprintln!("soundboard: {e}");
                }
            });
        }
    }
}

async fn download_sound(
    conn: quinn::Connection,
    user_id: u32,
    index: usize,
    version: u64,
    path: &Path,
) -> Result<(), String> {
    let (mut tx, mut rx) = crate::api::blob::open_blob(
        &conn,
        BlobRequest {
            body: Some(parley_proto::pb::blob_request::Body::Download(
                BlobDownload {
                    target: Some(parley_proto::pb::blob_download::Target::Sound(SoundRef {
                        kind: blob_kind(index) as i32,
                        user_id,
                    })),
                },
            )),
        },
    )
    .await?;
    let _ = tx.finish();
    let reply: BlobReply = timeout(Duration::from_secs(20), wire::read_message(&mut rx))
        .await
        .map_err(|_| "the server did not answer (is it older than this client?)".to_string())?
        .map_err(|e| format!("sound reply: {e}"))?;
    if !reply.ok {
        return Err(reply.error);
    }
    if reply.sound_version != version {
        // Presence moved while we asked; the next broadcast retries.
        return Ok(());
    }
    let mut bytes = vec![0u8; reply.size as usize];
    rx.read_exact(&mut bytes)
        .await
        .map_err(|e| format!("sound bytes: {e}"))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("cache dir: {e}"))?;
    }
    // Publish only a complete file: a prefetch and an on-demand fetch
    // can race for the same revision, and a player must never read a
    // half-written cue.
    let tmp = path.with_extension("part");
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(|e| format!("cache write: {e}"))?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| format!("cache publish: {e}"))?;
    Ok(())
}

/// Play one user's custom cue locally. Returns false when there is no
/// custom sound (the caller then uses the default chime), or when the
/// output is muted — an alert is not supposed to override mute.
pub fn play_personal_sound(session_id: u32, user_id: u32, kind: String) -> bool {
    let Some(index) = kind_index(&kind) else {
        return false;
    };
    let version = {
        let versions = VERSIONS.lock().unwrap();
        let Some(v) = versions.get(&user_id) else {
            return false;
        };
        [v.0, v.1, v.2][index]
    };
    if version == 0 {
        return false;
    }
    let path = cache_path(user_id, index, version);
    if !path.is_file() {
        // The prefetch runs on presence, which may have arrived before
        // the session was registered (the disconnect test does exactly
        // that) or been missed. One blocking fetch on this FRB worker
        // thread covers the first cue; later ones hit the cache.
        let Ok(conn) = crate::api::session::connection(session_id) else {
            return false;
        };
        if crate::api::session::rt()
            .block_on(download_sound(conn, user_id, index, version, &path))
            .is_err()
        {
            return false;
        }
    }
    #[cfg(feature = "audio-device")]
    {
        let samples = match load_mp3(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("soundboard: {e}");
                return false;
            }
        };
        crate::api::voice::play_custom_cue(session_id, samples)
    }
    #[cfg(not(feature = "audio-device"))]
    {
        let _ = (path, session_id);
        false
    }
}

/// Decode an MP3 to 48 kHz mono. Cues are short; a 20-second ceiling
/// keeps a mislabelled podcast from stalling the UI thread.
#[cfg(feature = "audio-device")]
fn load_mp3(path: &Path) -> Result<Vec<f32>, String> {
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    const TARGET_RATE: u32 = 48_000;
    const MAX_SECONDS: usize = 20;

    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("probe: {e}"))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "no audio track".to_string())?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate.unwrap_or(TARGET_RATE).max(1);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let mut mono: Vec<f32> = Vec::new();
    let cap = TARGET_RATE as usize * MAX_SECONDS;
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(_) => break,
        };
        append_mono(&decoded, &mut mono);
        if mono.len() >= cap {
            break;
        }
    }
    if mono.is_empty() {
        return Err("the MP3 has no audio".into());
    }
    mono.truncate(cap);
    Ok(resample(&mono, rate, TARGET_RATE))
}

#[cfg(feature = "audio-device")]
fn append_mono(buffer: &symphonia::core::audio::AudioBufferRef<'_>, out: &mut Vec<f32>) {
    use symphonia::core::audio::{AudioBufferRef, Signal};
    let channels = buffer.spec().channels.count().max(1);
    let frames = buffer.frames();
    macro_rules! mix {
        ($b:expr) => {{
            for frame in 0..frames {
                let mut sum = 0.0f32;
                for ch in 0..channels {
                    sum += $b.chan(ch)[frame] as f32;
                }
                out.push(sum / channels as f32);
            }
        }};
    }
    match buffer {
        AudioBufferRef::F32(b) => mix!(b),
        AudioBufferRef::F64(b) => mix!(b),
        AudioBufferRef::S16(b) => mix!(b),
        AudioBufferRef::S32(b) => mix!(b),
        AudioBufferRef::U8(b) => mix!(b),
        // Symphonia's MP3 decoder is F32; anything else is rare and can
        // be skipped rather than guessed at.
        _ => {}
    }
}

/// Linear resampling. Short, non-musical cues do not justify a windowed
/// sinc; the output rate is always the device rate (48 kHz).
#[cfg(feature = "audio-device")]
fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_len = (input.len() as f64 / ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let left = pos.floor() as usize;
        let frac = (pos - left as f64) as f32;
        let a = input.get(left).copied().unwrap_or(0.0);
        let b = input.get(left + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_roundtrip() {
        for (index, name) in KINDS.iter().enumerate() {
            assert_eq!(kind_index(name), Some(index));
            assert_eq!(blob_kind(index) as i32, index as i32 + 1);
        }
        assert_eq!(kind_index("poke"), None);
    }

    #[test]
    fn the_cache_path_is_revision_scoped() {
        let a = cache_path(7, 0, 3);
        let b = cache_path(7, 0, 4);
        assert_ne!(a, b, "a new revision must not overwrite the old file");
        assert!(a.to_string_lossy().contains("7-join-3"));
    }
}
