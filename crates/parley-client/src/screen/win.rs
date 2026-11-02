//! Windows capture backend: Windows Graphics Capture for the pixels,
//! WASAPI for the sound.
//!
//! The shape is the Linux backend's, because everything downstream of
//! the capture is shared: this module produces `ToEncoder` messages
//! (format, then BGRA frames with a row stride) and the shared encoder
//! thread paces, fragments and sends them. The share-audio mux is shared
//! too; this module only produces interleaved 48 kHz stereo f32, exactly
//! what PipeWire hands the mux on Linux.
//!
//! Source selection uses the system picker (`GraphicsCapturePicker`) —
//! the Windows equivalent of the XDG ScreenCast dialog. It owns a hidden
//! owner window and pumps its own messages, so it runs on the capture
//! thread, never the Flutter thread. That thread then blocks in
//! `GraphicsCaptureApiHandler::start` (WGC's message loop); stopping is a
//! `WM_QUIT` posted to it, which is the same thing the crate's own
//! `CaptureControl` does.
//!
//! Static desktops are a real case (a document, a paused video, an idle
//! window): WGC only delivers a frame when the content changes, while
//! the receiver hides a share after five seconds without a datagram. A
//! 1 Hz keepalive repeats the newest frame's `Arc` — no second copy — and
//! the shared pace governor now compares viewer reports against the rate
//! the sender actually delivered, so a still screen cannot be mistaken
//! for a viewer that cannot keep up.
//!
//! Share audio comes in the same two scopes as Linux:
//!
//! - `Output` is the default render device in loopback mode: everything
//!   the machine plays, Parley included. The caller runs the shared echo
//!   guard on it.
//! - `Players(ids)` is one WASAPI process-loopback client per PID
//!   (Windows 10 2004+), summed by this module. Parley's own process is
//!   never offered or captured, so the participants do not hear
//!   themselves back.

use std::collections::VecDeque;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wasapi::{
    AudioClient, DeviceEnumerator, Direction, SampleType, SessionState, StreamMode, WaveFormat,
    initialize_mta,
};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_SOURCE_READERF_STREAMTICK,
    MF_VERSION, MFCreateAttributes, MFCreateDeviceSource, MFCreateMediaType,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video, MFSTARTUP_LITE,
    MFShutdown, MFStartup, MFVideoFormat_RGB32, MFVideoInterlace_Progressive,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
use windows::core::PWSTR;
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::graphics_capture_picker::{Error as PickerError, GraphicsCapturePicker};
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use super::{
    CameraOverlay, PreviewSink, ShareAudio, ShareAudioTap, ShareController, ShareHandle,
    ShareOptions, ToEncoder, encoder_thread,
};
use crate::ShareEvent;

/// A capture rate the shared encoder clamps to its profile: software
/// caps at 30 fps, hardware would take 60. WGC does not report the
/// monitor's refresh through the picker, and the encoder's `target_fps`
/// is the authority anyway, so 60 is a ceiling, never a promise.
const CAPTURE_FPS: f64 = 60.0;

/// How often the keepalive thread looks at the newest frame. A frame
/// older than KEEPALIVE_AFTER is repeated, so an idle desktop still
/// carries a datagram every second (the receiver's stall detector
/// hides a share after 5 s of silence).
const KEEPALIVE_EVERY: Duration = Duration::from_millis(250);
const KEEPALIVE_AFTER: Duration = Duration::from_millis(900);

/// 10 ms of stereo audio at 48 kHz: the mux's natural step, and what a
/// WASAPI event usually delivers.
const MIX_STEP_SAMPLES: usize = 480 * 2;
/// Period asked of a process-loopback client. There is no render device
/// behind it to report one, and shared-mode event-driven clients still
/// need a non-zero buffer; 10 ms is the engine's usual period.
const PROCESS_LOOPBACK_BUFFER_HNS: i64 = 100_000;
/// Playout backlog before an app is mixed in (30 ms, the same figure the
/// PipeWire collector uses): enough to absorb event jitter without adding
/// audible delay.
const PLAYER_PREBUFFER_SAMPLES: usize = 30 * 48 * 2;
/// Bound on one process feed's backlog: a stalled app must not add a
/// second of latency to the mix. Oldest samples are dropped first.
const FEED_CAP_SAMPLES: usize = 240 * 2 * 48;

/// The newest captured frame, shared between the WGC callback (writer)
/// and the keepalive thread (reader).
#[derive(Clone)]
struct LatestFrame {
    data: Arc<Vec<u8>>,
    stride: usize,
    at: Instant,
}

/// Values handed to the capture handler. Everything it needs to feed
/// the shared encoder and to be stopped from outside.
struct CaptureFlags {
    tx: SyncSender<ToEncoder>,
    ready: tokio::sync::oneshot::Sender<Result<(), String>>,
    width: u32,
    height: u32,
    stop: Arc<AtomicBool>,
    /// Set by `on_closed` (the captured window went away). The supervisor
    /// reports that as an external end, like the portal's Closed signal.
    closed: Arc<AtomicBool>,
    /// Wakes the supervisor when the capture thread stops for any reason.
    die: tokio::sync::mpsc::UnboundedSender<()>,
    /// The capture thread's id, written once `new` runs on it. Needed to
    /// post WM_QUIT from the supervisor.
    tid: Arc<AtomicU32>,
    latest: Arc<Mutex<Option<LatestFrame>>>,
}

struct Capture {
    tx: SyncSender<ToEncoder>,
    width: u32,
    height: u32,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    die: tokio::sync::mpsc::UnboundedSender<()>,
    latest: Arc<Mutex<Option<LatestFrame>>>,
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = CaptureFlags;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let flags = ctx.flags;
        flags
            .tid
            .store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
        // The encoder can be built before the first frame, so a static
        // screen is not a share with no format.
        flags
            .tx
            .send(ToEncoder::Negotiated {
                width: flags.width,
                height: flags.height,
                fps: CAPTURE_FPS,
            })
            .map_err(|_| "encoder is gone".to_string())?;
        let keepalive_latest = flags.latest.clone();
        let keepalive_tx = flags.tx.clone();
        let tx = flags.tx;
        let stop = flags.stop.clone();
        // One thread for the whole share: it repeats the newest frame
        // while WGC has nothing new. `latest` holds an Arc, so this is a
        // pointer copy per tick and a cheap encode once a second.
        std::thread::Builder::new()
            .name("parley-share-keepalive".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(KEEPALIVE_EVERY);
                    let frame = keepalive_latest.lock().unwrap().clone();
                    let Some(frame) = frame else { continue };
                    if frame.at.elapsed() < KEEPALIVE_AFTER {
                        continue;
                    }
                    if keepalive_tx
                        .try_send(ToEncoder::Frame {
                            data: frame.data,
                            stride: frame.stride,
                        })
                        .is_ok()
                    {
                        // Re-stamp the copy we just queued, not the one
                        // the capture callback stores, so the next repeat
                        // waits a full second again.
                        if let Some(slot) = keepalive_latest.lock().unwrap().as_mut() {
                            slot.at = Instant::now();
                        }
                    }
                }
            })
            .map_err(|e| format!("keepalive thread: {e}"))?;
        let _ = flags.ready.send(Ok(()));
        Ok(Self {
            tx,
            width: flags.width,
            height: flags.height,
            stop: flags.stop,
            closed: flags.closed,
            die: flags.die,
            latest: flags.latest,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // A stop requested while the session was still starting: the
        // WM_QUIT may have arrived before the message loop began.
        if self.stop.load(Ordering::Relaxed) {
            capture_control.stop();
            return Ok(());
        }
        // A failed map is one bad frame (a GPU hiccup, a lost device),
        // not the end of the share: drop it and keep the session alive.
        let mut buffer = match frame.buffer() {
            Ok(buffer) => buffer,
            Err(e) => {
                eprintln!("share: capture frame: {e}");
                return Ok(());
            }
        };
        let width = buffer.width();
        let height = buffer.height();
        let stride = buffer.row_pitch() as usize;
        if width == 0 || height == 0 || stride < width as usize * 4 {
            return Ok(());
        }
        // The frame as WGC mapped it: row stride kept (BGRA + padding).
        // The shared encoder's strided path walks it directly.
        let raw = buffer.as_raw_buffer();
        let len = height as usize * stride;
        let Some(bytes) = raw.get(..len) else {
            return Ok(()); // truncated mapping: skip this frame
        };
        let data = Arc::new(bytes.to_vec());
        // A window can be resized mid-share; the encoder follows.
        if width != self.width || height != self.height {
            self.width = width;
            self.height = height;
            let _ = self.tx.send(ToEncoder::Negotiated {
                width,
                height,
                fps: CAPTURE_FPS,
            });
        }
        {
            let mut slot = self.latest.lock().unwrap();
            match slot.as_mut() {
                Some(frame) => {
                    frame.data = data.clone();
                    frame.stride = stride;
                    frame.at = Instant::now();
                }
                None => {
                    *slot = Some(LatestFrame {
                        data: data.clone(),
                        stride,
                        at: Instant::now(),
                    });
                }
            }
        }
        // Bounded and non-blocking: if the encoder is behind, the newest
        // frame wins on the next callback (the shared skip-to-newest
        // rule) instead of stalling the WGC callback.
        let _ = self.tx.try_send(ToEncoder::Frame { data, stride });
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.closed.store(true, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.die.send(());
        Ok(())
    }
}

/// A camera opened through Media Foundation, already negotiated to
/// RGB32 (the BGRA order WGC and the encoder both produce). The buffer
/// is top-down or bottom-up depending on the driver's stride sign, so
/// the reader normalises it before handing a frame on.
struct MfCamera {
    reader: IMFSourceReader,
    width: usize,
    height: usize,
    /// Negative when the driver hands over bottom-up RGB32.
    stride: isize,
}

/// Open the first video capture device. Beta path: the first device is
/// chosen deliberately (there is no camera picker yet), and every
/// failure is a plain `Err` the bridge turns into a message — never a
/// panic and never a share that starts without video.
fn open_mf_camera() -> Result<MfCamera, String> {
    unsafe {
        // MTA because this thread only ever owns the source reader; a
        // failure here (RPC_E_CHANGED_MODE on an already-STA thread) is
        // harmless for Media Foundation's lite startup.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(|e| format!("Media Foundation: {e}"))?;

        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(|e| format!("camera attributes: {e}"))?;
        let attrs = attrs.ok_or("camera attributes were not created")?;
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| format!("camera enumeration: {e}"))?;

        let mut devices: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut devices, &mut count)
            .map_err(|e| format!("camera enumeration: {e}"))?;
        let activate = if count == 0 || devices.is_null() {
            None
        } else {
            let list = std::slice::from_raw_parts(devices, count as usize);
            list[0].clone()
        };
        CoTaskMemFree(Some(devices as *const _));
        let activate =
            activate.ok_or("no camera found (Media Foundation listed no video device)")?;

        let source: IMFMediaSource =
            MFCreateDeviceSource(&activate).map_err(|e| format!("camera open: {e}"))?;

        let mut reader_attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut reader_attrs, 1)
            .map_err(|e| format!("camera reader attributes: {e}"))?;
        reader_attrs
            .as_ref()
            .ok_or("camera reader attributes were not created")?
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(|e| format!("camera reader attributes: {e}"))?;
        let reader: IMFSourceReader =
            MFCreateSourceReaderFromMediaSource(&source, reader_attrs.as_ref())
                .map_err(|e| format!("camera reader: {e}"))?;

        let media_type: IMFMediaType =
            MFCreateMediaType().map_err(|e| format!("camera media type: {e}"))?;
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("camera media type: {e}"))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(|e| format!("camera media type: {e}"))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| format!("camera media type: {e}"))?;
        reader
            .SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                None,
                &media_type,
            )
            .map_err(|e| format!("camera format: {e}"))?;

        let actual = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .map_err(|e| format!("camera format: {e}"))?;
        let size = actual
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(|e| format!("camera frame size: {e}"))?;
        let width = (size >> 32) as usize;
        let height = (size & 0xffff_ffff) as usize;
        if width == 0 || height == 0 {
            return Err("the camera reported an empty frame size".to_string());
        }
        let stride = actual
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|v| v as i32 as isize)
            .unwrap_or((width * 4) as isize);
        Ok(MfCamera {
            reader,
            width,
            height,
            stride,
        })
    }
}

/// Pull frames until stop or end-of-stream. Each frame is copied into a
/// tightly packed top-down BGRA buffer (the encoder's stride path still
/// works, but a camera frame is small enough that the normalisation is
/// free and removes the bottom-up driver quirk).
fn mf_read(
    cam: &MfCamera,
    stop: &AtomicBool,
    mut sink: impl FnMut(Arc<Vec<u8>>),
) -> Result<(), String> {
    let row_bytes = cam.width * 4;
    let pitch = cam.stride.unsigned_abs();
    if pitch < row_bytes {
        return Err(format!(
            "camera stride {pitch} is shorter than a {} px row",
            cam.width
        ));
    }
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        unsafe {
            let mut flags = 0u32;
            let mut sample: Option<IMFSample> = None;
            cam.reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    None,
                    Some(&mut sample),
                )
                .map_err(|e| format!("camera read: {e}"))?;
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err("camera read failed (device removed?)".to_string());
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if flags & MF_SOURCE_READERF_STREAMTICK.0 as u32 != 0 {
                continue;
            }
            let Some(sample) = sample else {
                continue;
            };
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| format!("camera buffer: {e}"))?;
            let mut data: *mut u8 = ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut data, None, Some(&mut len))
                .map_err(|e| format!("camera buffer lock: {e}"))?;
            let bytes = std::slice::from_raw_parts(data, len as usize);
            let result = if bytes.len() < pitch * cam.height {
                Err(format!(
                    "camera frame is {} bytes, expected {}",
                    bytes.len(),
                    pitch * cam.height
                ))
            } else {
                let mut out = vec![0u8; row_bytes * cam.height];
                for y in 0..cam.height {
                    let src_y = if cam.stride < 0 {
                        cam.height - 1 - y
                    } else {
                        y
                    };
                    out[y * row_bytes..(y + 1) * row_bytes]
                        .copy_from_slice(&bytes[src_y * pitch..src_y * pitch + row_bytes]);
                }
                Ok(out)
            };
            let _ = buffer.Unlock();
            sink(Arc::new(result?));
        }
    }
    Ok(())
}

/// Camera share. The encoder path is the screen path: one stream,
/// frame pacing, the ladder and the preview all behave identically.
/// The camera is opened on the capture thread so its COM apartment is
/// the one that reads it, and a failed open is reported before this
/// function returns.
pub async fn start_camera(
    conn: quinn::Connection,
    video_loss: Arc<AtomicU32>,
    viewer_fps: Arc<AtomicU32>,
    events: tokio::sync::mpsc::UnboundedSender<ShareEvent>,
    options: ShareOptions,
    preview: Option<PreviewSink>,
) -> Result<(ShareHandle, ShareController), String> {
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
                frame_rx, conn, force_idr, video_loss, viewer_fps, die_tx, options, None, preview,
            )
        })
        .map_err(|e| format!("camera encoder thread: {e}"))?;

    let (ready_tx, ready_rx) = sync_channel::<Result<(u32, u32), String>>(1);
    let stop_for_capture = stop.clone();
    let capture_handle = std::thread::Builder::new()
        .name("parley-camera".into())
        .spawn(move || {
            let cam = match open_mf_camera() {
                Ok(cam) => cam,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    let _ = done_tx.send(());
                    return;
                }
            };
            let width = cam.width as u32;
            let height = cam.height as u32;
            println!(
                "camera: Media Foundation {width}x{height} RGB32 stride {} @ 30 fps",
                cam.stride
            );
            if ready_tx.send(Ok((width, height))).is_err() {
                return; // the caller gave up before the camera opened
            }
            if frame_tx
                .send(ToEncoder::Negotiated {
                    width,
                    height,
                    fps: 30.0,
                })
                .is_err()
            {
                return; // encoder thread already gone
            }
            let row = cam.width * 4;
            if let Err(e) = mf_read(&cam, &stop_for_capture, |data| {
                let _ = frame_tx.try_send(ToEncoder::Frame { data, stride: row });
            }) {
                eprintln!("camera: capture ended: {e}");
            }
            unsafe {
                let _ = MFShutdown();
            }
            let _ = done_tx.send(());
        })
        .map_err(|e| format!("camera capture thread: {e}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let _ = capture_handle.join();
            let _ = enc_handle.join();
            return Err(e);
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            return Err("camera did not open within 5 s".to_string());
        }
    }

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
        let _ = tokio::task::spawn_blocking(move || {
            let _ = capture_handle.join();
            let _ = enc_handle.join();
        })
        .await;
        let _ = events_for_task.send(ShareEvent::Ended { external: false });
    });

    Ok((handle, ShareController { stop_tx }))
}

/// Camera overlay for a Windows screen share. The Media Foundation
/// source is a beta path; failing to open one is not a share failure,
/// and the screen keeps streaming. The thread clears the slot on exit,
/// so a frozen last picture cannot outlive the camera.
pub fn start_overlay(
    stop: Arc<AtomicBool>,
    overlay: CameraOverlay,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("parley-camera-overlay".into())
        .spawn(move || match open_mf_camera() {
            Ok(cam) => {
                println!(
                    "camera overlay: Media Foundation {}x{} RGB32 stride {}",
                    cam.width, cam.height, cam.stride
                );
                let width = cam.width;
                let height = cam.height;
                let slot = overlay.clone();
                if let Err(e) = mf_read(&cam, &stop, |data| slot.publish(data, width, height)) {
                    eprintln!("camera overlay: capture ended: {e}");
                }
                unsafe {
                    let _ = MFShutdown();
                }
                overlay.clear();
            }
            Err(e) => {
                eprintln!("camera overlay: {e}; continuing screen-only");
                overlay.clear();
            }
        })
        .map_err(|e| format!("camera overlay thread: {e}"))
}

/// Entry point, matching the Linux one: pick a source, start capture,
/// start share audio, hand back the keyframe poke + stop controller.
/// Completion is reported as `ShareEvent::Ended` after teardown.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    conn: quinn::Connection,
    video_loss: Arc<AtomicU32>,
    viewer_fps: Arc<AtomicU32>,
    events: tokio::sync::mpsc::UnboundedSender<ShareEvent>,
    audio: Option<ShareAudioTap>,
    dmabuf: bool,
    options: ShareOptions,
    preview: Option<PreviewSink>,
) -> Result<(ShareHandle, ShareController), String> {
    if dmabuf {
        return Err("dma-buf capture is Linux-only (VAAPI); Windows uses WGC copies".to_string());
    }
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<ToEncoder>(4);
    let force_idr = Arc::new(AtomicBool::new(false));
    let handle = ShareHandle {
        flag: force_idr.clone(),
    };
    // The overlay is opt-in. If the camera fails to open, the thread
    // logs it and the screen keeps going; nothing downstream depends on
    // it having produced a frame.
    let overlay = options.camera_overlay.then(CameraOverlay::new);
    let overlay_for_encoder = overlay.clone();
    let overlay_for_capture = overlay.clone();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let tid = Arc::new(AtomicU32::new(0));
    let latest: Arc<Mutex<Option<LatestFrame>>> = Arc::new(Mutex::new(None));
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let (die_tx, mut die_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let capture_flags = CaptureFlags {
        tx: frame_tx,
        ready: ready_tx,
        width: 0,
        height: 0,
        stop: stop_flag.clone(),
        closed: closed.clone(),
        die: die_tx.clone(),
        tid: tid.clone(),
        latest,
    };
    let capture_die = die_tx.clone();
    let capture_stop = stop_flag.clone();
    // The picker blocks for as long as the dialog is open, and the WGC
    // message loop then owns this thread for the whole share. One thread
    // for both keeps the WinRT apartment and the picked item on it.
    let capture_thread = std::thread::Builder::new()
        .name("parley-share-capture".into())
        .spawn(move || {
            let picked = match GraphicsCapturePicker::pick_item() {
                Ok(Some(item)) => item,
                Ok(None) => {
                    let _ = capture_flags
                        .ready
                        .send(Err("no source selected".to_string()));
                    return;
                }
                Err(PickerError::Canceled) => {
                    let _ = capture_flags.ready.send(Err("share cancelled".to_string()));
                    return;
                }
                Err(e) => {
                    let _ = capture_flags
                        .ready
                        .send(Err(format!("capture picker: {e}")));
                    return;
                }
            };
            let (width, height) = match picked.size() {
                Ok(size) if size.0 > 0 && size.1 > 0 => (size.0 as u32, size.1 as u32),
                Ok(_) => {
                    let _ = capture_flags
                        .ready
                        .send(Err("the selected source has no size".to_string()));
                    return;
                }
                Err(e) => {
                    let _ = capture_flags
                        .ready
                        .send(Err(format!("capture item size: {e}")));
                    return;
                }
            };
            let mut flags = capture_flags;
            flags.width = width;
            flags.height = height;
            let settings = Settings::new(
                picked,
                CursorCaptureSettings::WithCursor,
                DrawBorderSettings::WithoutBorder,
                SecondaryWindowSettings::Include,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                flags,
            );
            // Blocks in WGC's message loop until the supervisor posts
            // WM_QUIT (or the session dies). Anything after this point
            // cannot be returned from `start`, so an error ends the
            // share and reaches the log; a clean return means the
            // capture session is over either way.
            if let Err(e) = Capture::start(settings) {
                eprintln!("share: capture ended: {e}");
            }
            capture_stop.store(true, Ordering::Relaxed);
            let _ = capture_die.send(());
        })
        .map_err(|e| format!("capture thread: {e}"))?;

    // Wait for the picker + session setup. Cancelling is an immediate
    // error, exactly as the portal dialog's cancel is on Linux.
    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("capture thread died before starting".to_string()),
    }

    let encoder = std::thread::Builder::new()
        .name("parley-share-encoder".into())
        .spawn(move || {
            encoder_thread(
                frame_rx,
                conn,
                force_idr,
                video_loss,
                viewer_fps,
                die_tx,
                options,
                overlay_for_encoder,
                preview,
            )
        })
        .map_err(|e| format!("encoder thread: {e}"))?;

    let overlay_thread = overlay_for_capture.as_ref().and_then(|overlay| {
        match start_overlay(stop_flag.clone(), overlay.clone()) {
            Ok(handle) => Some(handle),
            Err(e) => {
                eprintln!("camera overlay: {e}; continuing screen-only");
                None
            }
        }
    });

    // Audio starts only once every fallible step is behind us: an early
    // return must never strand a capture thread holding a stop flag that
    // nobody left can set (the Linux supervisor follows the same order).
    if let Some(tap) = audio {
        start_audio_capture(stop_flag.clone(), tap);
    }
    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        let external = tokio::select! {
            biased;
            _ = stop_rx.recv() => {
                eprintln!("share: ending — stop requested");
                false
            }
            _ = die_rx.recv() => {
                let closed = closed.load(Ordering::Relaxed);
                eprintln!(
                    "share: ending — {}",
                    if closed {
                        "the captured window or session closed"
                    } else {
                        "capture pipeline ended"
                    }
                );
                closed
            }
        };
        stop_flag.store(true, Ordering::Relaxed);
        // WM_QUIT ends WGC's GetMessage loop. Post only after `new`
        // recorded the id, i.e. after the queue exists.
        let thread_id = tid.load(Ordering::Relaxed);
        if thread_id != 0 {
            unsafe {
                let _ =
                    PostThreadMessageW(thread_id, WM_QUIT, Default::default(), Default::default());
            }
        }
        match tokio::time::timeout(
            Duration::from_secs(3),
            tokio::task::spawn_blocking(move || capture_thread.join()),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => eprintln!("share: capture thread join failed"),
            Err(_) => eprintln!("share: capture thread did not stop within 3 s — leaked"),
        }
        match tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || encoder.join()),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => eprintln!("share: encoder thread join failed"),
            Err(_) => eprintln!("share: encoder thread did not stop within 5 s — leaked"),
        }
        if let Some(overlay) = overlay_thread {
            match tokio::time::timeout(
                Duration::from_secs(2),
                tokio::task::spawn_blocking(move || overlay.join()),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => eprintln!("camera overlay: join failed"),
                Err(_) => eprintln!("camera overlay: did not stop within 2 s — leaked"),
            }
        }
        let _ = events.send(ShareEvent::Ended { external });
    });

    Ok((handle, ShareController { stop_tx }))
}

// ---- share audio (WASAPI loopback) ----

/// Start the share's audio capture on its own thread. Failures reach
/// stderr; video continues.
fn start_audio_capture(stop: Arc<AtomicBool>, tap: ShareAudioTap) {
    std::thread::Builder::new()
        .name("parley-share-audio".into())
        .spawn(move || {
            let ShareAudioTap { source, tx } = tap;
            let result = match source {
                ShareAudio::Output => output_capture_thread(stop, tx),
                ShareAudio::Players(pids) => players_capture_thread(stop, tx, pids),
            };
            if let Err(e) = result {
                eprintln!("share-audio: {e}");
            }
        })
        .ok();
}

/// One WASAPI capture client on the default render device in loopback
/// mode: the Windows equivalent of the PipeWire monitor. Everything the
/// machine plays is here, this app included — hence the echo guard.
fn output_capture_thread(
    stop: Arc<AtomicBool>,
    sys_tx: SyncSender<Vec<f32>>,
) -> Result<(), String> {
    initialize_mta()
        .ok()
        .map_err(|e| format!("COM init: {e}"))?;
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(|e| e.to_string())?;
    let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    let (period, _min) = client.get_device_period().map_err(|e| e.to_string())?;
    client
        .initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: period,
            },
        )
        .map_err(|e| format!("loopback init: {e}"))?;
    run_capture(stop, sys_tx, client)
}

/// The per-application tap: one process-loopback client per selected
/// PID, summed by [`players_capture_thread`]. Windows 10 2004+ only;
/// the picker's list is what the user chose from, so a PID that died in
/// the meantime is logged, not fatal.
fn players_capture_thread(
    stop: Arc<AtomicBool>,
    sys_tx: SyncSender<Vec<f32>>,
    pids: Vec<u32>,
) -> Result<(), String> {
    if pids.is_empty() {
        return Err("no application selected for share audio".to_string());
    }
    let feeds: Vec<Arc<PlayerFeed>> = pids.iter().map(|_| Arc::new(PlayerFeed::new())).collect();
    let mut handles = Vec::new();
    for (pid, feed) in pids.into_iter().zip(feeds.iter().cloned()) {
        let stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name(format!("parley-share-audio-{pid}"))
            .spawn(move || {
                if let Err(e) = one_player_capture(stop, pid, feed.clone()) {
                    eprintln!("share-audio: pid {pid}: {e}");
                }
                feed.alive.store(false, Ordering::Relaxed);
            });
        match handle {
            Ok(h) => handles.push(h),
            Err(e) => eprintln!("share-audio: pid {pid} thread: {e}"),
        }
    }

    // Mix at a fixed 10 ms step. Each feed is drained independently, so
    // one app's event jitter cannot shift another's timeline; a feed
    // that has not produced a full step contributes silence, exactly as
    // a paused PipeWire node would.
    //
    // A 10 ms cadence needs a 1 ms timer: Windows' default Sleep
    // granularity is ~15.6 ms, which would deliver audio ~35% slow and
    // make the receiver's jitter buffer conceal constantly. Audio apps
    // raise the resolution for exactly this; Drop restores it.
    let _timer = TimerResolution::new();
    let mut mix = vec![0.0f32; MIX_STEP_SAMPLES];
    // The clock is wall time, not one step per loop: a scheduler hiccup
    // must deliver the audio that elapsed, or the stream slowly falls
    // behind real time and the receiver starves.
    let mut clock = Instant::now();
    'mix: while !stop.load(Ordering::Relaxed) {
        let due = (clock.elapsed().as_secs_f64() * 100.0).floor() as u32;
        if due == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        // Never try to make up more than 100 ms in one go; a long stall
        // (suspend/resume) is better dropped than replayed.
        let steps = due.min(10);
        if due > steps {
            clock = Instant::now();
        } else {
            clock += Duration::from_millis(10 * u64::from(steps));
        }
        for _ in 0..steps {
            mix.fill(0.0);
            let mut alive = false;
            for feed in &feeds {
                if feed.alive.load(Ordering::Relaxed) {
                    alive = true;
                }
                feed.drain_into(&mut mix);
            }
            if !alive && feeds.iter().all(|f| f.is_empty()) {
                break 'mix;
            }
            // The mux is gone (share ended) or behind; dropping is right
            // for live audio.
            let _ = sys_tx.try_send(mix.clone());
        }
    }
    for handle in handles {
        let _ = handle.join();
    }
    Ok(())
}

/// One app's loopback client. Its samples are appended to `feed`, which
/// the mixer drains on its own clock.
fn one_player_capture(
    stop: Arc<AtomicBool>,
    pid: u32,
    feed: Arc<PlayerFeed>,
) -> Result<(), String> {
    initialize_mta()
        .ok()
        .map_err(|e| format!("COM init: {e}"))?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    let mut client = AudioClient::new_application_loopback_client(pid, true)
        .map_err(|e| format!("process loopback: {e}"))?;
    client
        .initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: PROCESS_LOOPBACK_BUFFER_HNS,
            },
        )
        .map_err(|e| format!("process loopback init: {e}"))?;
    run_capture_with(stop, client, |pcm| feed.push(pcm))
}

fn run_capture(
    stop: Arc<AtomicBool>,
    tx: SyncSender<Vec<f32>>,
    client: AudioClient,
) -> Result<(), String> {
    run_capture_with(stop, client, |pcm| {
        let _ = tx.try_send(pcm);
    })
}

/// The shared WASAPI read loop: event-driven, reads whatever the client
/// has, converts f32 LE bytes to interleaved samples, hands them to
/// `sink`. The timeout is what makes `stop` responsive (100 ms).
fn run_capture_with(
    stop: Arc<AtomicBool>,
    client: AudioClient,
    mut sink: impl FnMut(Vec<f32>),
) -> Result<(), String> {
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
    client
        .start_stream()
        .map_err(|e| format!("start stream: {e}"))?;
    let mut bytes: VecDeque<u8> = VecDeque::new();
    while !stop.load(Ordering::Relaxed) {
        if event.wait_for_event(100).is_err() {
            // A timeout is the normal case; a real device failure shows
            // up as an error on the next read.
            continue;
        }
        let new_frames = match capture.get_next_packet_size() {
            Ok(frames) => frames.unwrap_or(0),
            Err(e) => {
                eprintln!("share-audio: capture read: {e}");
                break;
            }
        };
        if new_frames > 0
            && let Err(e) = capture.read_from_device_to_deque(&mut bytes)
        {
            eprintln!("share-audio: capture: {e}");
            break;
        }
        if bytes.len() >= 4 {
            bytes.make_contiguous();
            let slice = bytes.as_slices().0;
            let mut pcm = Vec::with_capacity(slice.len() / 4);
            for sample in slice.as_chunks::<4>().0 {
                pcm.push(f32::from_le_bytes(*sample));
            }
            bytes.clear();
            if !pcm.is_empty() {
                sink(pcm);
            }
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

/// Process-wide timer resolution for the mix clock (`timeBeginPeriod`).
struct TimerResolution;

impl TimerResolution {
    fn new() -> Self {
        unsafe {
            timeBeginPeriod(1);
        }
        Self
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

/// One selected app's backlog, written by its capture thread and drained
/// by the mixer. The same discipline as the PipeWire collector: fill to
/// the prebuffer once, then contribute one step per tick and clamp the
/// sum so two loud apps cannot clip.
struct PlayerFeed {
    queue: Mutex<VecDeque<f32>>,
    alive: AtomicBool,
    primed: AtomicBool,
}

impl PlayerFeed {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            alive: AtomicBool::new(true),
            primed: AtomicBool::new(false),
        }
    }

    fn push(&self, pcm: Vec<f32>) {
        let mut queue = self.queue.lock().unwrap();
        queue.extend(pcm);
        if queue.len() > FEED_CAP_SAMPLES {
            let excess = queue.len() - FEED_CAP_SAMPLES;
            queue.drain(0..excess);
        }
        if queue.len() >= PLAYER_PREBUFFER_SAMPLES {
            self.primed.store(true, Ordering::Relaxed);
        }
    }

    /// Move up to one mix step into `mix` (summed and clamped), leaving
    /// the feed's own timeline intact when it has less than a step. A
    /// feed still filling its prebuffer contributes silence.
    fn drain_into(&self, mix: &mut [f32]) {
        if !self.primed.load(Ordering::Relaxed) {
            return;
        }
        let mut queue = self.queue.lock().unwrap();
        let n = queue.len().min(mix.len());
        for (i, sample) in queue.drain(0..n).enumerate() {
            mix[i] = (mix[i] + sample).clamp(-1.0, 1.0);
        }
    }

    fn is_empty(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }
}

// ---- application list for the share dialog ----

/// Applications playing audio right now, as seen by the default render
/// device's session manager. Parley itself is filtered out: capturing
/// our own playback is the echo this feature exists to avoid.
///
/// COM apartments are per thread, and this is called from the Flutter
/// platform thread — which Windows has usually already initialised as
/// an STA for shell integration, where `CoInitializeEx(MTA)` fails with
/// `RPC_E_CHANGED_MODE` and the picker would silently be empty. One
/// short-lived worker owns the MTA for this enumeration instead.
pub fn list_audio_players() -> Result<Vec<super::AudioPlayer>, String> {
    std::thread::Builder::new()
        .name("parley-audio-players".into())
        .spawn(list_audio_players_mta)
        .map_err(|e| format!("audio player thread: {e}"))?
        .join()
        .map_err(|_| "audio player enumeration panicked".to_string())?
}

fn list_audio_players_mta() -> Result<Vec<super::AudioPlayer>, String> {
    initialize_mta()
        .ok()
        .map_err(|e| format!("COM init: {e}"))?;
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(|e| e.to_string())?;
    let manager = device
        .get_iaudiosessionmanager()
        .map_err(|e| e.to_string())?;
    let sessions = manager
        .get_audiosessionenumerator()
        .map_err(|e| e.to_string())?;
    let count = sessions.get_count().map_err(|e| e.to_string())?;
    let self_pid = std::process::id();
    let mut players: Vec<super::AudioPlayer> = Vec::new();
    for i in 0..count {
        let Ok(control) = sessions.get_session(i) else {
            continue;
        };
        if control.get_state().ok() != Some(SessionState::Active) {
            continue;
        }
        let Ok(pid) = control.get_process_id() else {
            continue;
        };
        // PID 0 is the system-sounds session: not an application, and
        // not something the picker can name.
        if pid == 0 || pid == self_pid || players.iter().any(|p| p.id == pid) {
            continue;
        }
        let exe = process_name(pid).unwrap_or_else(|| format!("process {pid}"));
        let app = exe.trim_end_matches(".exe").to_string();
        players.push(super::AudioPlayer {
            id: pid,
            name: format!("{exe}#{pid}"),
            app,
            description: format!("PID {pid}"),
        });
    }
    players.sort_by_key(|p| p.app.to_lowercase());
    Ok(players)
}

/// `QueryFullProcessImageNameW`'s file name, without a directory. A PID
/// that exited between enumeration and lookup gets the fallback the
/// caller supplies.
fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok || len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feed_waits_for_its_prebuffer_then_drains_one_step() {
        let feed = PlayerFeed::new();
        feed.push(vec![0.25, -0.25]);
        let mut mix = vec![0.0; 4];
        feed.drain_into(&mut mix);
        assert_eq!(mix, vec![0.0; 4], "still filling its prebuffer");

        feed.push(vec![0.5; PLAYER_PREBUFFER_SAMPLES - 2]);
        let mut mix = vec![0.0; 4];
        feed.drain_into(&mut mix);
        assert_eq!(mix, vec![0.25, -0.25, 0.5, 0.5]);
        assert!(!feed.is_empty(), "the rest stays for the next step");
    }

    #[test]
    fn two_feeds_sum_into_one_step_without_clipping() {
        let a = PlayerFeed::new();
        let b = PlayerFeed::new();
        a.push(vec![0.8; PLAYER_PREBUFFER_SAMPLES]);
        b.push(vec![0.8; PLAYER_PREBUFFER_SAMPLES]);
        let mut mix = vec![0.0; 4];
        a.drain_into(&mut mix);
        b.drain_into(&mut mix);
        assert_eq!(mix, vec![1.0; 4]);
    }

    #[test]
    fn a_feed_drops_its_oldest_audio_past_the_cap() {
        let feed = PlayerFeed::new();
        feed.push(vec![1.0; FEED_CAP_SAMPLES + 8]);
        let queue = feed.queue.lock().unwrap();
        assert_eq!(queue.len(), FEED_CAP_SAMPLES);
        assert_eq!(queue.front(), Some(&1.0));
    }
}
