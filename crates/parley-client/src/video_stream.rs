//! Sending and receiving video on QUIC unidirectional streams.
//!
//! One stream per frame. QUIC carries the fragmentation and retransmits
//! what the network loses; because each frame is a separate stream, a
//! delayed frame does not block the next one. Audio keeps its datagram
//! path (an Opus frame fits in one MTU).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

#[cfg(feature = "video")]
use parley_proto::video_stream::MAX_STREAM_BYTES;
#[cfg(feature = "video")]
use parley_proto::video_stream::decode_frame;
use parley_proto::video_stream::{HEADER_LEN, StreamFrame, encode_header};
#[cfg(feature = "video")]
use quinn::RecvStream;
use quinn::{Connection, SendStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Frames waiting for the stream writer. Four frames is under 100 ms at
/// 60 fps: enough to absorb scheduler jitter, small enough that a slow
/// link drops the past instead of growing latency.
const SEND_QUEUE: usize = 4;
const OPEN_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(feature = "video")]
const READ_TIMEOUT: Duration = Duration::from_secs(5);

struct OutgoingFrame {
    frame_id: u32,
    timestamp: u32,
    keyframe: bool,
    high_rate: bool,
    data: Vec<u8>,
}

/// Handle used by the encoder thread. Cloning is cheap: the writer task
/// and the two feedback atomics are shared.
#[derive(Clone)]
pub struct VideoSender {
    tx: mpsc::Sender<OutgoingFrame>,
    /// 0 = the last frame left cleanly; non-zero = the link pushed back
    /// since the governor last sampled it. The pace governor consumes
    /// this instead of inferring congestion from frame loss.
    pressure: Arc<AtomicU32>,
    /// A frame was dropped, so the next encoded picture must be an IDR.
    need_keyframe: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Queued,
    /// The writer queue was full. The frame is lost; a keyframe has been
    /// requested and the caller should keep the session alive.
    Dropped,
    /// The writer task is gone (connection closed).
    Closed,
}

impl VideoSender {
    /// Start the writer task for `conn`. Must be called inside a Tokio
    /// runtime (all screen/camera start paths are async).
    pub fn spawn(conn: Connection) -> Self {
        let (tx, mut rx) = mpsc::channel::<OutgoingFrame>(SEND_QUEUE);
        let pressure = Arc::new(AtomicU32::new(0));
        let need_keyframe = Arc::new(AtomicBool::new(false));
        let task_pressure = pressure.clone();
        let task_keyframe = need_keyframe.clone();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                match write_frame(&conn, &frame).await {
                    Ok(()) => {
                        // A clean write proves the link is moving again.
                        // Keeping the flag until the governor samples it
                        // would turn one transient reset into a permanent
                        // resolution drop.
                        task_pressure.store(0, Ordering::Relaxed);
                    }
                    Err(e) => {
                        if conn.close_reason().is_none() {
                            eprintln!("share: video stream write failed: {e}; requesting IDR");
                        }
                        task_pressure.store(1000, Ordering::Relaxed);
                        task_keyframe.store(true, Ordering::Relaxed);
                        if conn.close_reason().is_some() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            tx,
            pressure,
            need_keyframe,
        }
    }

    /// Queue one encoded frame. The frame is copied because the encoder's
    /// Annex-B buffer is reused for the next picture.
    pub fn send(
        &self,
        frame_id: u32,
        timestamp: u32,
        keyframe: bool,
        high_rate: bool,
        data: &[u8],
    ) -> SendOutcome {
        if data.is_empty() || data.len() > parley_proto::video_stream::MAX_FRAME_BYTES {
            return SendOutcome::Dropped;
        }
        let frame = OutgoingFrame {
            frame_id,
            timestamp,
            keyframe,
            high_rate,
            data: data.to_vec(),
        };
        match self.tx.try_send(frame) {
            Ok(()) => SendOutcome::Queued,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // The network (or the server's per-viewer fan-out) is
                // pushing back. Stop filling the buffer and ask the
                // encoder for a fresh anchor.
                self.pressure.store(1000, Ordering::Relaxed);
                self.need_keyframe.store(true, Ordering::Relaxed);
                SendOutcome::Dropped
            }
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        }
    }

    /// Read and clear the "link pushed back" state. 0 when healthy.
    pub fn take_pressure(&self) -> u32 {
        self.pressure.swap(0, Ordering::Relaxed)
    }

    /// True once after a drop or a failed stream write.
    pub fn take_keyframe_request(&self) -> bool {
        self.need_keyframe.swap(false, Ordering::Relaxed)
    }
}

async fn write_frame(conn: &Connection, frame: &OutgoingFrame) -> Result<(), String> {
    let mut send = timeout(OPEN_TIMEOUT, conn.open_uni())
        .await
        .map_err(|_| "opening a video stream timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let header = encode_header(&StreamFrame {
        sender_id: 0, // the server stamps the authenticated identity
        frame_id: frame.frame_id,
        timestamp: frame.timestamp,
        keyframe: frame.keyframe,
        high_rate: frame.high_rate,
        codec: parley_proto::media::VIDEO_CODEC_H264,
        data: &[],
    });
    if let Err(e) = write_all(&mut send, &header, &frame.data).await {
        send.reset(0u32.into()).ok();
        return Err(e);
    }
    send.finish().map_err(|e| e.to_string())
}

async fn write_all(
    send: &mut SendStream,
    header: &[u8; HEADER_LEN],
    data: &[u8],
) -> Result<(), String> {
    timeout(WRITE_TIMEOUT, async {
        send.write_all(header).await.map_err(|e| e.to_string())?;
        send.write_all(data).await.map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| "writing a video frame timed out".to_string())??;
    Ok(())
}

#[cfg(feature = "video")]
async fn read_frame(recv: &mut RecvStream) -> Result<Vec<u8>, String> {
    timeout(READ_TIMEOUT, recv.read_to_end(MAX_STREAM_BYTES))
        .await
        .map_err(|_| "reading a video frame timed out".to_string())?
        .map_err(|e| e.to_string())
}

/// Receive stream frames for one connection and feed the existing display
/// hub. Kept in a dedicated task so decoding never runs on the audio
/// datagram reader; the hub's bounded queue is the display backstop.
#[cfg(feature = "video")]
pub fn spawn_reader(conn: Connection, hub: parley_core::video_receive::SharedVideoReceiveHub) {
    tokio::spawn(async move {
        loop {
            let mut recv = match conn.accept_uni().await {
                Ok(r) => r,
                Err(_) => break,
            };
            let bytes = match read_frame(&mut recv).await {
                Ok(b) => b,
                Err(e) => {
                    if conn.close_reason().is_none() {
                        eprintln!("video: stream read failed: {e}");
                    }
                    continue;
                }
            };
            let frame = match decode_frame(&bytes) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("video: rejected a stream frame: {e}");
                    continue;
                }
            };
            hub.push_stream_frame(
                frame.sender_id,
                frame.codec,
                frame.keyframe,
                frame.high_rate,
                frame.frame_id,
                frame.data,
            );
        }
    });
}
