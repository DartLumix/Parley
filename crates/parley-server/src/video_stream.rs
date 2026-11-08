//! Server fan-out for reliable video streams.
//!
//! The sharer opens one unidirectional QUIC stream per frame. This task
//! reads those frames, stamps the authenticated sender identity, and
//! writes each frame to every eligible viewer on its own stream. The
//! per-viewer queue is deliberately short: if a viewer's link backs up,
//! the server drops stale deltas, asks the sharer for an IDR, and resumes
//! at that keyframe instead of growing latency.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use parley_proto::video_stream::{MAX_STREAM_BYTES, StreamFrame, decode_frame, encode_header};
use parley_proto::{ServerBody, ServerMessage, VideoReport};
use quinn::{Connection, RecvStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::server::announce_share;
use crate::session::SessionRegistry;
use crate::voice::VoiceManager;

const VIEWER_QUEUE: usize = 4;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const KEYFRAME_REQUEST_EVERY: Duration = Duration::from_secs(1);

struct Frame {
    frame_id: u32,
    timestamp: u32,
    keyframe: bool,
    high_rate: bool,
    data: Vec<u8>,
}

struct Viewer {
    tx: mpsc::Sender<Arc<Frame>>,
    need_keyframe: Arc<AtomicBool>,
    last_request: Arc<Mutex<Option<Instant>>>,
}

/// Read the sharer's video streams for the life of the connection.
pub fn spawn_acceptor(
    conn: Connection,
    user_id: u32,
    username: String,
    media_id: u32,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
) {
    tokio::spawn(async move {
        let mut viewers: HashMap<u32, Viewer> = HashMap::new();
        loop {
            let mut recv = match conn.accept_uni().await {
                Ok(r) => r,
                Err(_) => break,
            };
            let frame = match read_frame(&mut recv).await {
                Ok(f) => f,
                Err(e) => {
                    if conn.close_reason().is_none() {
                        warn!(user_id, media_id, error = %e, "video stream rejected");
                    }
                    continue;
                }
            };
            let Some(channel_id) = voice.channel_of(media_id) else {
                continue;
            };
            // The helper marks the share itself. Marking here first would
            // consume the first-frame signal and suppress the announcement.
            announce_share(user_id, &username, media_id, channel_id, &registry, &voice);
            let frame = Arc::new(frame);
            let targets: Vec<u32> = voice
                .members_of(channel_id)
                .into_iter()
                .filter(|member| member.media_id != media_id && member.user_id != user_id)
                .map(|member| member.media_id)
                .collect();
            for target in targets {
                let Some(viewer_conn) = registry.connection_of_media(target) else {
                    continue;
                };
                let viewer = viewers.entry(target).or_insert_with(|| {
                    spawn_viewer(viewer_conn, target, media_id, registry.clone())
                });
                if let Err(mpsc::error::TrySendError::Full(_)) = viewer.tx.try_send(frame.clone()) {
                    // The viewer is behind. Drop the queued future (the
                    // writer will discard deltas until a keyframe) and ask
                    // the sharer for a fresh anchor.
                    viewer.need_keyframe.store(true, Ordering::Relaxed);
                    request_keyframe(&registry, target, media_id, &viewer.last_request);
                }
            }
        }
    });
}

impl Viewer {
    fn new(tx: mpsc::Sender<Arc<Frame>>) -> Self {
        Self {
            tx,
            need_keyframe: Arc::new(AtomicBool::new(false)),
            last_request: Arc::new(Mutex::new(None)),
        }
    }
}

fn spawn_viewer(
    conn: Connection,
    target_media: u32,
    sharer_media: u32,
    registry: Arc<SessionRegistry>,
) -> Viewer {
    let (tx, mut rx) = mpsc::channel::<Arc<Frame>>(VIEWER_QUEUE);
    let viewer = Viewer::new(tx);
    let need_keyframe = viewer.need_keyframe.clone();
    let last_request = viewer.last_request.clone();
    tokio::spawn(async move {
        let mut waiting_for_keyframe = false;
        while let Some(frame) = rx.recv().await {
            // A full queue or a failed write means the viewer's reference
            // chain is broken. Continue only from a keyframe.
            if need_keyframe.swap(false, Ordering::Relaxed) {
                waiting_for_keyframe = true;
            }
            if waiting_for_keyframe && !frame.keyframe {
                continue;
            }
            if frame.keyframe {
                waiting_for_keyframe = false;
            }
            if let Err(e) = write_frame(&conn, sharer_media, &frame).await {
                if conn.close_reason().is_none() {
                    debug!(target_media, sharer_media, error = %e, "viewer stream write failed");
                }
                waiting_for_keyframe = true;
                need_keyframe.store(true, Ordering::Relaxed);
                request_keyframe(&registry, target_media, sharer_media, &last_request);
            }
        }
    });
    viewer
}

fn request_keyframe(
    registry: &Arc<SessionRegistry>,
    viewer_media: u32,
    sharer_media: u32,
    last: &Arc<Mutex<Option<Instant>>>,
) {
    let Some(viewer_user) = registry.user_of_media(viewer_media) else {
        return;
    };
    let now = Instant::now();
    let mut last = last.lock().unwrap();
    if last.is_some_and(|t| now.duration_since(t) < KEYFRAME_REQUEST_EVERY) {
        return;
    }
    *last = Some(now);
    registry.send_to_media(
        sharer_media,
        ServerMessage {
            body: Some(ServerBody::VideoReport(VideoReport {
                reporter_id: viewer_user,
                reporter_media_id: viewer_media,
                gap_rate_permille: 0,
                viewer_fps: 0,
                keyframe_request: true,
            })),
        },
    );
}

async fn read_frame(recv: &mut RecvStream) -> Result<Frame, String> {
    let bytes = timeout(READ_TIMEOUT, recv.read_to_end(MAX_STREAM_BYTES))
        .await
        .map_err(|_| "video stream read timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let frame = decode_frame(&bytes).map_err(|e| e.to_string())?;
    Ok(Frame {
        frame_id: frame.frame_id,
        timestamp: frame.timestamp,
        keyframe: frame.keyframe,
        high_rate: frame.high_rate,
        data: frame.data.to_vec(),
    })
}

async fn write_frame(conn: &Connection, sender_id: u32, frame: &Frame) -> Result<(), String> {
    let mut send = timeout(Duration::from_secs(2), conn.open_uni())
        .await
        .map_err(|_| "opening a viewer stream timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let header = encode_header(&StreamFrame {
        sender_id,
        frame_id: frame.frame_id,
        timestamp: frame.timestamp,
        keyframe: frame.keyframe,
        high_rate: frame.high_rate,
        codec: parley_proto::media::VIDEO_CODEC_H264,
        data: &[],
    });
    let result = timeout(WRITE_TIMEOUT, async {
        send.write_all(&header).await.map_err(|e| e.to_string())?;
        send.write_all(&frame.data)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })
    .await;
    match result {
        Ok(Ok(())) => send.finish().map_err(|e| e.to_string()),
        Ok(Err(e)) => {
            send.reset(0u32.into()).ok();
            Err(e)
        }
        Err(_) => {
            send.reset(0u32.into()).ok();
            Err("writing a viewer frame timed out".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parley_proto::video_stream::{MAX_FRAME_BYTES, encode_frame};

    #[test]
    fn a_frame_survives_the_server_round_trip() {
        let payload = vec![7u8; 64 * 1024];
        let encoded = encode_frame(&StreamFrame {
            sender_id: 3,
            frame_id: 9,
            timestamp: 11,
            keyframe: true,
            high_rate: true,
            codec: parley_proto::media::VIDEO_CODEC_H264,
            data: &payload,
        });
        let frame = decode_frame(&encoded).unwrap();
        assert_eq!(frame.sender_id, 3);
        assert_eq!(frame.data, payload);
        assert!(frame.keyframe);
        assert!(MAX_FRAME_BYTES >= payload.len());
    }
}
