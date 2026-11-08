//! Server fan-out for reliable video streams.
//!
//! The sharer opens one unidirectional QUIC stream per frame. This task
//! reads those frames, stamps the authenticated sender identity, and
//! writes each frame to every eligible viewer on its own stream. The
//! per-viewer queue is deliberately short: if a viewer's link backs up,
//! the server drops stale deltas, asks the sharer for an IDR, and resumes
//! at that keyframe instead of growing latency.
//!
//! IDR requests are debounced per SENDER, not per viewer. One encoder
//! emits one keyframe that anchors every viewer at once, so ten stalled
//! viewers must cost the sharer one request — a per-viewer debounce (the
//! first version) produced a request storm exactly when the link was
//! already saturated. Suppressed requests still accumulate their gap
//! rate, which rides the next report: the sharer's ladder needs the loss
//! signal even when the IDR request itself was folded into a neighbour's.
//!
//! When server transcode is enabled the same acceptor feeds the workers
//! whole frames (no fragment assembly) and [`StreamOut`] lets a worker
//! emit its tier through the identical framing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
use crate::transcode::TranscodeManager;
use crate::voice::VoiceManager;

/// Frames waiting for one viewer's stream writer. Four frames is under
/// 100 ms at 60 fps: enough to absorb scheduler jitter, small enough that
/// a slow link drops the past instead of growing latency.
const VIEWER_QUEUE: usize = 4;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
/// Global per-sender IDR debounce. One IDR anchors every viewer, so this
/// is the interval a whole share may be asked for an intra frame, not the
/// interval one viewer may ask.
const KEYFRAME_REQUEST_EVERY: Duration = Duration::from_secs(2);

/// One encoded picture, shared by every viewer of the share. The payload
/// is behind an `Arc` so a fan-out to twenty viewers copies a handle, not
/// the frame; a transcoded tier reuses one buffer the same way.
pub(crate) struct Frame {
    pub(crate) frame_id: u32,
    pub(crate) timestamp: u32,
    pub(crate) keyframe: bool,
    pub(crate) high_rate: bool,
    pub(crate) data: Arc<[u8]>,
}

struct Viewer {
    tx: mpsc::Sender<Arc<Frame>>,
    need_keyframe: Arc<AtomicBool>,
    loss: Arc<ViewerLoss>,
}

/// Frames successfully queued for, and dropped from, one viewer since the
/// last report to the sharer. The ratio is the honest server-side loss
/// signal: 1000‰ means every frame this viewer was sent was shed.
#[derive(Default)]
struct ViewerLoss {
    sent: AtomicU32,
    dropped: AtomicU32,
}

impl ViewerLoss {
    fn note_sent(&self) {
        self.sent.fetch_add(1, Ordering::Relaxed);
    }

    fn note_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn permille(&self) -> u32 {
        let sent = self.sent.load(Ordering::Relaxed);
        let dropped = self.dropped.load(Ordering::Relaxed);
        dropped
            .saturating_mul(1000)
            .checked_div(sent.saturating_add(dropped))
            .unwrap_or(0)
    }

    fn clear(&self) {
        self.sent.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
    }
}

/// One sharer's IDR state, shared by every viewer of that share.
struct SenderFeedback {
    last_request: Mutex<Option<Instant>>,
    window: Duration,
    /// Worst gap seen since the last report actually left. A suppressed
    /// request must not erase the evidence that its viewer was starving.
    pending_gap: AtomicU32,
}

impl SenderFeedback {
    fn new() -> Self {
        Self::with_window(KEYFRAME_REQUEST_EVERY)
    }

    fn with_window(window: Duration) -> Self {
        Self {
            last_request: Mutex::new(None),
            window,
            pending_gap: AtomicU32::new(0),
        }
    }

    /// Record one viewer's loss and say whether a report is due. Returns
    /// the gap rate to send (the worst of the accumulated window).
    fn note_loss(&self, gap_permille: u32) -> Option<u32> {
        self.pending_gap.fetch_max(gap_permille, Ordering::Relaxed);
        let now = Instant::now();
        let mut last = self.last_request.lock().unwrap();
        if last.is_some_and(|t| now.duration_since(t) < self.window) {
            return None;
        }
        *last = Some(now);
        let gap = self.pending_gap.swap(0, Ordering::Relaxed);
        Some(gap.max(gap_permille))
    }
}

/// Read the sharer's video streams for the life of the connection.
pub fn spawn_acceptor(
    conn: Connection,
    user_id: u32,
    username: String,
    media_id: u32,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
    transcode: Arc<TranscodeManager>,
) {
    tokio::spawn(async move {
        let mut viewers: HashMap<u32, Viewer> = HashMap::new();
        let feedback = Arc::new(SenderFeedback::new());
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
            // Server-side transcode: a whole frame skips the datagram
            // fragmenter/assembler, which is pure overhead on this path.
            transcode.note_stream_frame(media_id, channel_id, &frame);
            let frame = Arc::new(frame);
            let targets: Vec<u32> = voice
                .members_of(channel_id)
                .into_iter()
                .filter(|member| member.media_id != media_id && member.user_id != user_id)
                .map(|member| member.media_id)
                .collect();
            // A viewer who left the channel (or disconnected) must not
            // keep a queue and a writer task alive in this map; a long
            // call would otherwise accumulate one per departed peer.
            viewers.retain(|target, _| targets.contains(target));
            let mut closed = None;
            for target in targets {
                // This viewer is on a server-encoded tier: the transcode
                // worker owns its stream, and feeding the original here
                // would put two unrelated reference chains on one decoder.
                if !transcode.should_forward(media_id, target) {
                    continue;
                }
                let Some(viewer_conn) = registry.connection_of_media(target) else {
                    continue;
                };
                let viewer = viewers.entry(target).or_insert_with(|| {
                    spawn_viewer(
                        viewer_conn,
                        target,
                        media_id,
                        registry.clone(),
                        feedback.clone(),
                    )
                });
                match viewer.tx.try_send(frame.clone()) {
                    Ok(()) => viewer.loss.note_sent(),
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // The viewer is behind. Drop the queued future (the
                        // writer discards deltas until a keyframe) and ask
                        // the sharer for a fresh anchor.
                        viewer.need_keyframe.store(true, Ordering::Relaxed);
                        viewer.loss.note_dropped();
                        notify_loss(&registry, target, media_id, &feedback, &viewer.loss);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // The writer task is gone (viewer disconnected).
                        // Leaving the entry would make every later frame
                        // pay for a dead viewer.
                        closed = Some(target);
                        break;
                    }
                }
            }
            if let Some(target) = closed {
                viewers.remove(&target);
            }
        }
        // `viewers` drops here: each writer's receiver closes and its task
        // exits, so a dead connection never leaves stream handles alive.
    });
}

/// Writer for one viewer of one sharer. Frames are written sequentially,
/// so the decoder sees the encoder's order; the queue is short, so a slow
/// link loses stale pictures instead of growing latency.
fn spawn_viewer(
    conn: Connection,
    target_media: u32,
    sharer_media: u32,
    registry: Arc<SessionRegistry>,
    feedback: Arc<SenderFeedback>,
) -> Viewer {
    let (tx, mut rx) = mpsc::channel::<Arc<Frame>>(VIEWER_QUEUE);
    let need_keyframe = Arc::new(AtomicBool::new(false));
    let loss = Arc::new(ViewerLoss::default());
    let task_keyframe = need_keyframe.clone();
    let task_loss = loss.clone();
    tokio::spawn(async move {
        let mut waiting_for_keyframe = false;
        while let Some(frame) = rx.recv().await {
            // A full queue or a failed write means the viewer's reference
            // chain is broken. Continue only from a keyframe.
            if task_keyframe.swap(false, Ordering::Relaxed) {
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
                task_keyframe.store(true, Ordering::Relaxed);
                task_loss.note_dropped();
                notify_loss(&registry, target_media, sharer_media, &feedback, &task_loss);
            }
            // A closed connection will never accept another stream; the
            // acceptor sees the closed queue and removes this entry.
            if conn.close_reason().is_some() {
                break;
            }
        }
    });
    Viewer {
        tx,
        need_keyframe,
        loss,
    }
}

/// A server-encoded tier's output transport for one viewer. Same framing
/// and same writer discipline as the sharer's own path: order preserved,
/// tiny queue, early reset on a failed write.
pub(crate) struct StreamOut {
    tx: mpsc::Sender<Arc<Frame>>,
    failed: Arc<AtomicBool>,
}

impl StreamOut {
    /// Requires a runtime handle: the transcode worker is a plain thread,
    /// and the writer has to be an async task on the connection's runtime.
    pub(crate) fn spawn(handle: &tokio::runtime::Handle, conn: Connection, sender_id: u32) -> Self {
        let (tx, mut rx) = mpsc::channel::<Arc<Frame>>(VIEWER_QUEUE);
        let failed = Arc::new(AtomicBool::new(false));
        let task_failed = failed.clone();
        handle.spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write_frame(&conn, sender_id, &frame).await.is_err() {
                    task_failed.store(true, Ordering::Relaxed);
                    if conn.close_reason().is_some() {
                        break;
                    }
                }
            }
        });
        Self { tx, failed }
    }

    /// Queue one tier frame. False means the viewer is behind (or gone);
    /// the worker then forces an IDR so it can resume.
    pub(crate) fn offer(&self, frame: Arc<Frame>) -> bool {
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(_) => {
                self.failed.store(true, Ordering::Relaxed);
                false
            }
        }
    }

    /// Read and clear "this viewer pushed back".
    pub(crate) fn take_failed(&self) -> bool {
        self.failed.swap(false, Ordering::Relaxed)
    }
}

/// Turn one viewer's drop into a debounced report to the sharer. The
/// report is a keyframe request AND a real gap sample: the sharer both
/// emits a fresh anchor and steps its bitrate ladder down.
fn notify_loss(
    registry: &Arc<SessionRegistry>,
    viewer_media: u32,
    sharer_media: u32,
    feedback: &SenderFeedback,
    loss: &ViewerLoss,
) {
    if let Some(gap) = feedback.note_loss(loss.permille()) {
        send_keyframe_report(registry, viewer_media, sharer_media, gap);
        loss.clear();
    }
}

fn send_keyframe_report(
    registry: &Arc<SessionRegistry>,
    viewer_media: u32,
    sharer_media: u32,
    gap_permille: u32,
) {
    let Some(viewer_user) = registry.user_of_media(viewer_media) else {
        return;
    };
    registry.send_to_media(
        sharer_media,
        ServerMessage {
            body: Some(ServerBody::VideoReport(VideoReport {
                reporter_id: viewer_user,
                reporter_media_id: viewer_media,
                gap_rate_permille: gap_permille,
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
        data: Arc::from(frame.data),
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
    // Explicit drop in every arm: a reset handle that stays in scope keeps
    // its stream slot until the next statement, and under heavy shedding
    // that is the difference between reclaiming the slot and hitting the
    // peer's stream ceiling.
    match result {
        Ok(Ok(())) => {
            let finished = send.finish().map_err(|e| e.to_string());
            drop(send);
            finished
        }
        Ok(Err(e)) => {
            send.reset(0u32.into()).ok();
            drop(send);
            Err(e)
        }
        Err(_) => {
            send.reset(0u32.into()).ok();
            drop(send);
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

    /// The whole point of a per-sender debounce: ten stalled viewers cause
    /// one request, not ten. The encoder answers all of them with a single
    /// IDR, and a storm would turn congestion into a keyframe-only stream.
    #[test]
    fn one_idr_request_per_sender_per_window() {
        let feedback = SenderFeedback::with_window(Duration::from_millis(50));
        assert_eq!(feedback.note_loss(200), Some(200));
        for _ in 0..9 {
            assert_eq!(feedback.note_loss(400), None);
        }
    }

    /// Debouncing may delay the loss signal but must never drop it: the
    /// suppressed viewers' worst gap rides the next report, so the
    /// sharer's bitrate ladder still sees the congestion.
    #[test]
    fn suppressed_requests_accumulate_gap_for_the_next_report() {
        let feedback = SenderFeedback::with_window(Duration::from_millis(20));
        assert_eq!(feedback.note_loss(100), Some(100));
        assert_eq!(feedback.note_loss(300), None);
        assert_eq!(feedback.note_loss(200), None);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(feedback.note_loss(50), Some(300));
    }

    #[test]
    fn a_viewer_loss_window_reads_as_permille() {
        let loss = ViewerLoss::default();
        for _ in 0..3 {
            loss.note_sent();
        }
        loss.note_dropped();
        assert_eq!(loss.permille(), 250, "one frame of four lost");
        loss.clear();
        assert_eq!(loss.permille(), 0, "a report consumes the window");
    }
}
