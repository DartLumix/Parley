//! Phase 5d/6a/6.9: real screen capture through the XDG ScreenCast
//! portal, with a full stop/restart lifecycle — plus share audio.
//!
//! ashpd drives the portal (user picks a monitor or window in the
//! compositor's picker) and hands back a PipeWire node id + fd. A
//! dedicated thread runs the pipewire loop; its process callback only
//! memcpy's the frame and try_sends it into a bounded channel — if the
//! encoder falls behind, frames are dropped, the pw graph is never
//! stalled. A second thread owns the encoder (5e: a Box<dyn
//! VideoEncoder> from create_encoder — software today, hardware
//! backends later): encode → fragment → paced datagrams on the
//! existing FLAG_VIDEO path.
//!
//! 6a lifecycle: `start()` runs the portal dance, spawns both threads,
//! hands the portal Session to a supervisor task, and returns a
//! ShareHandle (keyframe pokes) + ShareController (stop requests).
//! The supervisor is the single owner of the Session (it's not Clone):
//! it selects over stop requests and the portal's Closed signal (the
//! user hitting stop in the desktop's sharing indicator), then tears
//! down in order — pw loop stop → thread joins → session close — and
//! reports ShareEvent::Ended. Only after Ended does the client send
//! StopShare on the control stream, so no video datagram can follow
//! the stop notice.
//!
//! The pw thread runs the loop as iterate(Finite(100 ms)) + a stop
//! flag instead of run(): MainLoopBox is !Sync, so quit() can't be
//! called from another thread. A 100 ms poll costs nothing (the loop
//! is otherwise idle) and stop latency is bounded by one slice. All
//! Box wrappers (StreamBox/CoreBox/ContextBox/MainLoopBox) destroy
//! their C objects on Drop, in exactly the reverse of declaration
//! order — teardown is scope exit.
//!
//! Encoder-thread output discipline (learned the hard way):
//! - Rate pacing: mpv plays piped rawvideo at its declared fps (30);
//!   encoding faster than that accumulates unbounded queue drift
//!   (~3 s glass-to-glass measured over one session). Sleep to the
//!   interval — never drop for rate.
//! - Skip-to-newest: damage bursts queue stale captures; encoding one
//!   while a fresher sits behind it wastes ~30 ms of CPU on the past.
//!
//! Keyframe-on-join: main.rs holds a ShareHandle and pokes it when a
//! VoiceState says a viewer joined — drop-until-IDR would otherwise
//! cost a late joiner up to one full intra period (2 s). The request
//! rides an Arc<AtomicBool>, NOT the frame channel — a bounded media
//! channel silently swallows try_send'd control under saturation.
//!
//! Only BGRx and BGRA are requested — both are byte-compatible with
//! the BGRA input the encoders take. The wire format is untouched;
//! the negotiated size is whatever the compositor serves (the viewer
//! sizes itself from the H.264 SPS).
//!
//! 7j DMA-BUF capture (`--dmabuf`, behind the `vaapi` feature) is the
//! other half of that request: instead of asking for a shared-memory copy
//! and encoding it, the stream asks for the compositor's OWN buffers (a
//! `Buffers` param whose memory-type mask is `1 << SPA_DATA_DmaBuf`, plus
//! the format's modifier property) and the encoder imports the descriptor.
//! No pixels cross the CPU on that path.
//!
//! The catch that shapes the code: PipeWire guarantees a buffer only until
//! the process callback queues it back, and an import is read by the GPU
//! later. So in DMA-BUF mode the encoder lives ON THE CAPTURE THREAD
//! (`UserData::inline`) and the frame is encoded inside the callback — the
//! buffer cannot be handed to another thread and forgotten. That is also
//! why pacing there drops an early frame instead of sleeping: there is no
//! queue to sleep in front of. The memfd path keeps its channel and its
//! separate encoder thread, unchanged.
//!
//! A request this build or run cannot honor is refused before the portal
//! opens (`start`), because a silent downgrade would be measured as
//! zero-copy while copying.
//!
//! Share audio (6.9): `--share-audio` captures the DEFAULT OUTPUT via
//! PipeWire's native `stream.capture.sink` (no portal involvement —
//! the xdg ScreenCast spec has no audio; see the handoff ledger). The
//! capture is THIS OUTPUT'S audio, not a specific window's (PipeWire
//! monitor scope; documented product behavior). It rides the music
//! profile: stereo, 256 kbps, no FEC/ladder — share audio is
//! fidelity-class content. The mux ([system + mic] → one stream) lives
//! here too; main.rs routes the mic to it via a tee while sharing.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Duration;

use crate::video::VideoLadder;
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use futures_util::StreamExt;
use parley_core::audio::pipeline::SendPipeline;
use parley_core::video::{VideoEncoder, bitrate_for, create_encoder_auto, target_fps};
use parley_proto::video::fragment_frame_with_max;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;

use crate::ShareEvent;

/// Messages from the pipewire thread to the encoder thread. One
/// channel keeps them ordered — format always arrives before frames.
enum ToEncoder {
    /// Negotiated stream format.
    Negotiated { width: u32, height: u32, fps: f64 },
    /// 7g: one BGRx/BGRA frame AS MAPPED — row stride kept (bytes),
    /// padding bytes included. The encoder converts in place from the
    /// buffer via the strided SIMD path; no tight repack here.
    Frame { data: Vec<u8>, stride: usize },
}

/// Handle for poking the capture pipeline from the control reader
/// (keyframe-on-join). NOT a channel message: the frame channel is
/// saturated under load and try_send would silently drop the request —
/// an atomic flag cannot be lost.
#[derive(Clone)]
pub struct ShareHandle {
    flag: Arc<AtomicBool>,
}

impl ShareHandle {
    /// Ask the encoder to emit an IDR next — a late viewer's
    /// drop-until-IDR wait collapses from up to 2 s to ~RTT.
    pub fn force_keyframe(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

/// Stop switch for a running share.
///
/// `request_stop` only ASKS: completion is reported later as
/// ShareEvent::Ended (after full teardown), and only then does the
/// client send StopShare — so no video datagram can follow the stop
/// notice. Returns false if the supervisor is already gone (Ended was
/// sent or is on its way).
#[derive(Clone)]
pub struct ShareController {
    stop_tx: tokio::sync::mpsc::UnboundedSender<()>,
}

impl ShareController {
    pub fn request_stop(&self) -> bool {
        self.stop_tx.send(()).is_ok()
    }
}

/// Listener user data, owned by the pw loop thread.
#[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
struct UserData {
    format: spa::param::video::VideoInfoRaw,
    accepted: bool,
    width: u32,
    height: u32,
    tx: SyncSender<ToEncoder>,
    drops: u64,
    /// 7j: DMA-BUF capture. Some ONLY in that mode, and in that mode it
    /// also holds the encoder: the producer's buffer is guaranteed valid
    /// only until this callback queues it back, so the GPU has to be done
    /// with it before we return — which means encoding here, not on the
    /// encoder thread. `None` keeps the proven memfd + channel path.
    inline: Option<ShareEncoder>,
    /// Pixel format of the DMA-BUF the producer negotiated, as a
    /// `DRM_FORMAT_*` code. Only meaningful with `inline`.
    drm_format: u32,
    /// The negotiated format modifier, passed through to the importer.
    modifier: u64,
}

/// Everything the sender half needs, shared by both capture paths.
///
/// The memfd path owns one on `encoder_thread`; the DMA-BUF path owns one
/// inside the PipeWire listener (see `UserData::inline`). One
/// implementation, so a fix to pacing, the ladder, timestamps or
/// fragmentation reaches both.
struct ShareEncoder {
    conn: quinn::Connection,
    encoder: Option<Box<dyn VideoEncoder>>,
    ladder: VideoLadder,
    fps: f64,
    seq: u16,
    frame_id: u32,
    annexb: Vec<u8>,
    max_payload: usize,
    encoded: u64,
    stale_skipped: u64,
    window: std::time::Instant,
    /// Start of the last encode — what both pacing rules measure from.
    last_encode: Option<std::time::Instant>,
    die: tokio::sync::mpsc::UnboundedSender<()>,
    force_idr: Arc<AtomicBool>,
    video_loss: Arc<AtomicU32>,
}

/// The connection-side handles the capture thread needs when it owns the
/// encoder (DMA-BUF mode). Cloned from `start`'s arguments rather than
/// shared through a channel, because in that mode the capture thread IS the
/// encoding thread.
#[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
struct ShareSenderArgs {
    conn: quinn::Connection,
    die: tokio::sync::mpsc::UnboundedSender<()>,
    force_idr: Arc<AtomicBool>,
    video_loss: Arc<AtomicU32>,
}

/// The pipewire side, on its own thread: loop, stream, listener.
///
/// Setup outcome is reported through `setup` (a oneshot) so `start()`
/// can surface failures instead of stranding a live portal session.
/// After setup the loop polls in 100 ms slices until the stop flag is
/// set, then disconnects and returns (scope exit drops stream, core,
/// context, mainloop — in reverse declaration order, which is the
/// required C teardown order).
fn pw_thread(
    node_id: u32,
    fd: OwnedFd,
    tx: SyncSender<ToEncoder>,
    stop: Arc<AtomicBool>,
    setup: tokio::sync::oneshot::Sender<Result<(), pw::Error>>,
    dmabuf: bool,
    sender: ShareSenderArgs,
) {
    // MainLoopBox::new calls pw::init() itself (refcounted in C, no
    // deinit exists — repeated init across share restarts is the
    // designed pattern).
    let mainloop = match pw::main_loop::MainLoopBox::new(None) {
        Ok(m) => m,
        Err(e) => {
            let _ = setup.send(Err(e));
            return;
        }
    };
    let context = match pw::context::ContextBox::new(mainloop.loop_(), None) {
        Ok(c) => c,
        Err(e) => {
            let _ = setup.send(Err(e));
            return;
        }
    };
    let core = match context.connect_fd(fd, None) {
        Ok(c) => c,
        Err(e) => {
            let _ = setup.send(Err(e));
            return;
        }
    };

    // 7j: DMA-BUF mode builds its encoder HERE, on the thread that will
    // use it (libva contexts are thread-bound), and the format pod below
    // asks for the producer's own buffers. A request this build cannot
    // honor — no `vaapi` feature — was already refused by `start`.
    #[cfg(feature = "vaapi")]
    let inline = dmabuf.then(|| ShareEncoder::new(sender));
    #[cfg(not(feature = "vaapi"))]
    let inline = {
        let _ = (dmabuf, sender);
        None::<ShareEncoder>
    };

    let data = UserData {
        format: Default::default(),
        accepted: false,
        width: 0,
        height: 0,
        tx,
        drops: 0,
        inline,
        drm_format: 0,
        modifier: 0,
    };

    let stream = match pw::stream::StreamBox::new(
        &core,
        "parley-share",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            let _ = setup.send(Err(e));
            return;
        }
    };

    let _listener = match stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _, old, new| {
            println!("share: pw stream {:?} -> {:?}", old, new);
        })
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) = match spa::param::format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if user_data.format.parse(param).is_err() {
                return;
            }
            let raw = user_data.format.format().as_raw();
            let bgra = raw == spa::param::video::VideoFormat::BGRA.as_raw();
            if !bgra && raw != spa::param::video::VideoFormat::BGRx.as_raw() {
                user_data.accepted = false;
                eprintln!("share: negotiated unsupported pixel format — refusing");
                return;
            }
            user_data.accepted = true;
            user_data.width = user_data.format.size().width;
            user_data.height = user_data.format.size().height;
            let fr = user_data.format.framerate();
            // Portals report 0/1 (unspecified) — treat as 30: the
            // encoder is rate-bound anyway and only timestamps use it.
            let fps = if fr.num > 0 && fr.denom > 0 {
                fr.num as f64 / fr.denom as f64
            } else {
                30.0
            };
            #[cfg(feature = "vaapi")]
            if user_data.inline.is_some() {
                // DRM names the packed word, so the byte order decides:
                // BGRA in memory is DRM_FORMAT_ARGB8888 ("AR24"), BGRx is
                // DRM_FORMAT_XRGB8888 ("XR24"). Getting this wrong feeds
                // the importer a format whose channel order is not the
                // buffer's.
                user_data.drm_format = if bgra {
                    u32::from_le_bytes(*b"AR24")
                } else {
                    u32::from_le_bytes(*b"XR24")
                };
                user_data.modifier = user_data.format.modifier();
                let (w, h) = (user_data.width, user_data.height);
                println!(
                    "share: negotiated {w}x{h} {} @ {fps:.0} fps (dma-buf, modifier {:#x})",
                    if bgra { "BGRA" } else { "BGRx" },
                    user_data.modifier
                );
                if let Some(share) = user_data.inline.as_mut() {
                    share.negotiate(w, h, fps);
                }
                return;
            }
            println!(
                "share: negotiated {}x{} {} @ {:.0} fps (source-paced)",
                user_data.width,
                user_data.height,
                if bgra { "BGRA" } else { "BGRx" },
                fps
            );
            let _ = user_data.tx.try_send(ToEncoder::Negotiated {
                width: user_data.width,
                height: user_data.height,
                fps,
            });
        })
        .process(|stream, user_data| {
            if !user_data.accepted {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let chunk = data.chunk();
            let size = chunk.size() as usize;
            if size == 0 {
                return;
            }
            let offset = chunk.offset() as usize;
            let stride = chunk.stride() as usize;
            let (w, h) = (user_data.width as usize, user_data.height as usize);

            // 7j: DMA-BUF mode. The producer's buffer is valid only until
            // this callback queues it back, so the GPU has to be done with
            // it before we return: encode here, on this thread, and let the
            // buffer go at the end of the call. `due()` drops a frame that
            // arrives ahead of the pacing interval — this path cannot
            // queue, so dropping is the pacing.
            #[cfg(feature = "vaapi")]
            let dmabuf_state = (
                user_data.drm_format,
                user_data.modifier,
                user_data.width,
                user_data.height,
            );
            #[cfg(feature = "vaapi")]
            if let Some(share) = user_data.inline.as_mut() {
                let (drm_format, modifier, width, height) = dmabuf_state;
                // Pace by waiting, holding the producer's buffer (see
                // `wait_for_slot`). A frame that is more than one interval
                // late means the encoder cannot keep up: drop it rather than
                // stack latency, which is what the memfd path's
                // skip-to-newest does on the other side of its channel.
                let wait = share.wait_for_slot();
                if !wait.is_zero() {
                    if wait < share.interval() {
                        std::thread::sleep(wait);
                    } else {
                        return;
                    }
                }
                if data.type_() == spa::buffer::DataType::DmaBuf {
                    // A dupped descriptor is what the encoder takes; the
                    // buffer itself stays dequeued (and therefore unwritten)
                    // until this callback returns.
                    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(data.fd()) };
                    let Ok(fd) = borrowed.try_clone_to_owned() else {
                        eprintln!(
                            "share: could not dup the producer's DMA-BUF fd — skipping frame"
                        );
                        return;
                    };
                    let frame = parley_core::video::DmaBufFrame {
                        fd,
                        drm_format,
                        modifier,
                        width,
                        height,
                        planes: vec![parley_core::video::DmaBufPlane {
                            offset: offset as u32,
                            pitch: stride as u32,
                        }],
                    };
                    if !share.encode_dmabuf(frame) {
                        let _ = share.die.send(());
                    }
                } else if let Some(mapped) = data.data() {
                    // The producer gave us a mapped buffer despite the
                    // negotiation. Encode it inline rather than dropping
                    // frames; the encoder takes either form.
                    let stride = if stride == 0 { w * 4 } else { stride };
                    let mut padded = Vec::with_capacity(stride * h);
                    for y in 0..h {
                        let start = offset + y * stride;
                        let Some(row) = mapped.get(start..start + w * 4) else {
                            return;
                        };
                        let dst = y * stride;
                        padded.resize(dst + row.len(), 0);
                        padded[dst..dst + row.len()].copy_from_slice(row);
                    }
                    if !share.encode_bgra(&padded, stride) {
                        let _ = share.die.send(());
                    }
                }
                return;
            }

            let Some(mapped) = data.data() else {
                return;
            };
            // Row-wise repack: handles both padded strides and the
            // tightly-packed case, and never panics on short buffers.
            let stride = if stride == 0 { w * 4 } else { stride };
            // 7g: copy the frame ONCE, as mapped (stride intact), then
            // hand the padded buffer to the encoder — the strided SIMD
            // path walks the rows directly (bench: ~2× the old path).
            let mut data = Vec::with_capacity(stride * h);
            for y in 0..h {
                let start = offset + y * stride;
                let Some(row) = mapped.get(start..start + w * 4) else {
                    return; // truncated buffer: skip the frame
                };
                let dst = y * stride;
                data.resize(dst + row.len(), 0);
                data[dst..dst + row.len()].copy_from_slice(row);
            }
            if user_data
                .tx
                .try_send(ToEncoder::Frame { data, stride })
                .is_err()
            {
                user_data.drops += 1;
                if user_data.drops.is_multiple_of(128) {
                    eprintln!(
                        "share: encoder behind — {} frames dropped so far",
                        user_data.drops
                    );
                }
            }
        })
        .register()
    {
        Ok(l) => l,
        Err(e) => {
            let _ = setup.send(Err(e));
            return;
        }
    };

    // 7j: the DMA-BUF offer — two parameter objects, because SPA puts the
    // memory type in the buffer configuration, not the format:
    //
    // * `EnumFormat` describing packed BGRx plus a MANDATORY modifier. The
    //   modifier property is what marks the negotiation as DMA-BUF, and it
    //   offers only DRM_FORMAT_MOD_LINEAR: the importer can address linear
    //   memory, and a screencast producer can always allocate it.
    // * `Buffers` with a data-type mask of exactly `1 << SPA_DATA_DmaBuf`,
    //   so the producer allocates the compositor's own buffers rather than
    //   a shared-memory copy.
    //
    // Both are built by hand where the `object!` macro cannot express them:
    // numeric choices (the modifier, the mask) have no macro arm in this
    // version of libspa, and the buffers keys have no typed constants.
    #[cfg(feature = "vaapi")]
    let dmabuf_objects = dmabuf.then(|| {
        use spa::param::ParamType;
        use spa::pod::Object;
        use spa::utils::SpaTypes;

        let mut properties = vec![
            spa::pod::property!(
                spa::param::format::FormatProperties::MediaType,
                Id,
                spa::param::format::MediaType::Video
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::MediaSubtype,
                Id,
                spa::param::format::MediaSubtype::Raw
            ),
            // BGRA is the default because that is what the DMA-BUF
            // producers offer first (xdg-desktop-portal-hyprland offers
            // BGRA alone under a modifier, BGRx only without one); the
            // alpha is ignored by the encoder.
            spa::pod::property!(
                spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                spa::param::video::VideoFormat::BGRA,
                spa::param::video::VideoFormat::BGRA,
                spa::param::video::VideoFormat::BGRx
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                spa::utils::Rectangle {
                    width: 1280,
                    height: 720
                },
                spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                spa::utils::Rectangle {
                    width: 4096,
                    height: 4096
                }
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                spa::utils::Fraction { num: 30, denom: 1 },
                spa::utils::Fraction { num: 0, denom: 1 },
                spa::utils::Fraction { num: 60, denom: 1 }
            ),
        ];
        properties.push(modifier_property());

        let format = Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties,
        };
        let buffers = Object {
            type_: SpaTypes::ObjectParamBuffers.as_raw(),
            id: ParamType::Buffers.as_raw(),
            properties: vec![buffers_data_type_property()],
        };
        vec![format, buffers]
    });

    // EnumFormat: BGRx preferred (alpha is meaningless for encoding),
    // BGRA accepted; size/framerate ranges let the source pick native.
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1280,
                height: 720
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 4096,
                height: 4096
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 60, denom: 1 }
        ),
    );
    #[cfg(feature = "vaapi")]
    let objects = dmabuf_objects.unwrap_or_else(|| vec![obj]);
    #[cfg(not(feature = "vaapi"))]
    let objects = vec![obj];

    // The serialized bytes must outlive the Pods that borrow them.
    let values: Vec<Vec<u8>> = objects
        .iter()
        .map(|object| {
            spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &spa::pod::Value::Object(object.clone()),
            )
            .expect("static pod serialization cannot fail")
            .0
            .into_inner()
        })
        .collect();
    let mut params: Vec<&spa::pod::Pod> = values
        .iter()
        .map(|bytes| spa::pod::Pod::from_bytes(bytes).expect("serialized pod is valid"))
        .collect();

    // MAP_BUFFERS asks PipeWire to map every buffer into our address
    // space. A DMA-BUF is not mapped here — the GPU reads it — and asking
    // for both at once is not a combination the stream accepts.
    let flags = if dmabuf {
        pw::stream::StreamFlags::AUTOCONNECT
    } else {
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS
    };
    if let Err(e) = stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        flags,
        &mut params,
    ) {
        let _ = setup.send(Err(e));
        return;
    }
    println!("share: pipewire stream connected");
    let _ = setup.send(Ok(()));

    // Poll loop instead of run(): MainLoopBox is !Sync so quit() from
    // another thread is impossible — a stop flag + 100 ms slices is
    // the designed alternative. See the module docs.
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }
    if let Err(e) = stream.disconnect() {
        eprintln!("share: pw disconnect error: {e}");
    }
    println!("share: pipewire stream disconnected");
}

/// The `VideoModifier` property of a DMA-BUF format offer: MANDATORY, and
/// offering the two tilings that are always importable —
/// [`DRM_FORMAT_MOD_INVALID`](0x00ffffffffffffff), which is what a portal
/// advertises for its own buffers (xdg-desktop-portal-hyprland offers
/// exactly this and nothing else), and `DRM_FORMAT_MOD_LINEAR` (0), which
/// is what a compositor allocating through GBM picks.
///
/// This must match the producer's advertisements: offering LINEAR alone
/// against a portal that only speaks `INVALID` fails the negotiation
/// outright — `pw stream ... Error("no more input formats")`, measured
/// against xdg-desktop-portal-hyprland.
///
/// Built by hand because the `object!`/`property!` macros have no arm for a
/// numeric choice in this version of libspa (`Choice, Enum, Long, ..` looks
/// for a type named `utils::Long`, which is a pod type tag, not a type).
#[cfg(feature = "vaapi")]
fn modifier_property() -> spa::pod::Property {
    use spa::pod::{ChoiceValue, PropertyFlags, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    const MOD_INVALID: i64 = 0x00ff_ffff_ffff_ffff;
    const MOD_LINEAR: i64 = 0;
    spa::pod::Property {
        key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
        flags: PropertyFlags::MANDATORY,
        value: Value::Choice(ChoiceValue::Long(Choice::<i64>(
            ChoiceFlags::empty(),
            ChoiceEnum::<i64>::Enum {
                default: MOD_INVALID,
                alternatives: vec![MOD_INVALID, MOD_LINEAR],
            },
        ))),
    }
}

/// The `SPA_PARAM_BUFFERS_dataType` property of a DMA-BUF request: a flags
/// choice whose only member is the DMA-BUF bit, so the producer allocates
/// its own buffers instead of a shared-memory copy.
///
/// `SPA_PARAM_BUFFERS_dataType` has no typed constant in libspa, so the raw
/// key comes from the generated bindings.
#[cfg(feature = "vaapi")]
fn buffers_data_type_property() -> spa::pod::Property {
    use spa::pod::{ChoiceValue, PropertyFlags, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let dma_buf_bit = 1i32 << spa::buffer::DataType::DmaBuf.as_raw();
    spa::pod::Property {
        key: spa::sys::SPA_PARAM_BUFFERS_dataType,
        flags: PropertyFlags::empty(),
        value: Value::Choice(ChoiceValue::Int(Choice::<i32>(
            ChoiceFlags::empty(),
            ChoiceEnum::<i32>::Flags {
                default: dma_buf_bit,
                flags: vec![dma_buf_bit],
            },
        ))),
    }
}

// 7h: `fragment_budget_for` (the connection's live datagram budget)
// lives in crate::video — both senders use it and only one of them is
// behind this module's feature gate.
use crate::video::fragment_budget_for;

/// The rate the share actually encodes and paces at: the compositor's,
/// clamped by the encoder's profile ceiling (30 fps software, 60 fps
/// hardware). Both the pacing interval and the frame timestamps read
/// this, so a 60 Hz compositor on a software encoder cannot stretch
/// every timestamp to 1/60 s.
fn paced_fps(capture_fps: f64) -> f64 {
    capture_fps.min(f64::from(target_fps()))
}

impl ShareEncoder {
    fn new(sender: ShareSenderArgs) -> Self {
        let max_payload = fragment_budget_for(&sender.conn);
        Self {
            conn: sender.conn,
            encoder: None,
            ladder: VideoLadder::new(),
            fps: 30.0,
            seq: 0x8000,
            frame_id: 0,
            annexb: Vec::with_capacity(1 << 20),
            max_payload,
            encoded: 0,
            stale_skipped: 0,
            window: std::time::Instant::now(),
            die: sender.die,
            force_idr: sender.force_idr,
            video_loss: sender.video_loss,
            last_encode: None,
        }
    }

    /// The pacing interval the stream is being held to.
    fn interval(&self) -> Duration {
        Duration::from_nanos((1_000_000_000.0 / paced_fps(self.fps)) as u64)
    }

    /// How long until the next frame is due (zero when it already is).
    ///
    /// The DMA-BUF path sleeps this out while still holding the producer's
    /// buffer — holding it is exactly the backpressure PipeWire expects, and
    /// dropping instead costs frames the source did send (measured: 24.7 of a
    /// 30 fps source when every early frame was dropped).
    #[cfg(feature = "vaapi")]
    fn wait_for_slot(&mut self) -> Duration {
        let interval = self.interval();
        match self.last_encode {
            Some(t) => interval.saturating_sub(t.elapsed()),
            None => Duration::ZERO,
        }
    }

    fn note_stale(&mut self) {
        self.stale_skipped += 1;
    }

    /// Apply a negotiated format: (re)create the encoder, record the fps,
    /// and move the loss ladder's rungs to the new resolution's rate.
    ///
    /// 7h HD: the ladder's rungs were absolute (2.5 Mbps clean / 1 Mbps
    /// robust), which is a 720p30 budget — on a 1080p60 stream that is
    /// the same as always running degraded. `bitrate_for` scales them
    /// with the pixels and rate the encoder is actually being asked for.
    fn negotiate(&mut self, width: u32, height: u32, f: f64) {
        self.fps = f;
        match create_encoder_auto(width as usize, height as usize) {
            Ok(e) => {
                let eff = paced_fps(f);
                let base = bitrate_for(width as usize, height as usize, eff as f32);
                let applied = self.ladder.set_base(base);
                println!(
                    "share: encoder ready ({width}x{height}, source {f:.0} fps → paced {eff:.0} fps, \
                     clean rung {base} bps); hardware={}",
                    parley_core::video::hardware_encoder_active()
                );
                if applied != base {
                    // The ladder was already on the robust rung: keep the
                    // encoder there rather than pretending the rebuild
                    // restored the clean target.
                    println!("share: bitrate → {applied} bps (robust rung, scaled to profile)");
                }
                self.encoder = Some(e);
            }
            Err(e) => eprintln!("share: encoder init failed: {e}"),
        }
    }

    /// Ladder step + pending keyframe requests, applied to the next frame.
    fn prepare(&mut self) -> Option<&mut Box<dyn VideoEncoder>> {
        let enc = self.encoder.as_mut()?;
        if let Some(bps) = self.ladder.step(self.video_loss.load(Ordering::Relaxed)) {
            enc.set_bitrate(bps);
            println!("share: bitrate → {bps} bps (video loss ladder)");
        }
        if self.ladder.should_force_keyframe() {
            enc.force_keyframe();
            println!("share: IDR re-poke (loss epoch)");
        }
        if self.force_idr.swap(false, Ordering::Relaxed) {
            enc.force_keyframe();
            println!("share: forced keyframe (viewer joined)");
        }
        Some(enc)
    }

    /// Encode one BGRA frame (strided) and send it. Returns false when the
    /// connection is gone; a dropped frame returns true.
    fn encode_bgra(&mut self, data: &[u8], stride: usize) -> bool {
        let Some((w, h)) = self.encoder.as_ref().map(|e| e.dimensions()) else {
            return true;
        };
        self.last_encode = Some(std::time::Instant::now());
        self.frame_encode(|enc, out| enc.encode_bgra_strided(data, w, h, stride, out))
    }

    /// 7j: encode one frame in the producer's DMA-BUF. The descriptor is
    /// consumed by the encoder, and the capture thread holds the producer's
    /// buffer until this returns.
    #[cfg(feature = "vaapi")]
    fn encode_dmabuf(&mut self, frame: parley_core::video::DmaBufFrame) -> bool {
        self.last_encode = Some(std::time::Instant::now());
        self.frame_encode(move |enc, out| enc.encode_dmabuf(frame, out))
    }

    /// The shared hot path: prepare, encode into the reused buffer, send.
    fn frame_encode(
        &mut self,
        encode: impl FnOnce(
            &mut Box<dyn VideoEncoder>,
            &mut Vec<u8>,
        ) -> Result<bool, parley_core::error::Error>,
    ) -> bool {
        let mut annexb = std::mem::take(&mut self.annexb);
        let encoded = match self.prepare() {
            Some(enc) => encode(enc, &mut annexb),
            None => {
                self.annexb = annexb;
                return true;
            }
        };
        let keyframe = match encoded {
            Ok(k) => k,
            Err(e) => {
                eprintln!("share: encode failed: {e}");
                self.annexb = annexb;
                return true;
            }
        };
        self.annexb = annexb;
        self.send(keyframe)
    }

    /// Fragment + pace + send the last encoded frame.
    ///
    /// Output discipline (learned the hard way): fragments leave over HALF
    /// the frame's interval, split evenly (7h; a fixed 500 µs per fragment
    /// made a 100 KB keyframe unfinishable inside its own frame budget).
    fn send(&mut self, keyframe: bool) -> bool {
        let annexb = std::mem::take(&mut self.annexb);
        let alive = self.send_inner(&annexb, keyframe);
        self.annexb = annexb;
        alive
    }

    fn send_inner(&mut self, frame: &[u8], keyframe: bool) -> bool {
        let first_seq = self.seq;
        // The seq advance must match the fragmenter's own count — it is
        // computed from the same budget, or the two drift apart the moment
        // the MTU estimate changes.
        let frags = frame.len().div_ceil(self.max_payload).max(1) as u16;
        self.seq = self.seq.wrapping_add(frags);
        // The codec clock the receiver sees must advance at the rate we
        // actually pace at (see `paced_fps`), not the compositor's and not
        // a constant.
        let eff_fps = paced_fps(self.fps);
        let timestamp = (self.frame_id as f64 * eff_fps.recip() * 48_000.0) as u32;
        let dgs = fragment_frame_with_max(
            self.frame_id,
            keyframe,
            frame,
            first_seq,
            timestamp,
            0, // sender_id: the server stamps the authenticated id
            self.max_payload,
            // 7h HD: tell viewers this stream runs above ~45 fps (mpv's
            // rawvideo demuxer needs a nominal rate; a 60 fps stream
            // announced as 30 plays at half speed).
            eff_fps > 45.0,
        );
        self.frame_id = self.frame_id.wrapping_add(1);
        let min_interval = self.interval();
        let total = dgs.len();
        let per_frag = if total > 1 {
            let half_interval_us = (min_interval.as_micros() as usize / 2).max(200);
            (half_interval_us / (total - 1)).min(500) as u64
        } else {
            0
        };
        for (i, dg) in dgs.into_iter().enumerate() {
            if let Err(e) = self.conn.send_datagram(dg.into()) {
                // Real reason, not a guess: TooLarge (the estimate moved
                // under us) and ConnectionLost are very different
                // diagnoses — and only the second one means the share is
                // over. Dropping the frame and re-reading the budget keeps
                // the session alive; the viewer rebases on the next
                // keyframe. This used to end the share outright, so a
                // transient MTU collapse looked like the share crashing.
                println!(
                    "share: send_datagram failed: {e}; dropping this frame and re-reading the \
                     datagram budget"
                );
                self.max_payload = fragment_budget_for(&self.conn);
                return self.conn.close_reason().is_none();
            }
            if i + 1 < total && per_frag > 0 {
                std::thread::sleep(Duration::from_micros(per_frag));
            }
        }
        self.encoded += 1;
        let el = self.window.elapsed();
        if el.as_secs() >= 10 {
            println!(
                "share: encoded {} frames ({:.1} fps), {} stale skipped",
                self.encoded,
                self.encoded as f64 / el.as_secs_f64(),
                self.stale_skipped
            );
            self.encoded = 0;
            self.stale_skipped = 0;
            self.window = std::time::Instant::now();
        }
        true
    }
}

/// Encode → fragment → paced send, for the memfd capture path. Owns the
/// encoder once the format is negotiated; drops frames that arrive before
/// that. Exits when the frame channel closes (the pw thread ended).
///
/// Output discipline, in order per frame:
/// 1. Pace: sleep until min_interval since the last encode START — mpv
///    consumes piped rawvideo at exactly VIDEO_FPS; sending faster
///    accumulates drift in its queue (~3 s over a session, measured).
///    Sleep, never drop: the frame is wanted, just early.
/// 2. Skip-to-newest: drain anything that queued during the sleep and
///    keep only the latest capture — encoding a stale frame while a
///    fresher one sits behind it is pure added latency.
fn encoder_thread(
    rx: Receiver<ToEncoder>,
    conn: quinn::Connection,
    force_idr: Arc<AtomicBool>,
    video_loss: Arc<AtomicU32>,
    die: tokio::sync::mpsc::UnboundedSender<()>,
) {
    let mut share = ShareEncoder::new(ShareSenderArgs {
        conn,
        die,
        force_idr,
        video_loss,
    });
    while let Ok(msg) = rx.recv() {
        match msg {
            ToEncoder::Negotiated { width, height, fps } => share.negotiate(width, height, fps),
            ToEncoder::Frame {
                data: first,
                stride: first_stride,
            } => {
                // 1. Rate pacing (sleep, never drop).
                let min_interval = share.interval();
                let now = std::time::Instant::now();
                if let Some(t) = share.last_encode
                    && now.duration_since(t) < min_interval
                {
                    std::thread::sleep(min_interval - now.duration_since(t));
                }

                // 2. Skip-to-newest. A Negotiated here is impossible
                //    (pw delivers the format before any buffer), but
                //    stash and apply it rather than lose it.
                let mut bgra = first;
                let mut stride = first_stride;
                let mut renegotiate: Option<(u32, u32, f64)> = None;
                loop {
                    match rx.try_recv() {
                        Ok(ToEncoder::Frame { data, stride: s }) => {
                            bgra = data;
                            stride = s;
                            share.note_stale();
                        }
                        Ok(ToEncoder::Negotiated {
                            width,
                            height,
                            fps: f,
                        }) => {
                            renegotiate = Some((width, height, f));
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if let Some((width, height, f)) = renegotiate {
                    share.negotiate(width, height, f);
                }
                if !share.encode_bgra(&bgra, stride) {
                    // The connection itself is gone: nothing left to do.
                    let _ = share.die.send(());
                    return;
                }
            }
        }
    }
    println!("share: capture ended");
}

// ---- 6.9: share audio (default-output capture + mux) ----

/// Listener user data for the audio capture stream.
struct AudioState {
    stop: Arc<AtomicBool>,
    accepted: bool,
    channels: u32,
    rate: u32,
    tx: SyncSender<Vec<f32>>,
    drops: u64,
}

/// Start capture of the DEFAULT OUTPUT (PipeWire `stream.capture.sink`)
/// on its own thread/loop. Bounded channel + try_send: never block the
/// pw thread (the 6f discipline); a slow consumer drops stale audio —
/// correct for live content. Failures surface on stderr; the video
/// share proceeds regardless.
fn start_audio_capture(stop: Arc<AtomicBool>, sys_tx: SyncSender<Vec<f32>>) {
    std::thread::spawn(move || {
        if let Err(e) = audio_capture_thread(stop, sys_tx) {
            eprintln!("share-audio: {e}");
        }
    });
}

fn audio_capture_thread(
    stop: Arc<AtomicBool>,
    sys_tx: SyncSender<Vec<f32>>,
) -> Result<(), pw::Error> {
    // Native connection (NOT the portal's connect_fd): this stream is
    // our own client on the user's PipeWire instance.
    let mainloop = pw::main_loop::MainLoopBox::new(None)?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)?;
    let core = context.connect(None)?;

    let state = AudioState {
        stop: stop.clone(),
        accepted: false,
        channels: 2,
        rate: 48_000,
        tx: sys_tx,
        drops: 0,
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "parley-share-audio",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            // THE mechanism: capture the default sink's monitor. No
            // registry, no node targeting — PipeWire routes it. Scope:
            // this output's audio (documented product behavior).
            *pw::keys::STREAM_CAPTURE_SINK => "true",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, old, new| {
            println!("share-audio: pw stream {:?} -> {:?}", old, new);
        })
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) = match spa::param::format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };
            if media_type != spa::param::format::MediaType::Audio
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            let mut info = spa::param::audio::AudioInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            let fmt = info.format().as_raw();
            if fmt != spa::param::audio::AudioFormat::F32LE.as_raw() {
                user_data.accepted = false;
                user_data.stop.store(true, Ordering::Relaxed);
                eprintln!(
                    "share-audio: negotiated a non-F32 format — refusing (v1 supports F32 only)"
                );
                return;
            }
            user_data.channels = info.channels();
            user_data.rate = info.rate();
            if user_data.rate != 48_000 {
                user_data.accepted = false;
                user_data.stop.store(true, Ordering::Relaxed);
                eprintln!(
                    "share-audio: graph rate is {} Hz, pipeline is 48k-only — refusing",
                    user_data.rate
                );
                return;
            }
            if user_data.channels != 2 {
                user_data.accepted = false;
                user_data.stop.store(true, Ordering::Relaxed);
                eprintln!(
                    "share-audio: negotiated {}ch, mux expects stereo — refusing",
                    user_data.channels
                );
                return;
            }
            user_data.accepted = true;
            println!(
                "share-audio: negotiated F32 {}ch @ {} Hz (default-output monitor)",
                user_data.channels, user_data.rate
            );
        })
        .process(|stream, user_data| {
            if !user_data.accepted {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let chunk = data.chunk();
            let size = chunk.size() as usize;
            if size == 0 {
                return;
            }
            let offset = chunk.offset() as usize;
            let Some(mapped) = data.data() else { return };
            let samples = size / 4;
            let Some(bytes) = mapped.get(offset..offset + samples * 4) else {
                return; // truncated buffer: skip
            };
            let interleaved: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            if user_data.tx.try_send(interleaved).is_err() {
                user_data.drops += 1;
                if user_data.drops.is_multiple_of(128) {
                    eprintln!(
                        "share-audio: mux behind — {} buffers dropped",
                        user_data.drops
                    );
                }
            }
        })
        .register()?;

    // EnumFormat: exactly F32LE / 48 kHz / 2ch. The pod property!
    // macro CANNOT express Int-valued choices (its $type_ must be a
    // utils:: struct; ints are plain i32 — no path). The Id properties
    // and the two Int ranges are built with Property::new — the
    // crate's own AudioInfoRaw idiom (raw.rs From<...> impl).
    use spa::pod::{ChoiceValue, Object, Property, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id};
    let obj = Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                spa::param::format::FormatProperties::MediaType.as_raw(),
                Value::Id(Id(spa::param::format::MediaType::Audio.as_raw())),
            ),
            Property::new(
                spa::param::format::FormatProperties::MediaSubtype.as_raw(),
                Value::Id(Id(spa::param::format::MediaSubtype::Raw.as_raw())),
            ),
            Property::new(
                spa::param::format::FormatProperties::AudioFormat.as_raw(),
                Value::Id(Id(spa::param::audio::AudioFormat::F32LE.as_raw())),
            ),
            Property::new(
                spa::param::format::FormatProperties::AudioRate.as_raw(),
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 48_000,
                        min: 48_000,
                        max: 48_000,
                    },
                ))),
            ),
            Property::new(
                spa::param::format::FormatProperties::AudioChannels.as_raw(),
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 2,
                        min: 2,
                        max: 2,
                    },
                ))),
            ),
        ],
    };
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .expect("static pod serialization cannot fail")
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&values).expect("serialized pod is valid")];

    stream.connect(
        spa::utils::Direction::Input,
        None, // STREAM_CAPTURE_SINK decides the target
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    println!("share-audio: capturing the default output (native PipeWire, stream.capture.sink)");

    // Same iterate-loop discipline as the video pw thread.
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }
    let _ = stream.disconnect();
    println!("share-audio: capture stopped");
    Ok(())
}

/// The share-audio mux: [default-output stereo + mic mono→center] →
/// ONE music-profile stream (stereo 256 kbps, FLAG_STEREO). While it
/// runs it owns the mic route (the tee in main.rs watches `mic_route`).
///
/// MIC IS THE CLOCK (design settled by second-opinion review after two
/// falsified alternatives): PipeWire SUSPENDS capture when the desktop
/// is silent, so blocking on sys_rx stalls the mux AND the mic (the
/// "instant burst": mic chunks pile up while blocked, then flush on
/// the first system sound). The mic (cpal) never suspends — blocking
/// on it keeps real-time pacing unconditionally. System audio is
/// buffered and consumed at mic pace; a 100 ms cap bounds staleness
/// (sustained surplus = rate mismatch; the cap drops OLDEST and
/// counts).
///
/// Without a mic (mixed-feature edge), degrades to sys-only at the
/// sys delivery pace — the same shape as a music channel.
///
/// Runs on a dedicated std thread. Exits when both inputs close or a
/// send fails; on EVERY exit it flips `mic_route` back so the mic
/// returns to the speech path. Returns the mic receiver for reuse.
pub fn spawn_share_audio_mux(
    mic_rx: Option<tokio::sync::mpsc::Receiver<Vec<f32>>>,
    sys_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    conn: quinn::Connection,
    mic_route: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Option<tokio::sync::mpsc::Receiver<Vec<f32>>>> {
    std::thread::spawn(move || {
        let mut mic_rx = mic_rx;
        // 7k: only take the mic route when the mic is actually joining
        // this stream. With None the voice stays on its own (speech)
        // encoder and the share carries system audio alone — which is
        // what lets a receiver play the voice without the share audio.
        mic_route.store(mic_rx.is_some(), Ordering::Relaxed);
        let mut send = match SendPipeline::new_music(256_000) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("share-audio: music encoder init failed: {e}");
                mic_route.store(false, Ordering::Relaxed);
                return mic_rx;
            }
        };
        // 7k: FLAG_SHARE_AUDIO marks this stream as a share's system
        // audio, so receivers gate it on "am I watching this share?"
        // without touching the sender's voice stream.
        let flags = parley_proto::media::FLAG_SPEAKING
            | parley_proto::media::FLAG_STEREO
            | parley_proto::media::FLAG_SHARE_AUDIO;
        let mut sys_buffer: Vec<f32> = Vec::new();
        let mut sys_alive = true;
        let mut capped: u64 = 0; // frames dropped by the drift cap
        // 100 ms of stereo = 4800 frames = 9600 interleaved samples.
        const SYS_CAP_SAMPLES: usize = 9_600;

        loop {
            // --- clock source ---
            let mut mic_flat: Vec<f32> = Vec::new();
            if let Some(rx) = mic_rx.as_mut() {
                match rx.blocking_recv() {
                    Some(chunk) => {
                        mic_flat.extend(chunk);
                        while let Ok(more) = rx.try_recv() {
                            mic_flat.extend(more);
                        }
                    }
                    None => {
                        // Mic channel closed (client teardown): finish
                        // with sys-only at delivery pace, below. Hand
                        // the mic route back so the voice encoder sees
                        // the microphone again.
                        mic_rx = None;
                        mic_route.store(false, Ordering::Relaxed);
                    }
                }
            }

            // --- sys: drain everything available, non-blocking ---
            if sys_alive {
                loop {
                    match sys_rx.try_recv() {
                        Ok(chunk) => sys_buffer.extend(chunk),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            sys_alive = false;
                            break;
                        }
                    }
                }
            }

            // The share is over when sys is gone and its tail drained.
            // 6.9 fix: the old condition also required the mic channel
            // closed — but the mic outlives the share (it's the client's
            // capture), so after `stop` the mux ran forever on mic-only
            // audio: mic_route stayed hijacked and the Ended handler's
            // join wedged the command task (StopShare never sent, mpv
            // never closed). sys_rx's sender is the audio capture thread,
            // whose lifetime IS the share's (shared stop flag); PipeWire
            // suspend is Empty, not Disconnected, so silence can't end
            // it. The tail drains at mic pace first — clean audio outro.
            if !sys_alive && sys_buffer.is_empty() {
                println!("share-audio: ended");
                break;
            }

            // --- frame the step ---
            // Mic-clocked: one step = mic_frames (mic never suspends ⇒
            // real-time pace). Sys-only: one quantum per delivery.
            let sys_frames_available = sys_buffer.len() / 2;
            let mix_frames = if !mic_flat.is_empty() {
                mic_flat.len()
            } else if sys_frames_available > 0 {
                sys_frames_available.min(4800) // cap a step at 100 ms
            } else {
                // No input this round (mic gone, sys suspended+empty):
                // park briefly so we don't spin.
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };

            let mut pcm: Vec<i16> = Vec::with_capacity(mix_frames * 2);
            for i in 0..mix_frames {
                let m = if i < mic_flat.len() { mic_flat[i] } else { 0.0 };
                let (l, r) = if i < sys_frames_available {
                    (sys_buffer[i * 2], sys_buffer[i * 2 + 1])
                } else {
                    (0.0, 0.0) // sys suspended/underrun — mic rides alone
                };
                pcm.push(((l + m).clamp(-1.0, 1.0) * 32767.0) as i16);
                pcm.push(((r + m).clamp(-1.0, 1.0) * 32767.0) as i16);
            }

            // Consume what the step used; cap the remainder.
            let consumed = mix_frames.min(sys_frames_available);
            if consumed > 0 {
                sys_buffer.drain(0..(consumed * 2));
            }
            if sys_buffer.len() > SYS_CAP_SAMPLES {
                let excess = sys_buffer.len() - SYS_CAP_SAMPLES;
                sys_buffer.drain(0..excess);
                capped += (excess / 2) as u64;
                if capped.is_multiple_of(4800) {
                    eprintln!(
                        "share-audio: drift cap dropped {} frames total (pw/cpal rate mismatch?)",
                        capped
                    );
                }
            }

            let mut packets = Vec::new();
            if send.push_pcm(&pcm, &mut packets).is_err() {
                eprintln!("share-audio: encode failed");
                break;
            }
            for (seq, ts, pkt) in packets {
                let dg = parley_proto::media::encode(flags, seq, ts, 0, &pkt);
                if conn.send_datagram(dg.into()).is_err() {
                    println!("share-audio: connection closed");
                    mic_route.store(false, Ordering::Relaxed);
                    return mic_rx;
                }
            }
        }
        mic_route.store(false, Ordering::Relaxed);
        mic_rx
    })
}

/// Entry point: portal dance → threads → supervisor.
///
/// `sys_tx` = Some ⇒ share WITH audio (the default-output capture
/// starts alongside video, same stop flag). Returns the keyframe
/// handle + stop controller. Completion of a stop (user-initiated or
/// desktop-initiated) is reported as ShareEvent::Ended on `events` —
/// only after full teardown.
pub async fn start(
    conn: quinn::Connection,
    video_loss: Arc<AtomicU32>,
    events: tokio::sync::mpsc::UnboundedSender<ShareEvent>,
    sys_tx: Option<SyncSender<Vec<f32>>>,
    dmabuf: bool,
) -> Result<(ShareHandle, ShareController), String> {
    // 7j: a request for zero-copy is refused up front when this build or
    // this run cannot honor it. Silently downgrading would make the caller
    // measure a CPU copy it believes is not there.
    if dmabuf {
        #[cfg(not(feature = "vaapi"))]
        return Err("dma-buf capture needs a build with the vaapi feature".to_string());
        #[cfg(feature = "vaapi")]
        if !parley_core::video::hw_encode_enabled() {
            return Err("dma-buf capture needs hardware encode (--hw-encode)".to_string());
        }
    }
    let proxy = Screencast::new()
        .await
        .map_err(|e| format!("portal: {e}"))?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| format!("create_session: {e}"))?;

    // The rest of the dance. Everything after create_session owns a
    // live portal session, so on failure we must close it before
    // returning (an async block gives `?` without having to name the
    // session's generic type anywhere).
    let negotiated: Result<(u32, OwnedFd), String> = async {
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    // cursor composited into the frames — what
                    // screen-share users expect
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(SourceType::Monitor | SourceType::Window)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(|e| format!("select_sources: {e}"))?;
        let response = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(|e| format!("start: {e}"))?
            .response()
            .map_err(|e| format!("start response: {e}"))?;
        let stream = response
            .streams()
            .first()
            .ok_or("no stream selected")?
            .to_owned();
        let node_id = stream.pipe_wire_node_id();
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(|e| format!("open_pipe_wire_remote: {e}"))?;
        Ok((node_id, fd))
    }
    .await;
    let (node_id, fd) = match negotiated {
        Ok(v) => v,
        Err(e) => {
            let _ = session.close().await;
            return Err(e);
        }
    };

    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<ToEncoder>(4);
    let force_idr = Arc::new(AtomicBool::new(false));
    let handle = ShareHandle {
        flag: force_idr.clone(),
    };
    let stop_flag = Arc::new(AtomicBool::new(false));

    // Encoder-death signal: whoever owns an encoder fires it on send
    // failure AND on any exit (pw death → rx closes → return → drop).
    // Either way the supervisor must tear everything down — a dead
    // connection must not leave a portal session and pw loop running
    // (found in the 6e netem run: endless "encoder behind").
    let (die_tx, mut die_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let sender = ShareSenderArgs {
        conn: conn.clone(),
        die: die_tx.clone(),
        force_idr: force_idr.clone(),
        video_loss: video_loss.clone(),
    };

    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel();
    let stop_for_thread = stop_flag.clone();
    let pw_handle = std::thread::spawn(move || {
        pw_thread(
            node_id,
            fd,
            frame_tx,
            stop_for_thread,
            setup_tx,
            dmabuf,
            sender,
        );
    });
    // 7j: in DMA-BUF mode there is no encoder thread — the capture thread
    // owns the encoder, because the producer's buffer must stay dequeued
    // until the GPU has read it. The channel is unused (and dropped) there.
    let enc_handle = if dmabuf {
        drop(frame_rx);
        None
    } else {
        Some(std::thread::spawn(move || {
            encoder_thread(frame_rx, conn, force_idr, video_loss, die_tx)
        }))
    };

    // Surface pw setup failures (and close the session on them).
    let setup = tokio::time::timeout(Duration::from_secs(5), setup_rx).await;
    match setup {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => {
            let _ = session.close().await;
            return Err(format!("pipewire: {e}"));
        }
        Ok(Err(_)) => {
            // oneshot dropped unsent: the thread died before signaling
            let _ = session.close().await;
            return Err("pipewire setup thread died".to_string());
        }
        Err(_) => {
            let _ = session.close().await;
            return Err("pipewire setup timeout".to_string());
        }
    }

    // 6.9: audio capture starts only after video setup succeeded — an
    // early return must never strand an audio thread. Same stop flag:
    // teardown kills both; the capture's channel-sender dropping is
    // what tells the mux the share ended.
    if let Some(sys_tx) = sys_tx {
        start_audio_capture(stop_flag.clone(), sys_tx);
    }

    // Supervisor: the single owner of the portal session. One select,
    // no loop — the first of (user stop, desktop Closed) wins, teardown
    // runs once, Ended is sent exactly once.
    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        // Box::pin: whether ashpd's signal stream is Unpin is
        // unverified; Pin<Box<S>> is Unpin unconditionally, so .next()
        // compiles either way.
        let mut closed = session.receive_closed().await.ok().map(Box::pin);
        if closed.is_none() {
            eprintln!("share: portal Closed watch failed — stop must come from \"stop\"");
        }
        let external = match closed.as_mut() {
            Some(signals) => tokio::select! {
                biased;
                _ = stop_rx.recv() => false, // user "stop" (or all controllers dropped)
                _ = signals.next() => true,  // desktop closed the session
                _ = die_rx.recv() => false,  // encoder thread died (send failure / pw death)
            },
            None => {
                tokio::select! {
                    _ = stop_rx.recv() => false,
                    _ = die_rx.recv() => false,
                }
            }
        };

        // Teardown, in order: pw loop stop → join threads → close the
        // portal session (kills the desktop's sharing indicator) →
        // report. Joins run on the blocking pool with timeouts so a
        // stuck thread can't stall a worker.
        stop_flag.store(true, Ordering::Relaxed);
        match tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || pw_handle.join()),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => eprintln!("share: pw thread join failed"),
            Err(_) => eprintln!("share: pw thread did not stop within 2 s — leaked"),
        }
        if let Some(enc_handle) = enc_handle {
            match tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || enc_handle.join()),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => eprintln!("share: encoder thread join failed"),
                Err(_) => eprintln!("share: encoder thread did not stop within 5 s — leaked"),
            }
        }
        // Already-closed (external stop) is fine — errors ignored.
        let _ = session.close().await;
        let _ = events.send(ShareEvent::Ended { external });
    });

    Ok((handle, ShareController { stop_tx }))
}

#[cfg(all(test, feature = "vaapi"))]
mod dmabuf_negotiation_tests {
    use super::{buffers_data_type_property, modifier_property};
    use pipewire::spa::pod::{ChoiceValue, PropertyFlags, Value};
    use pipewire::spa::utils::ChoiceEnum;

    /// The two properties that decide whether the share gets the
    /// compositor's buffers at all. A wrong bit or a missing modifier does
    /// not fail loudly — the producer quietly hands over a mapped copy, or
    /// refuses the format — so both are pinned here.
    ///
    /// 7j reasoning: the memory type lives in SPA_PARAM_Buffers (the format
    /// object has no dataType key in this protocol version), and the
    /// modifier property with MANDATORY is what marks a format offer as
    /// DMA-BUF.
    #[test]
    fn the_dmabuf_offer_asks_for_the_right_memory() {
        let modifier = modifier_property();
        assert_eq!(
            modifier.key,
            pipewire::spa::param::format::FormatProperties::VideoModifier.as_raw(),
            "the modifier property must be SPA_FORMAT_VIDEO_modifier"
        );
        assert_eq!(
            modifier.flags,
            PropertyFlags::MANDATORY,
            "without MANDATORY a producer may ignore the modifier and send memfd"
        );
        match modifier.value {
            Value::Choice(ChoiceValue::Long(choice)) => match choice.1 {
                ChoiceEnum::Enum {
                    default,
                    alternatives,
                } => {
                    // INVALID is what xdg-desktop-portal-hyprland offers for
                    // its own buffers; LINEAR is what a GBM allocator picks.
                    // An offer that cannot match the producer's own list
                    // fails negotiation with "no more input formats".
                    assert_eq!(
                        default, 0x00ff_ffff_ffff_ffff,
                        "the default modifier must be DRM_FORMAT_MOD_INVALID"
                    );
                    assert_eq!(
                        alternatives,
                        vec![0x00ff_ffff_ffff_ffff, 0],
                        "INVALID and LINEAR are the two importable tilings"
                    );
                }
                other => panic!("the modifier must be an enum choice, got {other:?}"),
            },
            other => panic!("the modifier must be a Long choice, got {other:?}"),
        }

        let buffers = buffers_data_type_property();
        assert_eq!(
            buffers.key, 6,
            "the data-type key is SPA_PARAM_BUFFERS_dataType"
        );
        match buffers.value {
            Value::Choice(ChoiceValue::Int(choice)) => match choice.1 {
                ChoiceEnum::Flags { default, flags } => {
                    // SPA_DATA_Invalid, MemPtr, MemFd, DmaBuf = 0, 1, 2, 3.
                    assert_eq!(flags, vec![1 << 3], "only the DMA-BUF bit is offered");
                    assert_eq!(default, 1 << 3);
                }
                other => panic!("the data type must be a flags choice, got {other:?}"),
            },
            other => panic!("the data type must be an Int choice, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::paced_fps;

    #[test]
    fn paced_rate_is_the_reachable_one() {
        // 7h HD: the rate the share paces and timestamps at is the
        // capture rate CLAMPED by the live profile. A 60 Hz compositor on
        // a software encoder must encode at 30 — otherwise every frame
        // timestamp would advance at 1/60 s while frames leave at 1/30,
        // and the receiver's clock would run at half speed.
        let profile = f64::from(parley_core::video::target_fps());
        assert_eq!(paced_fps(30.0), 30.0f64.min(profile));
        assert_eq!(paced_fps(60.0), 60.0f64.min(profile));
        assert_eq!(paced_fps(120.0), profile, "never above the profile");
        assert_eq!(paced_fps(1.0), 1.0, "a slow source stays a slow source");
    }
}
