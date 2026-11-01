//! Chat attachments: upload a picked file and download one from history.
//!
//! The transfer rides a fresh QUIC stream (proto/blob.proto), so the
//! session's control stream keeps carrying presence and chat while a
//! 100 MB file is in flight. Both functions block on the bridge's
//! runtime, which is what an FRB worker thread is for.

use std::path::Path;

use parley_core::session as wire;
use parley_proto::{BlobDownload, BlobKind, BlobReply, BlobRequest, BlobUpload};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};

use crate::api::session;

/// Upload one file into a channel's chat. `caption` may be empty.
/// Returns the new attachment id as a string (Dart keeps it for display;
/// the chat echo does the actual rendering).
pub fn upload_chat_file(
    session_id: u32,
    channel_id: u64,
    path: String,
    caption: String,
) -> Result<u64, String> {
    let file = Path::new(&path);
    let meta =
        std::fs::metadata(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    if !meta.is_file() {
        return Err("that is not a file".into());
    }
    let size = meta.len();
    if size == 0 {
        return Err("the file is empty".into());
    }
    if size > 100 * 1024 * 1024 {
        return Err("the file is larger than 100 MB".into());
    }
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let mime = guess_mime(&name).to_string();
    let conn = session::connection(session_id)?;

    session::rt().block_on(async move {
        let (mut tx, mut rx) = conn.open_bi().await.map_err(|e| format!("open: {e}"))?;
        wire::write_message(
            &mut tx,
            &BlobRequest {
                body: Some(parley_proto::pb::blob_request::Body::Upload(BlobUpload {
                    kind: BlobKind::ChatFile as i32,
                    channel_id,
                    file_name: name,
                    mime,
                    size,
                    caption,
                })),
            },
        )
        .await
        .map_err(|e| format!("request: {e}"))?;

        let mut file = tokio::fs::File::open(file)
            .await
            .map_err(|e| format!("open file: {e}"))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut left = size;
        while left > 0 {
            let want = buf.len().min(left as usize);
            let read = file
                .read(&mut buf[..want])
                .await
                .map_err(|e| format!("read file: {e}"))?;
            if read == 0 {
                return Err("the file ended during upload".into());
            }
            // A server that predates blob streams accepts the connection
            // but never reads this one. Bound the stalled write so the
            // click fails with a sentence instead of hanging forever.
            timeout(Duration::from_secs(20), tx.write_all(&buf[..read]))
                .await
                .map_err(|_| {
                    "upload stalled — the server may be older than this client".to_string()
                })?
                .map_err(|e| format!("upload: {e}"))?;
            left -= read as u64;
        }
        let _ = tx.finish();

        let reply: BlobReply = timeout(Duration::from_secs(20), wire::read_message(&mut rx))
            .await
            .map_err(|_| "the server did not answer (is it older than this client?)".to_string())?
            .map_err(|e| format!("reply: {e}"))?;
        if !reply.ok {
            return Err(if reply.error.is_empty() {
                "the server refused the file".into()
            } else {
                reply.error
            });
        }
        Ok(reply.file_id)
    })
}

/// Download one attachment to `path` (chosen by the user). Returns the
/// number of bytes written. Overwrites an existing file.
pub fn download_chat_file(session_id: u32, file_id: u64, path: String) -> Result<u64, String> {
    let conn = session::connection(session_id)?;
    session::rt().block_on(async move {
        let (mut tx, mut rx) = conn.open_bi().await.map_err(|e| format!("open: {e}"))?;
        wire::write_message(
            &mut tx,
            &BlobRequest {
                body: Some(parley_proto::pb::blob_request::Body::Download(
                    BlobDownload {
                        target: Some(parley_proto::pb::blob_download::Target::FileId(file_id)),
                    },
                )),
            },
        )
        .await
        .map_err(|e| format!("request: {e}"))?;
        let _ = tx.finish();
        let reply: BlobReply = timeout(Duration::from_secs(20), wire::read_message(&mut rx))
            .await
            .map_err(|_| "the server did not answer (is it older than this client?)".to_string())?
            .map_err(|e| format!("reply: {e}"))?;
        if !reply.ok {
            return Err(if reply.error.is_empty() {
                "the server refused the download".into()
            } else {
                reply.error
            });
        }

        let target = Path::new(&path);
        if let Some(parent) = target.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
        }
        let mut out = tokio::fs::File::create(target)
            .await
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut left = reply.size;
        while left > 0 {
            let want = buf.len().min(left as usize);
            let read = rx
                .read(&mut buf[..want])
                .await
                .map_err(|e| format!("download: {e}"))?
                .ok_or_else(|| "the download ended early".to_string())?;
            out.write_all(&buf[..read])
                .await
                .map_err(|e| format!("write: {e}"))?;
            left -= read as u64;
        }
        out.flush().await.map_err(|e| format!("finish: {e}"))?;
        Ok(reply.size)
    })
}

/// The shared blob-stream opener used by the soundboard as well.
pub(crate) async fn open_blob(
    conn: &quinn::Connection,
    request: BlobRequest,
) -> Result<(quinn::SendStream, quinn::RecvStream), String> {
    let (mut tx, rx) = conn.open_bi().await.map_err(|e| format!("open: {e}"))?;
    wire::write_message(&mut tx, &request)
        .await
        .map_err(|e| format!("request: {e}"))?;
    Ok((tx, rx))
}

/// A small extension table: enough to give the receiver a sensible icon
/// and the server a stored MIME. Unknown types are octets.
pub(crate) fn guess_mime(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}
