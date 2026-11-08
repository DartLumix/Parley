//! Android capture backend.
//!
//! Kotlin owns the platform half: MediaProjection + ImageReader for a
//! screen share, Camera2 + ImageReader for a camera share. A screen frame
//! arrives as RGBA and is swizzled to the encoder's BGRA layout. A camera
//! frame arrives as its three YUV_420_888 planes and is converted, rotated,
//! mirrored and packed in one Rust pass. The encoder thread, pacing, loss
//! ladder, preview, keyframe pokes and wire format are the same code the
//! Linux and Windows backends use, so an Android share is indistinguishable
//! from a desktop share to a viewer.
//!
//! There is exactly one share at a time per app; the static slot is the
//! hand-off between the JNI frame callback and the encoder. Kotlin starts
//! the capture service after `start` has installed the slot, which means
//! the first few frames may be dropped rather than queued before there is
//! an encoder to receive them.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, LazyLock, Mutex};

use super::{PreviewSink, ShareController, ShareHandle, ShareOptions, ToEncoder, encoder_thread};
use crate::ShareEvent;

/// Shared with the JNI entry points. The sender is held here for the life
/// of the share; Kotlin's frames are always RGBA with a row stride.
struct Sink {
    tx: SyncSender<ToEncoder>,
    done: tokio::sync::mpsc::UnboundedSender<()>,
    failed: tokio::sync::mpsc::UnboundedSender<String>,
    negotiated: Option<(u32, u32)>,
}

static SINK: LazyLock<Mutex<Option<Sink>>> = LazyLock::new(|| Mutex::new(None));

/// Called from `ShareService.nativePushFrame` on the screen ImageReader
/// thread. The buffer is RGBA (`PixelFormat.RGBA_8888`), with `row_stride`
/// bytes per row. The shared encoder wants tightly packed BGRA, so this is
/// one row-wise swizzle and copy.
pub fn android_push_frame(width: u32, height: u32, row_stride: usize, rgba: &[u8]) {
    if width == 0 || height == 0 || row_stride < width as usize * 4 {
        return;
    }
    let w = width as usize;
    let h = height as usize;
    let Some(need) = w.checked_mul(h).and_then(|pixels| pixels.checked_mul(4)) else {
        return;
    };
    let row_bytes = w * 4;
    if rgba.len() < row_stride.saturating_mul(h - 1).saturating_add(row_bytes) {
        return;
    }

    let mut bgra = vec![0u8; need];
    for y in 0..h {
        let src = &rgba[y * row_stride..y * row_stride + row_bytes];
        let dst = &mut bgra[y * row_bytes..(y + 1) * row_bytes];
        for (s, d) in src
            .as_chunks::<4>()
            .0
            .iter()
            .zip(dst.as_chunks_mut::<4>().0)
        {
            d[0] = s[2]; // B
            d[1] = s[1]; // G
            d[2] = s[0]; // R
            d[3] = 255;
        }
    }

    send_bgra(w as u32, h as u32, bgra);
}

/// One converted BGRA frame on its way to the encoder. The sink is fenced
/// by the slot lock, and the format message is sent before the first frame
/// of a new shape so the encoder never sees pixels whose size it has not
/// negotiated.
fn send_bgra(width: u32, height: u32, bgra: Vec<u8>) {
    let row_bytes = width as usize * 4;
    let mut slot = SINK.lock().unwrap();
    let Some(sink) = slot.as_mut() else {
        return;
    };
    if sink.negotiated != Some((width, height)) {
        if sink
            .tx
            .try_send(ToEncoder::Negotiated {
                width,
                height,
                fps: 30.0,
            })
            .is_err()
        {
            return; // encoder busy or gone: retry the format on the next frame
        }
        sink.negotiated = Some((width, height));
    }
    // Bounded and non-blocking: a full encoder channel means this frame is
    // already stale, so dropping it is the same skip-to-newest rule the
    // other capture backends follow.
    match sink.tx.try_send(ToEncoder::Frame {
        data: Arc::new(bgra),
        stride: row_bytes,
    }) {
        Ok(()) | Err(TrySendError::Full(_)) => {}
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// Camera2's YUV_420_888 planes, converted straight into tightly packed
/// BGRA. `rotation` is the display-corrected sensor orientation (0/90/180/
/// 270) and `mirror` is true for a front-facing camera.
///
/// The output loop is organised by DESTINATION rows, so every BGRA row is
/// written sequentially. The source mapping is chosen at compile time per
/// rotation/mirror pair, which keeps the branch out of the pixel loop.
#[allow(clippy::too_many_arguments)]
pub fn android_push_yuv_frame(
    width: u32,
    height: u32,
    y: &[u8],
    y_row: usize,
    y_pixel: usize,
    u: &[u8],
    u_row: usize,
    u_pixel: usize,
    v: &[u8],
    v_row: usize,
    v_pixel: usize,
    rotation: u32,
    mirror: bool,
) {
    if width == 0 || height == 0 || y_pixel == 0 || u_pixel == 0 || v_pixel == 0 {
        return;
    }
    let w = width as usize;
    let h = height as usize;
    let Some(y_last) = (h - 1)
        .checked_mul(y_row)
        .and_then(|row| row.checked_add((w - 1).checked_mul(y_pixel)?))
    else {
        return;
    };
    // Chroma planes are sampled at half resolution. Their last byte is not
    // necessarily at width/2: an odd width rounds up, and the pixel stride
    // (usually 2 for NV21-style planes) still applies.
    let uv_w = w.div_ceil(2);
    let uv_h = h.div_ceil(2);
    let Some(u_last) = (uv_h - 1)
        .checked_mul(u_row)
        .and_then(|row| row.checked_add((uv_w - 1).checked_mul(u_pixel)?))
    else {
        return;
    };
    let Some(v_last) = (uv_h - 1)
        .checked_mul(v_row)
        .and_then(|row| row.checked_add((uv_w - 1).checked_mul(v_pixel)?))
    else {
        return;
    };
    if y.len() <= y_last || u.len() <= u_last || v.len() <= v_last {
        return;
    }

    let normalized = rotation % 360;
    if normalized % 90 != 0 {
        return;
    }
    let (out_w, out_h) = if normalized % 180 == 0 {
        (w, h)
    } else {
        (h, w)
    };
    let Some(needed) = out_w.checked_mul(out_h).and_then(|px| px.checked_mul(4)) else {
        return;
    };
    let mut bgra = vec![0u8; needed];
    match (normalized, mirror) {
        (0, false) => convert_yuv::<0, false>(
            y, y_row, y_pixel, u, u_row, u_pixel, v, v_row, v_pixel, w, h, &mut bgra,
        ),
        (0, true) => convert_yuv::<0, true>(
            y, y_row, y_pixel, u, u_row, u_pixel, v, v_row, v_pixel, w, h, &mut bgra,
        ),
        (90, false) => convert_yuv::<90, false>(
            y, y_row, y_pixel, u, u_row, u_pixel, v, v_row, v_pixel, w, h, &mut bgra,
        ),
        (90, true) => convert_yuv::<90, true>(
            y, y_row, y_pixel, u, u_row, u_pixel, v, v_row, v_pixel, w, h, &mut bgra,
        ),
        (180, false) => convert_yuv::<180, false>(
            y, y_row, y_pixel, u, u_row, u_pixel, v, v_row, v_pixel, w, h, &mut bgra,
        ),
        (180, true) => convert_yuv::<180, true>(
            y, y_row, y_pixel, u, u_row, u_pixel, v, v_row, v_pixel, w, h, &mut bgra,
        ),
        (270, false) => convert_yuv::<270, false>(
            y, y_row, y_pixel, u, u_row, u_pixel, v, v_row, v_pixel, w, h, &mut bgra,
        ),
        (270, true) => convert_yuv::<270, true>(
            y, y_row, y_pixel, u, u_row, u_pixel, v, v_row, v_pixel, w, h, &mut bgra,
        ),
        _ => return,
    }
    // A 90°/270° camera frame swaps dimensions; the encoder must be
    // negotiated at the ROTATED shape, not the sensor's native one.
    send_bgra(out_w as u32, out_h as u32, bgra);
}

/// YUV_420_888 → BGRA with the camera's display rotation and front-camera
/// mirror applied. The caller has already validated every plane index.
#[allow(clippy::too_many_arguments)]
fn convert_yuv<const ROT: u32, const MIRROR: bool>(
    y: &[u8],
    y_row: usize,
    y_pixel: usize,
    u: &[u8],
    u_row: usize,
    u_pixel: usize,
    v: &[u8],
    v_row: usize,
    v_pixel: usize,
    w: usize,
    h: usize,
    out: &mut [u8],
) {
    let (out_w, out_h) = if ROT % 180 == 0 { (w, h) } else { (h, w) };
    let out_row = out_w * 4;
    for dy in 0..out_h {
        let row = &mut out[dy * out_row..(dy + 1) * out_row];
        for dx in 0..out_w {
            let mx = if MIRROR { out_w - 1 - dx } else { dx };
            let (sx, sy) = match ROT {
                0 => (mx, dy),
                90 => (dy, h - 1 - mx),
                180 => (w - 1 - mx, h - 1 - dy),
                270 => (w - 1 - dy, mx),
                _ => return,
            };
            // SAFETY: every plane index was bounded against its buffer
            // before this conversion, and the output row/column are in
            // range by construction.
            let y_value = unsafe { *y.get_unchecked(sy * y_row + sx * y_pixel) } as i32;
            let u_value = unsafe { *u.get_unchecked((sy / 2) * u_row + (sx / 2) * u_pixel) } as i32;
            let v_value = unsafe { *v.get_unchecked((sy / 2) * v_row + (sx / 2) * v_pixel) } as i32;
            let c = (y_value - 16).max(0);
            let u = u_value - 128;
            let v = v_value - 128;
            let r = ((298 * c + 409 * v + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 516 * u + 128) >> 8).clamp(0, 255) as u8;
            let dst = unsafe { row.get_unchecked_mut(dx * 4..dx * 4 + 4) };
            dst[0] = b;
            dst[1] = g;
            dst[2] = r;
            dst[3] = 255;
        }
    }
}

/// The platform capture stopped (MediaProjection stop, camera error, or
/// the service being destroyed). The supervisor treats this like a capture
/// thread ending: tear down the encoder and report `ShareEvent::Ended`.
pub fn android_capture_ended() {
    let sink = SINK.lock().unwrap().take();
    if let Some(sink) = sink {
        let _ = sink.done.send(());
    }
}

/// The platform capture failed with a reason. Taking the slot means the
/// normal end signal cannot win the race with this failure: there is one
/// terminal event per share, and it carries the useful message.
pub fn android_capture_failed(reason: String) {
    let sink = SINK.lock().unwrap().take();
    if let Some(sink) = sink {
        let _ = sink.failed.send(reason);
    }
}

fn start_internal(
    conn: quinn::Connection,
    video_loss: Arc<AtomicU32>,
    viewer_fps: Arc<AtomicU32>,
    events: tokio::sync::mpsc::UnboundedSender<ShareEvent>,
    options: ShareOptions,
    preview: Option<PreviewSink>,
    video_stream: Option<crate::video_stream::VideoSender>,
) -> Result<(ShareHandle, ShareController), String> {
    let (frame_tx, frame_rx) = sync_channel::<ToEncoder>(4);
    let force_idr = Arc::new(AtomicBool::new(false));
    let pending = Arc::new(AtomicBool::new(false));
    let capture_drops = Arc::new(AtomicU64::new(0));
    let handle = ShareHandle {
        flag: force_idr.clone(),
    };
    let (die_tx, mut die_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (failed_tx, mut failed_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    {
        let mut slot = SINK.lock().unwrap();
        if slot.is_some() {
            return Err("a share is already active".to_string());
        }
        *slot = Some(Sink {
            tx: frame_tx,
            done: done_tx,
            failed: failed_tx,
            negotiated: None,
        });
    }

    let enc_handle = match std::thread::Builder::new()
        .name("parley-android-encode".into())
        .spawn(move || {
            encoder_thread(
                frame_rx,
                conn,
                force_idr,
                video_loss,
                viewer_fps,
                pending,
                capture_drops,
                die_tx,
                options,
                None,
                preview,
                video_stream,
            )
        }) {
        Ok(handle) => handle,
        Err(e) => {
            let _ = SINK.lock().unwrap().take();
            return Err(format!("android encoder thread: {e}"));
        }
    };

    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        let failure = tokio::select! {
            biased;
            reason = failed_rx.recv() => reason,
            _ = stop_rx.recv() => None,
            _ = die_rx.recv() => None,
            _ = done_rx.recv() => None,
        };
        // Drop the only sender first: this wakes the encoder if it is
        // waiting for a frame, then the join finishes promptly.
        let _ = SINK.lock().unwrap().take();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = enc_handle.join();
        })
        .await;
        match failure {
            Some(reason) => {
                let _ = events.send(ShareEvent::Failed(reason));
            }
            None => {
                let _ = events.send(ShareEvent::Ended { external: false });
            }
        }
    });

    Ok((handle, ShareController { stop_tx }))
}

/// Screen-share entry point, matching the Linux/Windows signature. Android
/// has no system-audio capture in this first cut; the caller may still
/// pass a tap, but it is dropped rather than delaying the video share.
pub async fn start(
    conn: quinn::Connection,
    video_loss: Arc<AtomicU32>,
    viewer_fps: Arc<AtomicU32>,
    events: tokio::sync::mpsc::UnboundedSender<ShareEvent>,
    audio: Option<super::ShareAudioTap>,
    dmabuf: bool,
    options: ShareOptions,
    preview: Option<PreviewSink>,
) -> Result<(ShareHandle, ShareController), String> {
    if dmabuf {
        return Err("dma-buf capture is Linux-only".to_string());
    }
    if audio.is_some() {
        eprintln!("android share: system audio is not implemented; sharing video only");
    }
    let video_stream = options
        .video_streams
        .then(|| crate::video_stream::VideoSender::spawn(conn.clone()));
    start_internal(
        conn,
        video_loss,
        viewer_fps,
        events,
        options,
        preview,
        video_stream,
    )
}

/// Camera-share entry point. Kotlin opens Camera2 only after this slot is
/// installed, so a failed `start_camera` never leaves a camera running.
pub async fn start_camera(
    conn: quinn::Connection,
    video_loss: Arc<AtomicU32>,
    viewer_fps: Arc<AtomicU32>,
    events: tokio::sync::mpsc::UnboundedSender<ShareEvent>,
    options: ShareOptions,
    preview: Option<PreviewSink>,
) -> Result<(ShareHandle, ShareController), String> {
    let video_stream = options
        .video_streams
        .then(|| crate::video_stream::VideoSender::spawn(conn.clone()));
    start_internal(
        conn,
        video_loss,
        viewer_fps,
        events,
        options,
        preview,
        video_stream,
    )
}

/// Android has no application-audio picker in this first cut. Returning an
/// empty list keeps the shared dialog usable and tells it there is nothing
/// to capture.
pub fn list_audio_players() -> Result<Vec<super::AudioPlayer>, String> {
    Ok(Vec::new())
}
