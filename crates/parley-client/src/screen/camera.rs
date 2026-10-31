//! Camera share (Linux): V4L2 capture feeding the SAME encoder path as a
//! screen share.
//!
//! A camera is not a portal source, so this module skips `ashpd` and the
//! PipeWire graph entirely: it opens the first capture-capable
//! `/dev/videoN`, negotiates YUYV, converts each frame to tightly packed
//! BGRA, and hands it to `ToEncoder` exactly like the portal callback
//! does. Everything downstream — pacing, the pace governor, bitrate
//! ladder, preview, keyframe pokes and the wire format — is shared, so a
//! viewer cannot tell a camera tile from a screen tile (which is the
//! point: one stream per sender, no second protocol).
//!
//! Format choice is YUYV because every UVC webcam offers it and it needs
//! no codec dependency to decode. 1280x720 is tried first (the common
//! native camera mode), then 960x540, then 640x480. MJPEG-only cameras
//! are rejected with a clear error rather than a black tile.
//!
//! Stop is a flag checked around a 500 ms V4L2 poll timeout; the
//! supervisor joins the capture and encoder threads before reporting
//! `ShareEvent::Ended`, the same ordering rule the screen path uses.

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use v4l::buffer::Type;
use v4l::capability::Flags;
use v4l::format::{Format, FourCC};
use v4l::io::traits::CaptureStream;
use v4l::prelude::{Device, MmapStream};
use v4l::video::Capture;

use super::{PreviewSink, ShareController, ShareHandle, ShareOptions, ToEncoder, encoder_thread};
use crate::ShareEvent;

/// The camera runs at a fixed 30 fps; the encoder's own profile and the
/// dialog's cap still lower what is actually sent.
const CAMERA_FPS: f64 = 30.0;

/// Preferred capture modes, best first. The encoder downscales from here
/// if the dialog asked for a smaller stream.
const MODES: [(u32, u32); 3] = [(1280, 720), (960, 540), (640, 480)];

/// Open the first camera that can capture YUYV, and pin a mode.
fn open_camera() -> Result<(Device, Format, String), String> {
    let mut last = String::new();
    for index in 0..8 {
        let dev = match Device::new(index) {
            Ok(dev) => dev,
            Err(e) => {
                last = format!("/dev/video{index}: {e}");
                continue;
            }
        };
        let caps = match dev.query_caps() {
            Ok(caps) => caps,
            Err(e) => {
                last = format!("/dev/video{index}: {e}");
                continue;
            }
        };
        // Metadata/ISP nodes advertise neither; opening one would only
        // fail later at stream start.
        if !caps
            .capabilities
            .contains(Flags::VIDEO_CAPTURE | Flags::STREAMING)
        {
            continue;
        }
        for (width, height) in MODES {
            let wanted = Format::new(width, height, FourCC::new(b"YUYV"));
            let applied = match dev.set_format(&wanted) {
                Ok(applied) => applied,
                Err(e) => {
                    last = format!("/dev/video{index}: {e}");
                    continue;
                }
            };
            if applied.fourcc.str() != Ok("YUYV") || applied.width == 0 || applied.height == 0 {
                last = format!(
                    "/dev/video{index}: driver answered {} instead of YUYV",
                    applied.fourcc
                );
                continue;
            }
            // Best-effort: a driver that rejects the interval still
            // streams at its own rate, and the encoder paces anyway.
            let _ = dev.set_params(&v4l::video::capture::Parameters::with_fps(
                CAMERA_FPS as u32,
            ));
            return Ok((dev, applied, caps.card));
        }
    }
    Err(if last.is_empty() {
        "no camera found (no /dev/video0..7 is a YUYV capture device)".to_string()
    } else {
        format!("no usable camera: {last}")
    })
}

/// One YUYV frame (row stride included) to tightly packed BGRA.
///
/// Integer BT.601 limited-range conversion: `(298·C + 409·E) >> 8` etc.,
/// with C = Y-16 and E = V-128; the `>> 8` form is the usual fast
/// approximation and is accurate to a code value. Returns false when the
/// buffer is shorter than the negotiated mode.
fn yuyv_to_bgra(src: &[u8], width: usize, height: usize, stride: usize, dst: &mut [u8]) -> bool {
    if width == 0 || height == 0 || !width.is_multiple_of(2) {
        return false;
    }
    let row_bytes = width * 2;
    let stride = stride.max(row_bytes);
    if src.len() < stride * (height - 1) + row_bytes || dst.len() < width * height * 4 {
        return false;
    }
    let mut out = 0usize;
    for row in 0..height {
        let start = row * stride;
        let row_src = &src[start..start + row_bytes];
        for pair in row_src.as_chunks::<4>().0 {
            let y0 = i32::from(pair[0]);
            let u = i32::from(pair[1]) - 128;
            let y1 = i32::from(pair[2]);
            let v = i32::from(pair[3]) - 128;
            for y in [y0, y1] {
                let c = (y - 16).max(0);
                let r = ((298 * c + 409 * v + 128) >> 8).clamp(0, 255) as u8;
                let g = ((298 * c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
                let b = ((298 * c + 516 * u + 128) >> 8).clamp(0, 255) as u8;
                dst[out..out + 4].copy_from_slice(&[b, g, r, 255]);
                out += 4;
            }
        }
    }
    true
}

/// The capture thread: poll, convert, try_send. A full encoder channel
/// drops the frame (live content is never worth queueing) and a 500 ms
/// timeout ends the share rather than wedging it on an unplugged camera.
fn capture_loop(
    dev: Device,
    width: usize,
    height: usize,
    stride: usize,
    tx: SyncSender<ToEncoder>,
    stop: Arc<AtomicBool>,
    done: tokio::sync::mpsc::UnboundedSender<()>,
) {
    let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 4) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("camera: stream setup failed: {e}");
            let _ = done.send(());
            return;
        }
    };
    stream.set_timeout(Duration::from_millis(500));
    if tx
        .try_send(ToEncoder::Negotiated {
            width: width as u32,
            height: height as u32,
            fps: CAMERA_FPS,
        })
        .is_err()
    {
        let _ = done.send(());
        return;
    }
    let frame_bytes = width * height * 4;
    let mut bgra = vec![0u8; frame_bytes];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match stream.next() {
            Ok((buf, meta)) => {
                let used = (meta.bytesused as usize).min(buf.len());
                if !yuyv_to_bgra(&buf[..used], width, height, stride, &mut bgra) {
                    continue;
                }
                let frame = Arc::new(std::mem::take(&mut bgra));
                // Reconnected/full channel: dropping is correct, the
                // next frame is fresher. The encoder's try_send failure
                // also means the share is over; the encoder thread's
                // death reaches the supervisor through `die`.
                let _ = tx.try_send(ToEncoder::Frame {
                    data: frame,
                    stride: width * 4,
                });
                bgra = vec![0u8; frame_bytes];
            }
            Err(e) if e.kind() == ErrorKind::TimedOut => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                eprintln!("camera: no frame for 500 ms — ending the share");
                break;
            }
            Err(e) => {
                eprintln!("camera: capture failed: {e}");
                break;
            }
        }
    }
    let _ = done.send(());
}

/// Start a camera share. Returns the same handles as the screen path:
/// a keyframe poke and a stop controller. Completion is reported as
/// `ShareEvent::Ended` only after both threads have joined.
pub async fn start_camera(
    conn: quinn::Connection,
    video_loss: Arc<AtomicU32>,
    viewer_fps: Arc<AtomicU32>,
    events: tokio::sync::mpsc::UnboundedSender<ShareEvent>,
    options: ShareOptions,
    preview: Option<PreviewSink>,
) -> Result<(ShareHandle, ShareController), String> {
    let (dev, format, card) = open_camera()?;
    let width = format.width as usize;
    let height = format.height as usize;
    let stride = (format.stride as usize).max(width * 2);
    println!("camera: {card} {width}x{height} YUYV @ {CAMERA_FPS:.0} fps (stride {stride})");

    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<ToEncoder>(4);
    let force_idr = Arc::new(AtomicBool::new(false));
    let handle = ShareHandle {
        flag: force_idr.clone(),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (die_tx, mut die_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let enc_handle = std::thread::Builder::new()
        .name("parley-camera-encode".into())
        .spawn(move || {
            encoder_thread(
                frame_rx, conn, force_idr, video_loss, viewer_fps, die_tx, options, preview,
            )
        })
        .map_err(|e| format!("camera encoder thread: {e}"))?;

    let stop_for_capture = stop.clone();
    let capture_handle = std::thread::Builder::new()
        .name("parley-camera".into())
        .spawn(move || {
            capture_loop(
                dev,
                width,
                height,
                stride,
                frame_tx,
                stop_for_capture,
                done_tx,
            )
        })
        .map_err(|e| format!("camera capture thread: {e}"))?;

    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let stop_for_task = stop.clone();
    let events_for_task = events.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = stop_rx.recv() => {}
            _ = die_rx.recv() => {}
            _ = done_rx.recv() => {}
        }
        stop_for_task.store(true, Ordering::Relaxed);
        // Join off the runtime: the encoder may be waiting on a frame,
        // and the capture thread may be in its 500 ms poll.
        let _ = tokio::task::spawn_blocking(move || {
            let _ = capture_handle.join();
            let _ = enc_handle.join();
        })
        .await;
        let _ = events_for_task.send(ShareEvent::Ended { external: false });
    });

    Ok((handle, ShareController { stop_tx }))
}

#[cfg(test)]
mod tests {
    use super::yuyv_to_bgra;

    #[test]
    fn yuyv_gray_is_neutral() {
        // Two grey pixels: Y=128 (limited range), U=V=128.
        let src = [128u8, 128, 128, 128];
        let mut dst = [0u8; 8];
        assert!(yuyv_to_bgra(&src, 2, 1, 4, &mut dst));
        assert_eq!(&dst[..4], &[130, 130, 130, 255]);
        assert_eq!(&dst[4..], &[130, 130, 130, 255]);
    }

    #[test]
    fn yuyv_honours_row_stride_and_rejects_short_buffers() {
        // One padded row: 4 bytes of pixels + 4 bytes of padding.
        let src = [128u8, 128, 128, 128, 0, 0, 0, 0];
        let mut dst = [0u8; 8];
        assert!(yuyv_to_bgra(&src, 2, 1, 8, &mut dst));
        assert_eq!(dst[3], 255);
        assert!(!yuyv_to_bgra(&src[..3], 2, 1, 8, &mut dst));
    }
}
