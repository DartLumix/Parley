//! Bulk transfers: chat attachments and personal sounds.
//!
//! One bidirectional QUIC stream per transfer (see `proto/blob.proto`).
//! The control stream stays free, and QUIC's per-stream flow control
//! paces the sender without a large in-memory buffer on either side.
//!
//! The uploader is already authenticated: the accept loop is spawned
//! from `serve` and carries the session's user id, never a client claim.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use quinn::{RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn};

use crate::db::DbHandle;
use crate::error::Error;
use crate::session::SessionRegistry;
use parley_core::session;
use parley_proto::{
    BlobDownload, BlobKind, BlobReply, BlobRequest, BlobUpload, ChatMessage, ServerBody,
    ServerMessage,
};

/// Chat attachments: large enough for a video clip, small enough that a
/// single upload cannot fill the VPS disk by itself.
pub const MAX_CHAT_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// Personal cues are short MP3s. 1 MiB is over three minutes at 40 kbps.
pub const MAX_SOUND_BYTES: u64 = 1024 * 1024;

/// Distinguishes uploads that begin in the same clock tick. The temp name
/// used to carry only the user id and a nanosecond timestamp, so two
/// simultaneous streams from one account could pick the same path and one
/// could delete the other's file.
static UPLOAD_SEQ: AtomicU64 = AtomicU64::new(0);

/// Spawn the per-connection accept loop. Every accepted stream is its
/// own task; the loop ends when the connection does.
pub fn spawn_acceptor(
    conn: quinn::Connection,
    user_id: u32,
    db: DbHandle,
    registry: Arc<SessionRegistry>,
    files_dir: PathBuf,
) {
    tokio::spawn(async move {
        while let Ok((tx, rx)) = conn.accept_bi().await {
            let db = db.clone();
            let registry = registry.clone();
            let files_dir = files_dir.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(tx, rx, user_id, &db, &registry, &files_dir).await {
                    warn!(user_id, error = %e, "blob transfer failed");
                }
            });
        }
    });
}

/// Remove upload temp files a crash left behind. Called at startup,
/// before any transfer can be in flight, so only the private prefix is
/// touched.
pub fn sweep_temp_files(files_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(files_dir) else {
        return;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".incoming-")
        {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    if removed > 0 {
        info!(removed, "removed interrupted uploads");
    }
}

async fn handle(
    mut tx: SendStream,
    mut rx: RecvStream,
    user_id: u32,
    db: &DbHandle,
    registry: &Arc<SessionRegistry>,
    files_dir: &Path,
) -> Result<(), Error> {
    let request: BlobRequest = session::read_message(&mut rx).await?;
    match request.body {
        Some(parley_proto::pb::blob_request::Body::Upload(upload)) => {
            let reply = upload_blob(rx, user_id, upload, db, registry, files_dir).await?;
            session::write_message(&mut tx, &reply).await?;
            let _ = tx.finish();
        }
        Some(parley_proto::pb::blob_request::Body::Download(download)) => {
            download_blob(tx, download, user_id, db, files_dir).await?;
        }
        None => {
            session::write_message(
                &mut tx,
                &BlobReply {
                    ok: false,
                    error: "empty blob request".into(),
                    ..BlobReply::default()
                },
            )
            .await?;
            let _ = tx.finish();
        }
    }
    Ok(())
}

async fn upload_blob(
    mut rx: RecvStream,
    user_id: u32,
    upload: BlobUpload,
    db: &DbHandle,
    registry: &Arc<SessionRegistry>,
    files_dir: &Path,
) -> Result<BlobReply, Error> {
    let kind = BlobKind::try_from(upload.kind).unwrap_or(BlobKind::ChatFile);
    match kind {
        BlobKind::ChatFile => {
            if upload.size > MAX_CHAT_FILE_BYTES {
                return Ok(reply_err("file is larger than 100 MB"));
            }
            if !db.channel_exists(upload.channel_id).await? {
                return Ok(reply_err("channel does not exist"));
            }
            let name = clean_name(&upload.file_name);
            if name.is_empty() {
                return Ok(reply_err("file name is empty"));
            }
            let caption: String = upload.caption.trim().chars().take(2000).collect();

            // Write to a private temp name first: an aborted transfer must
            // never leave a half file where a download can see it.
            tokio::fs::create_dir_all(files_dir).await?;
            let unique = UPLOAD_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = files_dir.join(format!(
                ".incoming-{user_id}-{}-{unique}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let stored = match copy_to_file(&mut rx, &tmp, upload.size).await {
                Ok(n) => n,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Ok(reply_err(&e));
                }
            };

            let (message_id, ts, file_id) = db
                .insert_file_message(
                    upload.channel_id,
                    user_id,
                    &caption,
                    &name,
                    &clean_mime(&upload.mime),
                    stored,
                )
                .await?;

            // Keep the bytes next to the metadata. A failure here makes
            // the row dead weight; remove it rather than advertise a
            // file nobody can fetch.
            let final_path = file_path(files_dir, file_id);
            if let Some(parent) = final_path.parent()
                && let Err(e) = tokio::fs::create_dir_all(parent).await
            {
                warn!(error = %e, "attachment directory");
            }
            if let Err(e) = tokio::fs::rename(&tmp, &final_path).await {
                warn!(error = %e, "attachment rename");
                let _ = tokio::fs::remove_file(&tmp).await;
                // The row is already committed. Remove it rather than
                // leave a chat message whose attachment can never be
                // downloaded; the file row is swept as an orphan.
                if let Err(e) = db.delete_message(message_id).await {
                    warn!(message_id, error = %e, "attachment rollback");
                }
                return Ok(reply_err("could not store the file"));
            }

            let username = registry.username_of(user_id).unwrap_or_default();
            info!(
                user_id,
                file_id,
                size = stored,
                channel_id = upload.channel_id,
                "chat attachment stored"
            );
            registry.broadcast(ServerMessage {
                body: Some(ServerBody::ChatMessage(ChatMessage {
                    message_id,
                    channel_id: upload.channel_id,
                    user_id,
                    username,
                    text: caption,
                    timestamp_ms: ts,
                    pinned: false,
                    attachment_id: file_id,
                    attachment_name: name.clone(),
                    attachment_size: stored,
                    attachment_mime: clean_mime(&upload.mime),
                })),
            });
            // The request half of the stream is done; the caller writes
            // the reply and finishes.
            Ok(BlobReply {
                ok: true,
                file_id,
                size: stored,
                file_name: name,
                mime: clean_mime(&upload.mime),
                ..BlobReply::default()
            })
        }
        BlobKind::SoundJoin | BlobKind::SoundLeave | BlobKind::SoundAfk => {
            if upload.size == 0 {
                // Empty upload = remove this sound.
                let version = db.set_sound(user_id, kind as u32, None, "").await?;
                let versions = db.sound_versions(user_id).await?;
                registry.update_sound_versions(user_id, versions);
                registry.broadcast(presence(registry));
                return Ok(BlobReply {
                    ok: true,
                    sound_version: version,
                    ..BlobReply::default()
                });
            }
            if upload.size > MAX_SOUND_BYTES {
                return Ok(reply_err("sound is larger than 1 MB"));
            }
            let mut bytes = Vec::with_capacity(upload.size as usize);
            let mut left = upload.size as usize;
            let mut buf = vec![0u8; 64 * 1024];
            while left > 0 {
                let want = buf.len().min(left);
                let read = rx
                    .read(&mut buf[..want])
                    .await?
                    .ok_or_else(|| Error::Internal("sound upload ended early".into()))?;
                bytes.extend_from_slice(&buf[..read]);
                left -= read;
            }
            if !is_mp3(&bytes) {
                return Ok(reply_err("not an MP3 file"));
            }
            let version = db
                .set_sound(user_id, kind as u32, Some(&bytes), "audio/mpeg")
                .await?;
            let versions = db.sound_versions(user_id).await?;
            registry.update_sound_versions(user_id, versions);
            registry.broadcast(presence(registry));
            info!(
                user_id,
                kind = kind as u32,
                version,
                "personal sound stored"
            );
            Ok(BlobReply {
                ok: true,
                size: bytes.len() as u64,
                sound_version: version,
                ..BlobReply::default()
            })
        }
    }
}

async fn download_blob(
    mut tx: SendStream,
    download: BlobDownload,
    _user_id: u32,
    db: &DbHandle,
    files_dir: &Path,
) -> Result<(), Error> {
    match download.target {
        Some(parley_proto::pb::blob_download::Target::FileId(file_id)) => {
            let Some(file) = db.file(file_id).await? else {
                session::write_message(&mut tx, &reply_err("file not found")).await?;
                let _ = tx.finish();
                return Ok(());
            };
            let path = file_path(files_dir, file.id);
            let mut handle = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) => {
                    warn!(file_id, error = %e, "attachment missing from disk");
                    session::write_message(&mut tx, &reply_err("file is gone")).await?;
                    let _ = tx.finish();
                    return Ok(());
                }
            };
            session::write_message(
                &mut tx,
                &BlobReply {
                    ok: true,
                    file_id: file.id,
                    size: file.size,
                    file_name: file.name,
                    mime: file.mime,
                    ..BlobReply::default()
                },
            )
            .await?;
            stream_file(&mut tx, &mut handle, file.size).await?;
            let _ = tx.finish();
        }
        Some(parley_proto::pb::blob_download::Target::Sound(sound)) => {
            let kind = BlobKind::try_from(sound.kind).unwrap_or(BlobKind::SoundJoin) as u32;
            let Some(row) = db.sound(sound.user_id, kind).await? else {
                session::write_message(&mut tx, &reply_err("sound not found")).await?;
                let _ = tx.finish();
                return Ok(());
            };
            session::write_message(
                &mut tx,
                &BlobReply {
                    ok: true,
                    size: row.bytes.len() as u64,
                    mime: row.mime,
                    sound_version: row.version,
                    ..BlobReply::default()
                },
            )
            .await?;
            tx.write_all(&row.bytes).await?;
            let _ = tx.finish();
        }
        None => {
            session::write_message(&mut tx, &reply_err("nothing to download")).await?;
            let _ = tx.finish();
        }
    }
    Ok(())
}

/// Copy exactly `size` bytes from the stream to `path`, returning the
/// count. Bounded by the caller's cap; an early EOF is an error.
async fn copy_to_file(rx: &mut RecvStream, path: &Path, size: u64) -> Result<u64, String> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| format!("could not create the file: {e}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = size;
    while left > 0 {
        let want = buf.len().min(left as usize);
        let read = rx
            .read(&mut buf[..want])
            .await
            .map_err(|e| format!("upload stalled: {e}"))?
            .ok_or_else(|| "upload ended early".to_string())?;
        file.write_all(&buf[..read])
            .await
            .map_err(|e| format!("could not write the file: {e}"))?;
        left -= read as u64;
    }
    file.flush()
        .await
        .map_err(|e| format!("could not finish the file: {e}"))?;
    Ok(size)
}

async fn stream_file(
    tx: &mut SendStream,
    file: &mut tokio::fs::File,
    size: u64,
) -> Result<(), Error> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = size;
    while left > 0 {
        let want = buf.len().min(left as usize);
        let read = file.read(&mut buf[..want]).await?;
        if read == 0 {
            break;
        }
        tx.write_all(&buf[..read]).await?;
        left -= read as u64;
    }
    Ok(())
}

/// Final on-disk location: sharded by the id's first byte so one
/// directory never grows into a problem for the filesystem.
pub fn file_path(files_dir: &Path, file_id: u64) -> PathBuf {
    let hex = format!("{file_id:016x}");
    files_dir.join(&hex[..2]).join(hex)
}

/// A file name safe to store and show: no separators, no control
/// characters, bounded length. Paths are never built from it.
fn clean_name(name: &str) -> String {
    name.trim()
        .replace(['/', '\\'], "-")
        .chars()
        .filter(|c| !c.is_control())
        .take(180)
        .collect()
}

fn clean_mime(mime: &str) -> String {
    mime.trim()
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(120)
        .collect()
}

fn reply_err(error: &str) -> BlobReply {
    BlobReply {
        ok: false,
        error: error.to_string(),
        ..BlobReply::default()
    }
}

fn is_mp3(bytes: &[u8]) -> bool {
    if bytes.len() < 4 {
        return false;
    }
    // A leading ID3v2 tag is conclusive. (The old check stopped here, which
    // is why a bare stream with padding, an APE tag or another leading
    // container could look like "not an MP3" even though every decoder
    // played it.)
    if bytes.starts_with(b"ID3") {
        return true;
    }
    // Otherwise find a real MPEG audio frame in the opening bytes. This is
    // deliberately a scan, not a first-two-bytes check: tag writers and
    // encoders leave all sorts of small prefixes in front of the first
    // frame. 64 KiB is far past any legitimate leading metadata and cheap
    // for a 1 MiB cue.
    bytes.windows(4).take(64 * 1024).any(is_mpeg_frame_header)
}

/// Validate the four-byte MPEG audio frame header, not just the sync word.
/// The sync alone also matches AAC/ADTS and random bytes; the version,
/// layer, bitrate and sample-rate fields are what make it an MP3 frame.
fn is_mpeg_frame_header(h: &[u8]) -> bool {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return false;
    }
    // Version 01 is reserved; layer 00 is reserved.
    if (h[1] >> 3) & 0x03 == 0x01 || (h[1] >> 1) & 0x03 == 0 {
        return false;
    }
    let bitrate = h[2] >> 4;
    let sample_rate = (h[2] >> 2) & 0x03;
    bitrate != 0 && bitrate != 0x0F && sample_rate != 0x03
}

fn presence(registry: &Arc<SessionRegistry>) -> ServerMessage {
    crate::server::presence_message(registry)
}

#[cfg(test)]
mod tests {
    use super::{is_mp3, sweep_temp_files};

    #[test]
    fn accepts_id3_and_bare_frames() {
        assert!(is_mp3(b"ID3\x04\x00\x00\x00\x00\x00\x00"));
        assert!(is_mp3(&[0xFF, 0xFB, 0x90, 0x64]));
    }

    #[test]
    fn accepts_a_frame_after_small_leading_junk() {
        // Some taggers leave a short pad before the first frame. The old
        // first-byte check rejected this even though a decoder plays it.
        let mut bytes = b"\0\0junk".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        assert!(is_mp3(&bytes));
    }

    #[test]
    fn rejects_non_mpeg_data() {
        assert!(!is_mp3(b"not audio at all"));
        assert!(!is_mp3(b"fLaC\0\0\0\x22"));
    }

    #[test]
    fn sweeps_only_interrupted_upload_temps() {
        let dir = std::env::temp_dir().join(format!("parley-blob-tmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".incoming-1-2-3-4"), b"partial").unwrap();
        std::fs::write(dir.join("keep.txt"), b"final").unwrap();

        sweep_temp_files(&dir);

        assert!(!dir.join(".incoming-1-2-3-4").exists());
        assert!(dir.join("keep.txt").exists(), "only temp files are swept");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
