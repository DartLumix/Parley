//! Video receive display bridge (7f). The protocol half lives in
//! parley-core's `VideoReceiveHub` (assembler, decoder, stopped-set,
//! bounded frame queue — the 3b/6.9 hardening); this module is the
//! SECOND display consumer the hub was built for (6.9: "Phase 7's
//! Flutter consumer replaces exactly this function against the same
//! hub" — the terminal client's mpv feeder being the first).
//!
//! SHAPE: one consumer thread per session drains decoded RGB frames
//! from the hub; per sender it lazily creates an `irondash_texture`
//! external texture whose payload provider serves the LATEST frame
//! (latest-frame-wins — the queue's drop-oldest discipline continues
//! through the bridge), converts RGB24 → RGBA (the Linux embedder's
//! expected pixel order — `PixelData::FORMAT`), and pings
//! `mark_frame_available`; Flutter's raster thread pulls pixels at
//! its own pace. Dart learns the texture id through a `video_started`
//! SessionEvent and renders a plain `Texture` widget; `video_stopped`
//! (ShareStopped / peer voice-leave) removes it. A stopped view is
//! re-ANNOUNCED (same texture id) when the sender's next share
//! starts — no mid-session texture teardown, no widget churn.
//!
//! THREADING (load-bearing — all verified against irondash 0.5
//! sources):
//! - Texture creation and destruction MUST run on the Flutter
//!   platform thread: `EngineContext::get()` asserts the main thread,
//!   and `SendableTexture`'s inner `Capsule` PANICS if dropped
//!   off-thread (Capsule::new has no sender hop). Both hops go
//!   through `EngineContext::perform_on_main_thread`; creation waits
//!   on a channel for the texture id, destruction is fire-and-forget
//!   (the closure owns the map and drops it there) but AFTER the
//!   consumer thread has exited (Drop waits ≤ 2 s for its done
//!   signal) — so the consumer can never be the last owner of a
//!   texture dropped off the platform thread.
//! - `mark_frame_available` goes through `SendableTexture`, which
//!   routes to the platform thread internally — safe from the
//!   consumer thread.
//! - The engine handle comes from Dart
//!   (`irondash_engine_context`'s `EngineContext.instance
//!   .getEngineHandle()`) via `video_prepare`, called once at app
//!   startup. The pub package registers the native plugin whose
//!   symbols the Rust side resolves via dlsym.
//!
//! LIFECYCLE: the bridge is session-owned (the 7e ownership rule —
//! one rig per LiveSession, dropped with it). Shares are routed to
//! voice-channel members, so membership is exactly the receive
//! domain; the hub persists across channel joins within a session and
//! per-sender state follows the voice_state / ShareStopped events.
//! disconnect() drops the LiveSession outside the registry lock —
//! the bridge's bounded teardown wait must never sit under it.
//!
//! FEATURE GATE: only `video_prepare` is ungated (the bridge surface
//! must compile featureless — the 7d voice_start convention); every
//! other item lives behind `video` (openh264 + irondash_texture).

/// One-time startup: validate the engine-context plugin (a main-thread
/// hop to `EngineContext::get()`) and store the engine handle for the
/// texture machinery. Dart calls this right after `RustLib.init()`.
/// Featureless builds return the 7d-convention error string.
pub fn video_prepare(engine_handle: i64) -> Result<(), String> {
    #[cfg(feature = "video")]
    {
        imp::prepare(engine_handle)
    }
    #[cfg(not(feature = "video"))]
    {
        let _ = engine_handle;
        Err("video feature off".into())
    }
}

/// The sharer's own view of the running share (7l). One fixed-size
/// texture per process — the frames are the capture, downscaled and
/// letterboxed by the capture crate, so switching the shared window
/// never re-creates the texture. Dart renders it while sharing and
/// hides it when the user turns the preview off; the Rust side keeps
/// producing only while that switch is on.
#[derive(Debug, Clone)]
pub struct SharePreview {
    pub texture_id: i64,
    pub width: u64,
    pub height: u64,
}

/// Create (once) and describe the preview texture. Call after
/// `video_prepare`; the id is stable for the life of the process.
/// Featureless: the 7d-convention error string. `video` alone is not
/// enough — the frames come from the capture crate, so a receive-only
/// build has no preview to describe.
pub fn share_preview() -> Result<SharePreview, String> {
    #[cfg(feature = "screen-capture")]
    {
        imp::share_preview()
    }
    #[cfg(not(feature = "screen-capture"))]
    {
        Err("screen-capture feature off".into())
    }
}

/// The shape of the running share, for the sharer's own card: capture
/// pixels and the rate the encoder actually paces at (after the profile
/// ceiling and the share dialog's cap). `None` before the first format
/// negotiation, and not shared (somebody else's share).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShareStats {
    pub width: u64,
    pub height: u64,
    pub fps: u64,
}

/// Latest negotiation of OUR share. Written on the capture/encode thread
/// at most once per format change, read by the UI's 1 Hz poll. Kept out
/// of the session snapshot because it is not session state — it is a
/// property of this machine's capture.
#[cfg(feature = "screen-capture")]
static SHARE_INFO: std::sync::Mutex<Option<parley_client::screen::StreamInfo>> =
    std::sync::Mutex::new(None);

/// The running share's negotiated shape, or None when nothing is being
/// shared (or the featureless build has no capture at all).
pub fn share_stats() -> Option<ShareStats> {
    #[cfg(feature = "screen-capture")]
    {
        SHARE_INFO.lock().unwrap().map(|i| ShareStats {
            width: u64::from(i.width),
            height: u64::from(i.height),
            fps: i.fps.round() as u64,
        })
    }
    #[cfg(not(feature = "screen-capture"))]
    {
        None
    }
}

/// Forget the previous share's shape when one starts or ends: the card
/// must never show the last share's resolution while the portal is still
/// open on a new pick.
#[cfg(feature = "screen-capture")]
pub(crate) fn clear_share_stats() {
    *SHARE_INFO.lock().unwrap() = None;
}

/// The capture pipeline's tap, or None on builds without screen capture.
///
/// `preview_enabled` is consulted on the ENCODE thread before a capture
/// frame is even shared, so a preview the user turned off costs one lock
/// read per frame; the pixels are scaled on the preview thread (see
/// `parley_client::screen::ShareTap`).
#[cfg(feature = "screen-capture")]
pub(crate) fn share_tap() -> Option<parley_client::screen::PreviewSink> {
    use parley_client::screen::{PreviewFrame as CaptureFrame, ShareTap, StreamInfo};

    struct AppTap;

    impl ShareTap for AppTap {
        fn preview_enabled(&self) -> bool {
            crate::api::settings::show_share_preview()
        }

        fn stream_info(&self, info: StreamInfo) {
            *SHARE_INFO.lock().unwrap() = Some(info);
        }

        fn preview_frame(&self, frame: CaptureFrame<'_>) {
            if let Err(e) = imp::push_preview(frame) {
                // Once per share is enough — a missing texture is a startup
                // problem, not a per-frame one.
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("preview: {e}");
                }
            }
        }
    }

    Some(std::sync::Arc::new(AppTap))
}

/// Session-owned display bridge (consumer thread + per-sender
/// textures). Re-exported for session.rs; the type itself is opaque
/// to codegen (never in a pub fn signature).
#[cfg(feature = "video")]
pub use imp::DisplayBridge;
// The generated file's top-level code references SessionEvent
// UNQUALIFIED (via `use crate::*`), which only sees `pub` items of
// api modules — so the event type must be re-exported here in every
// build. (Unused in the featureless build — the glob's other members
// cover it — hence the allow.)
#[allow(unused_imports)]
pub use crate::api::session::SessionEvent;

/// Featureless stub: FRB's generated file glob-imports `video::imp`
/// unconditionally AND emits wire glue naming DisplayBridge/
/// SharedVideoReceiveHub/SessionEvent/UnboundedSender (it never sees
/// the cfg), so the module and those names must exist in every build.
/// The stubs are unreachable: the Dart side can only obtain opaque
/// handles the real bridge produced, and featureless builds never
/// construct one (video_prepare errors out; video_started events
/// never fire). The two cfg arms are mutually exclusive.
#[cfg(not(feature = "video"))]
pub mod imp {
    /// Unreachable stub — see module docs.
    #[allow(dead_code)]
    pub struct DisplayBridge;

    #[allow(dead_code)]
    impl DisplayBridge {
        pub fn start(
            _hub: SharedVideoReceiveHub,
            _events: UnboundedSender<SessionEvent>,
            _session_id: u32,
        ) -> Self {
            unreachable!("video feature off")
        }
        pub fn sender_stopped(&self, _sender_id: u32, _forget: bool) {
            unreachable!("video feature off")
        }
        /// 7k: the stall watchdog's expiry hook — see the real impl.
        pub fn hide(&self, _sender_id: u32) {
            unreachable!("video feature off")
        }
        /// See the real impl: the picture is only painted for a share
        /// somebody opened.
        pub fn set_watching(&self, _sender_id: u32, _watching: bool) {
            unreachable!("video feature off")
        }
    }

    /// Stub type mirrors the real hub's name for the generated glue;
    /// never constructed.
    #[allow(dead_code)]
    pub struct SharedVideoReceiveHub;
    pub use crate::api::session::SessionEvent;
    pub use tokio::sync::mpsc::UnboundedSender;
}
// 7g: FRB's generated file references these unqualified (its glob import
// only sees `pub` items of `crate::api::video::imp`). Public re-exports
// so `use crate::*` there resolves them. The glob may itself be unused
// when no opaque signature exists — hence the allows.
#[allow(unused_imports)]
#[cfg(feature = "video")]
pub use parley_core::video_receive::SharedVideoReceiveHub;
#[allow(unused_imports)]
#[cfg(feature = "video")]
pub use tokio::sync::mpsc::UnboundedSender;

#[cfg(feature = "video")]
pub mod imp {
    // NOTE: pub — FRB codegen emits `use crate::api::video::imp::*` and
    // DisplayBridge wire glue into frb_generated.rs (7g). The wire glue
    // references Hub/Event/Sender types UNQUALIFIED at module level in
    // imp (its own items shadow the glob), so also make them visible
    // inside imp via pub use here.
    pub use super::SessionEvent;
    use irondash_engine_context::EngineContext;
    use irondash_texture::{BoxedPixelData, PayloadProvider, SendableTexture, SimplePixelData};
    pub use parley_core::video_receive::{SharedVideoReceiveHub, VideoFrame};
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;
    pub use tokio::sync::mpsc::UnboundedSender;

    use crate::api::session::VideoSurfaceInfo;

    static ENGINE_HANDLE: OnceLock<i64> = OnceLock::new();

    /// 7g: alias the opaque sender type FRB names in its glue (the
    /// generated file writes `UnboundedSenderSessionEvent`). Kept
    /// name-only: codegen may or may not reference it per cfg set.
    #[allow(dead_code)]
    pub(crate) type UnboundedSenderSessionEvent = tokio::sync::mpsc::UnboundedSender<SessionEvent>;

    /// The sharer's own preview: one texture, created on first use on
    /// the platform thread, fed latest-frame-wins exactly like a remote
    /// sender's view. Gated on screen-capture: a receive-only build has
    /// no capture frames to feed it (and cannot name the frame type).
    #[cfg(feature = "screen-capture")]
    static PREVIEW: Mutex<Option<Arc<PreviewView>>> = Mutex::new(None);

    /// The preview canvas's current shape, packed as `w << 32 | h` and
    /// written by the preview thread in `push_preview`. Zero until the
    /// first frame arrives; the seeded placeholder is 640x360. The
    /// canvas follows the STREAM (see the capture crate's
    /// `preview_target`), so the UI's aspect and fallback dimensions
    /// have to ask rather than assume.
    #[cfg(feature = "screen-capture")]
    static PREVIEW_SIZE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[cfg(feature = "screen-capture")]
    struct PreviewView {
        texture: Arc<SendableTexture<BoxedPixelData>>,
        provider: Arc<FrameProvider>,
        texture_id: i64,
    }

    #[cfg(feature = "screen-capture")]
    pub(super) fn share_preview() -> Result<super::SharePreview, String> {
        let view = preview_view()?;
        let packed = PREVIEW_SIZE.load(Ordering::Relaxed);
        let (width, height) = if packed == 0 {
            // Before the first frame: the seeded canvas. A share that is
            // already running reports its real shape on the next read.
            (
                parley_client::screen::PREVIEW_WIDTH as u64,
                parley_client::screen::PREVIEW_HEIGHT as u64,
            )
        } else {
            (packed >> 32, packed & 0xffff_ffff)
        };
        Ok(super::SharePreview {
            texture_id: view.texture_id,
            width,
            height,
        })
    }

    #[cfg(feature = "screen-capture")]
    fn preview_view() -> Result<Arc<PreviewView>, String> {
        let mut slot = PREVIEW.lock().unwrap();
        if let Some(view) = slot.as_ref() {
            return Ok(view.clone());
        }
        let handle = *ENGINE_HANDLE
            .get()
            .ok_or("video_prepare was never called")?;
        let (w, h) = (
            parley_client::screen::PREVIEW_WIDTH,
            parley_client::screen::PREVIEW_HEIGHT,
        );
        // Opaque black until the first capture frame lands — the pane
        // shows a placeholder, not a 1x1 white flash.
        let provider = Arc::new(FrameProvider::new(
            w as i32,
            h as i32,
            [0, 0, 0, 255].repeat(w * h),
        ));
        let (tx, rx) = std::sync::mpsc::channel::<
            Result<(i64, Arc<SendableTexture<BoxedPixelData>>), String>,
        >();
        let provider_for_main = provider.clone();
        EngineContext::perform_on_main_thread(move || {
            if let Err(e) = EngineContext::get() {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
            match irondash_texture::Texture::new_with_provider(handle, provider_for_main) {
                Ok(texture) => {
                    let id = texture.id();
                    let _ = tx.send(Ok((id, texture.into_sendable_texture())));
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
        })
        .map_err(|e| format!("main-thread hop: {e}"))?;
        let (texture_id, texture) = rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|e| format!("preview texture: {e}"))?
            .map_err(|e| format!("preview texture: {e}"))?;
        let view = Arc::new(PreviewView {
            texture,
            provider,
            texture_id,
        });
        *slot = Some(view.clone());
        Ok(view)
    }

    /// The preview thread's RGBA canvas → the preview texture's slot.
    /// Latest-frame-wins: a frame the raster thread has not pulled yet is
    /// dropped here, which is what keeps the preview from queueing behind
    /// a slow compositor. The swizzle already happened in the capture
    /// crate, so this is one `memcpy` and a hand-off.
    #[cfg(feature = "screen-capture")]
    pub(super) fn push_preview(
        frame: parley_client::screen::PreviewFrame<'_>,
    ) -> Result<(), String> {
        let view = preview_view()?;
        let need = frame.width * frame.height * 4;
        if frame.rgba.len() != need {
            return Err(format!(
                "preview frame is {} bytes, expected {need}",
                frame.rgba.len()
            ));
        }
        PREVIEW_SIZE.store(
            ((frame.width as u64) << 32) | frame.height as u64,
            Ordering::Relaxed,
        );
        let mut rgba = view.provider.take_buffer();
        if rgba.len() != need {
            rgba.resize(need, 255);
        }
        rgba.copy_from_slice(frame.rgba);
        view.provider
            .put_frame(frame.width as i32, frame.height as i32, rgba);
        view.texture.mark_frame_available();
        Ok(())
    }

    pub(super) fn prepare(engine_handle: i64) -> Result<(), String> {
        // Plugin-presence check must run on the platform thread; hop
        // there once and wait, so failures surface to Dart NOW instead
        // of at first frame.
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        EngineContext::perform_on_main_thread(move || {
            let res = EngineContext::get()
                .map(|_| ())
                .map_err(|e| format!("engine context: {e}"));
            let _ = tx.send(res);
        })
        .map_err(|e| format!("main-thread hop: {e}"))?;
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|e| format!("engine context check: {e}"))??;
        ENGINE_HANDLE
            .set(engine_handle)
            .map_err(|_| "video_prepare called twice".to_string())
    }

    /// Latest-frame slot behind the texture's payload provider. The
    /// raster thread clones the buffer out (one frame copy per
    /// displayed frame — unavoidable: a payload owns its bytes — but
    /// the producer no longer allocates one).
    ///
    /// The slot must NEVER be empty while a texture exists. Recycling its
    /// buffer by `take()`ing the slot (the first version of this) left it
    /// empty for the whole RGB→RGBA conversion — milliseconds at 1080p —
    /// and `get_payload` answered those pulls with a 1x1 black pixel, so
    /// the viewer saw the shared screen flash black for a single frame.
    /// Conversion therefore happens in a separate `spare` buffer that is
    /// swapped in under the lock.
    struct FrameProvider {
        frame: Mutex<Option<(i32, i32, Vec<u8>)>>,
        spare: Mutex<Vec<u8>>,
    }

    impl FrameProvider {
        fn new(width: i32, height: i32, rgba: Vec<u8>) -> Self {
            Self {
                frame: Mutex::new(Some((width, height, rgba))),
                spare: Mutex::new(Vec::new()),
            }
        }

        /// 7h HD: hand the producer the previous frame's buffer, so the
        /// next RGB→RGBA conversion writes into it. At 1080p the old
        /// code allocated 8.3 MB per frame — at the 48 fps a hardware
        /// encoder now delivers that is ~400 MB/s of allocator traffic
        /// for a buffer that is overwritten one frame later.
        ///
        /// Empty only until the first `put_frame` hands one back; the
        /// conversion resizes in that case.
        fn take_buffer(&self) -> Vec<u8> {
            std::mem::take(&mut *self.spare.lock().unwrap())
        }

        /// Publish the newest frame (latest-frame-wins: whatever the
        /// raster thread has not pulled yet is dropped here). The frame
        /// being replaced becomes the next conversion buffer.
        fn put_frame(&self, width: i32, height: i32, rgba: Vec<u8>) {
            let old = self.frame.lock().unwrap().replace((width, height, rgba));
            if let Some((_, _, old)) = old {
                *self.spare.lock().unwrap() = old;
            }
        }
    }

    /// Pulls that found no frame. Unreachable since the slot is seeded on
    /// creation and never emptied — counted so a regression is a log line
    /// rather than a mystery flash in someone's shared screen.
    static EMPTY_PULLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    impl PayloadProvider<BoxedPixelData> for FrameProvider {
        fn get_payload(&self) -> BoxedPixelData {
            let slot = self.frame.lock().unwrap();
            match &*slot {
                Some((w, h, rgba)) => SimplePixelData::new_boxed(*w, *h, rgba.clone()),
                None => {
                    let n = EMPTY_PULLS.fetch_add(1, Ordering::Relaxed) + 1;
                    if n == 1 || n.is_multiple_of(100) {
                        eprintln!(
                            "video: empty frame slot pulled {n}x — showing a 1x1 black frame \
                             (display-bridge race)"
                        );
                    }
                    SimplePixelData::new_boxed(1, 1, vec![0, 0, 0, 255])
                }
            }
        }
    }

    /// Per-sender display state. Created ON the platform thread (the
    /// texture inside); the map entry is only ever destroyed on the
    /// platform thread (bridge teardown) — no mid-session drops, ever
    /// (see the module docs).
    struct SenderView {
        texture: Arc<SendableTexture<BoxedPixelData>>,
        provider: Arc<FrameProvider>,
        /// The texture id (captured at creation; SendableTexture does
        /// not re-expose it) — re-sent on re-announce.
        texture_id: i64,
        /// Last displayed dimensions (a resolution change is logged;
        /// SimplePixelData carries its own dims so Flutter scales).
        dims: (usize, usize),
    }

    /// One session's video display bridge. Drop = stop flag → consumer
    /// exit (waited, bounded) → texture map cleared on the platform
    /// thread (unregisters every texture — Drop-based unregister only
    /// works there).
    ///
    /// `pub` within this PRIVATE module (capped by it) so the
    /// `pub(crate) use` re-export below is legal.
    pub struct DisplayBridge {
        stop: Arc<AtomicBool>,
        done_rx: Mutex<std::sync::mpsc::Receiver<()>>,
        views: Arc<Mutex<HashMap<u32, SenderView>>>,
        /// Senders whose display is currently hidden (ShareStopped /
        /// voice-leave). Re-announced on their next frame.
        hidden: Arc<Mutex<HashSet<u32>>>,
        /// Senders whose picture is on screen (a tile somebody opened).
        /// A closed tile costs no per-frame work at all: the decoder keeps
        /// the stream warm so opening one is instant, but the pixels are
        /// never converted, uploaded or handed to the raster thread, and
        /// nothing paints them.
        watching: Arc<Mutex<HashSet<u32>>>,
        hub: SharedVideoReceiveHub,
    }

    // (SenderView is private to this module; session.rs only ever
    // calls DisplayBridge::start / sender_stopped / Drop.)

    impl DisplayBridge {
        /// Spawn the consumer thread. `events` is the session's event
        /// channel (video_started flows through it); `hub` the receive
        /// hub the session's video task feeds.
        pub fn start(
            hub: SharedVideoReceiveHub,
            events: tokio::sync::mpsc::UnboundedSender<SessionEvent>,
            session_id: u32,
        ) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let views: Arc<Mutex<HashMap<u32, SenderView>>> = Arc::new(Mutex::new(HashMap::new()));
            let hidden: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
            let watching: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            {
                let stop = stop.clone();
                let views = views.clone();
                let hidden = hidden.clone();
                let watching = watching.clone();
                let hub_for_thread = hub.clone();
                std::thread::Builder::new()
                    .name("parley-video-display".into())
                    .spawn(move || {
                        consumer_loop(
                            hub_for_thread,
                            events,
                            session_id,
                            stop,
                            views,
                            hidden,
                            watching,
                        );
                        let _ = done_tx.send(()); // no texture clones exist past this
                    })
                    .expect("spawn video display thread");
            }
            Self {
                stop,
                done_rx: Mutex::new(done_rx),
                views,
                hidden,
                watching,
                hub,
            }
        }

        /// A sender stopped sharing (`forget = false`: hub suppresses
        /// stragglers until their next frame_id-0 keyframe) or left
        /// voice entirely (`forget = true`: hub forgets them). The
        /// view is KEPT and marked hidden — re-announced on their next
        /// frame — so textures are only ever destroyed at teardown.
        /// 7k: the sender's stream has gone quiet (no frames for long
        /// enough that it cannot be a hiccup) — hide the surface so the
        /// card goes away, WITHOUT declaring the share stopped. The hub
        /// keeps decoding, so frames that resume re-announce the same
        /// texture through the consumer's hidden-set path.
        pub fn hide(&self, sender_id: u32) {
            self.hidden.lock().unwrap().insert(sender_id);
            // Nothing on screen: stop paying for its pixels too.
            self.watching.lock().unwrap().remove(&sender_id);
            // Frames already decoded would arrive as "a hidden sender is
            // sending again" and re-announce the surface — the card
            // would flicker back the moment it was expired.
            self.hub.discard_queued(sender_id);
        }

        pub fn sender_stopped(&self, sender_id: u32, forget: bool) {
            if forget {
                self.hub.remove(sender_id);
            } else {
                self.hub.mark_stopped(sender_id);
            }
            self.hidden.lock().unwrap().insert(sender_id);
            self.watching.lock().unwrap().remove(&sender_id);
        }

        /// Show (or stop showing) one sender's picture. Called when a tile
        /// is opened or closed — the UI never renders an unopened share, so
        /// there is no reason to convert, upload or paint its frames.
        ///
        /// The decoder is deliberately NOT gated: a stream decoded from its
        /// first keyframe is what makes opening a tile show a picture on the
        /// next frame instead of waiting for the next IDR. What is gated is
        /// everything downstream of the decode, which on a 1080p share is
        /// the 8 MB RGBA conversion and the platform-thread texture upload
        /// per frame.
        pub fn set_watching(&self, sender_id: u32, watching: bool) {
            let mut set = self.watching.lock().unwrap();
            if watching {
                set.insert(sender_id);
            } else {
                set.remove(&sender_id);
            }
        }
    }

    impl Drop for DisplayBridge {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // Bounded wait for the consumer to exit: afterwards no
            // texture clones exist outside the map, so clearing the
            // map ON the platform thread is the only drop site.
            let _ = self
                .done_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2));
            let views = self.views.clone();
            let _ = EngineContext::perform_on_main_thread(move || {
                views.lock().unwrap().clear();
            });
        }
    }

    /// RGB24 → RGBA (the Linux embedder's pixel order).
    fn rgb24_to_rgba(rgb: &[u8]) -> Vec<u8> {
        let mut rgba = Vec::new();
        rgb24_to_rgba_into(rgb, &mut rgba);
        rgba
    }

    /// 7h HD: the conversion into a REUSED buffer (see
    /// [`FrameProvider::take_buffer`]). A correctly sized buffer is
    /// overwritten in full, so only a geometry change resizes it.
    fn rgb24_to_rgba_into(rgb: &[u8], out: &mut Vec<u8>) {
        let need = rgb.len() / 3 * 4;
        if out.len() != need {
            out.clear();
            out.resize(need, 255);
        }
        for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
            out[i * 4] = px[0];
            out[i * 4 + 1] = px[1];
            out[i * 4 + 2] = px[2];
            out[i * 4 + 3] = 255;
        }
    }

    /// The display consumer: hub frames → per-sender textures. Same
    /// skeleton as the terminal client's mpv_consumer (1 s poll, dead
    /// set), minus mpv: instead of a child process, a texture + frame
    /// slot. Exits when `stop` flips (checked around the ≤1 s pop).
    fn consumer_loop(
        hub: SharedVideoReceiveHub,
        events: tokio::sync::mpsc::UnboundedSender<SessionEvent>,
        session_id: u32,
        stop: Arc<AtomicBool>,
        views: Arc<Mutex<HashMap<u32, SenderView>>>,
        hidden: Arc<Mutex<HashSet<u32>>>,
        watching: Arc<Mutex<HashSet<u32>>>,
    ) {
        // Senders whose texture creation failed — no retry until the
        // bridge dies (the terminal client's dead-set rule; spam-free).
        // 7h: senders whose texture creation failed, and when. The failure
        // is nearly always the 2 s platform-thread hop timing out, not a
        // permanent condition — the old rule (dead for the life of the
        // session) turned one slow hop into a viewer that never saw
        // anything. Retried after a cooldown instead.
        let mut dead: HashMap<u32, std::time::Instant> = HashMap::new();
        // Per-sender arrival cadence (see `Cadence`).
        let mut rate: HashMap<u32, Cadence> = HashMap::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let Some(frame) = hub.pop_frame(Duration::from_secs(1)) else {
                continue; // idle poll; also the stop-flag heartbeat
            };
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let sender = frame.sender_id;
            // 7k: a straggler from a share that already ended (or from a
            // sender who left voice) must not reach the display — the
            // consumer would treat it as a hidden sender sending again
            // and re-announce the surface, resurrecting the card the
            // stop notice just removed.
            if hub.is_stopped(sender) {
                continue;
            }
            if let Some(&when) = dead.get(&sender) {
                if when.elapsed() < TEXTURE_RETRY_COOLDOWN {
                    continue; // keep draining, display nothing
                }
                dead.remove(&sender);
            }
            // A previously-hidden sender sending frames again = a NEW
            // share (the hub only un-suppresses on a frame_id-0
            // keyframe): re-announce the SAME texture.
            let reannounce = hidden.lock().unwrap().remove(&sender);
            // Ensure the sender has a texture (lazy, sized from the
            // first decoded frame — the sender's real resolution).
            let existing = views
                .lock()
                .unwrap()
                .get(&sender)
                .map(|v| (v.texture.clone(), v.provider.clone(), v.dims, v.texture_id));
            let first_view = existing.is_none();
            let (texture, provider, dims, texture_id) = match existing {
                Some(x) => x,
                None => match create_texture(&frame) {
                    Ok((texture, provider, id)) => {
                        println!(
                            "video: texture for user {sender}: {}x{} (id {id})",
                            frame.width, frame.height
                        );
                        let v = SenderView {
                            texture: texture.clone(),
                            provider: provider.clone(),
                            texture_id: id,
                            dims: (frame.width, frame.height),
                        };
                        views.lock().unwrap().insert(sender, v);
                        (texture, provider, (frame.width, frame.height), id)
                    }
                    Err(e) => {
                        eprintln!(
                            "video: texture creation failed for user {sender}: {e} (retrying in \
                             {} s)",
                            TEXTURE_RETRY_COOLDOWN.as_secs()
                        );
                        dead.insert(sender, std::time::Instant::now());
                        continue;
                    }
                },
            };
            // Provisional cadence until a measurement window closes: the
            // stream's rate flag is the sender's own 30/60 announcement.
            let provisional_fps = if frame.high_rate { 60 } else { 30 };
            let rate = rate.entry(sender).or_insert(Cadence {
                start: std::time::Instant::now(),
                frames: 0,
                shown: provisional_fps,
            });
            if reannounce || first_view {
                let _ = events.send(surface_event(
                    session_id,
                    "video_started",
                    sender,
                    &frame,
                    rate.shown,
                    texture_id,
                ));
            }
            if (frame.width, frame.height) != dims {
                println!(
                    "video: user {sender} resolution changed to {}x{}",
                    frame.width, frame.height
                );
                views.lock().unwrap().get_mut(&sender).unwrap().dims = (frame.width, frame.height);
                // The card's "1920×1080" must follow a window resize;
                // same texture, new dimensions.
                let _ = events.send(surface_event(
                    session_id,
                    "video_stats",
                    sender,
                    &frame,
                    rate.shown,
                    texture_id,
                ));
            }
            // Latest-frame-wins into the slot, then ping Flutter to
            // pull at its next raster (routed to the platform thread
            // by SendableTexture). The conversion writes into the slot
            // buffer the previous frame left behind — no per-frame
            // allocation on the consumer thread.	            // Nothing on screen: no conversion, no upload, no raster
            // ping. The frames were decoded for the tile's size/rate
            // label and for an instant first picture when it opens.
            if watching.lock().unwrap().contains(&sender) {
                let mut rgba = provider.take_buffer();
                rgb24_to_rgba_into(&frame.rgb, &mut rgba);
                provider.put_frame(frame.width as i32, frame.height as i32, rgba);
                // SendableTexture's mark_frame_available has no error
                // channel (it returns unit — the run-loop hop's failure is
                // swallowed there), so a torn-down engine surfaces as a
                // frozen texture rather than a log line. Nothing to check
                // here; the 7h consumer/decoder accounting remains the way
                // to tell "frames arrived" from "pictures were shown".
                texture.mark_frame_available();
            }

            // Cadence, measured where the frames actually reach the
            // display: a sender whose encoder is limping shows its real
            // rate, not the profile it was configured for. One window per
            // second, and an event only when the number moves — a 60 fps
            // stream that measures 59 must not rewrite the UI every second.
            rate.frames += 1;
            let elapsed = rate.start.elapsed();
            if elapsed >= Duration::from_secs(1) {
                let raw = f64::from(rate.frames) / elapsed.as_secs_f64();
                // Nearest 5 fps: one burst after a slow decode must not
                // print "32 fps" for a 30 fps stream.
                let measured = ((raw / 5.0).round() * 5.0) as u64;
                rate.start = std::time::Instant::now();
                rate.frames = 0;
                if measured > 0 && measured.abs_diff(rate.shown) >= 2 {
                    rate.shown = measured;
                    let _ = events.send(surface_event(
                        session_id,
                        "video_stats",
                        sender,
                        &frame,
                        measured,
                        texture_id,
                    ));
                }
            }
        }
    }

    /// One sender's arrival cadence, for the "60 fps" label and a
    /// resolution-change refresh.
    struct Cadence {
        start: std::time::Instant,
        frames: u32,
        /// The number the UI currently shows (starts at the stream's own
        /// 30/60 flag, replaced by the first measured window).
        shown: u64,
    }

    /// A `video_started` / `video_stats` event carrying one surface.
    fn surface_event(
        session_id: u32,
        kind: &str,
        sender: u32,
        frame: &VideoFrame,
        fps: u64,
        texture_id: i64,
    ) -> SessionEvent {
        SessionEvent {
            session_id,
            kind: kind.into(),
            channels: Vec::new(),
            users: Vec::new(),
            messages: Vec::new(),
            user_id: sender,
            username: String::new(),
            channel_id: 0,
            joined: false,
            muted: false,
            states: Vec::new(),
            surface: Some(VideoSurfaceInfo {
                user_id: sender,
                width: frame.width as u64,
                height: frame.height as u64,
                fps,
                texture_id,
            }),
        }
    }

    /// 7h: how long a sender stays off-screen after a failed texture
    /// creation before the consumer tries again.
    const TEXTURE_RETRY_COOLDOWN: Duration = Duration::from_secs(10);

    /// What texture creation hands back: the sendable handle (for
    /// mark_frame_available), the frame slot, and the Flutter texture
    /// id (for the video_started event).
    type CreatedTexture = (
        Arc<SendableTexture<BoxedPixelData>>,
        Arc<FrameProvider>,
        i64,
    );

    /// Create one texture for `frame`'s sender: provider seeded with
    /// the frame, texture built on the platform thread (waiting for
    /// the id), converted to a SendableTexture for the consumer loop.
    fn create_texture(frame: &VideoFrame) -> Result<CreatedTexture, String> {
        let handle = *ENGINE_HANDLE
            .get()
            .ok_or("video_prepare was never called")?;
        let provider = Arc::new(FrameProvider::new(
            frame.width as i32,
            frame.height as i32,
            rgb24_to_rgba(&frame.rgb),
        ));
        let (tx, rx) = std::sync::mpsc::channel::<
            Result<(i64, Arc<SendableTexture<BoxedPixelData>>), String>,
        >();
        let provider_for_main = provider.clone();
        EngineContext::perform_on_main_thread(move || {
            // Asserts the platform thread — true inside this hop.
            if let Err(e) = EngineContext::get() {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
            let texture =
                match irondash_texture::Texture::new_with_provider(handle, provider_for_main) {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                };
            let id = texture.id();
            // into_sendable_texture already returns Arc<SendableTexture>.
            let sendable = texture.into_sendable_texture();
            // The SendableTexture's RunLoopSender is bound to THIS
            // (platform) thread — constructing it here is what makes
            // later mark_frame_available calls safe from any thread.
            let _ = tx.send(Ok((id, sendable)));
        })
        .map_err(|e| format!("main-thread hop: {e}"))?;
        let (id, sendable) = rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|e| format!("texture creation: {e}"))?
            .map_err(|e| format!("texture creation: {e}"))?;
        Ok((sendable, provider, id))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// What a raster pull sees is the slot, and the slot must never
        /// go empty: the regression this guards against emptied it to
        /// recycle the buffer, so a pull during the conversion answered
        /// 1x1 black — the shared screen flashing black for one frame.
        #[test]
        fn payload_is_never_empty_while_converting() {
            let provider = FrameProvider::new(4, 2, vec![7u8; 32]);
            let seeded = provider.get_payload();
            assert_eq!((seeded.get().width, seeded.get().height), (4, 2));

            // The producer takes its conversion buffer; the last frame
            // must still be on offer meanwhile.
            let _spare = provider.take_buffer();
            let during = provider.get_payload();
            assert_eq!((during.get().width, during.get().height), (4, 2));
            assert_eq!(during.get().data, &[7u8; 32][..]);

            // Publishing swaps the buffers and hands the replaced one
            // back for the next conversion.
            provider.put_frame(4, 2, vec![9u8; 32]);
            let after = provider.get_payload();
            assert_eq!(after.get().data, &[9u8; 32][..]);
            assert_eq!(provider.take_buffer(), vec![7u8; 32]);
            assert_eq!(EMPTY_PULLS.load(Ordering::Relaxed), 0);
        }
    }
}
