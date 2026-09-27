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
use parley_core::video::{VIDEO_FPS, VideoEncoder, create_encoder};
use parley_proto::video::{MAX_FRAGMENT_PAYLOAD, fragment_frame};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;

use crate::ShareEvent;

/// Messages from the pipewire thread to the encoder thread. One
/// channel keeps them ordered — format always arrives before frames.
enum ToEncoder {
    /// Negotiated stream format.
    Negotiated { width: u32, height: u32, fps: f64 },
    /// One BGRx/BGRA frame, tightly packed (stride padding removed).
    Frame(Vec<u8>),
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
struct UserData {
    format: spa::param::video::VideoInfoRaw,
    accepted: bool,
    width: u32,
    height: u32,
    tx: SyncSender<ToEncoder>,
    drops: u64,
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

    let data = UserData {
        format: Default::default(),
        accepted: false,
        width: 0,
        height: 0,
        tx,
        drops: 0,
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
            let Some(mapped) = data.data() else {
                return;
            };
            let (w, h) = (user_data.width as usize, user_data.height as usize);
            // Row-wise repack: handles both padded strides and the
            // tightly-packed case, and never panics on short buffers.
            let stride = if stride == 0 { w * 4 } else { stride };
            let mut packed = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                let start = offset + y * stride;
                let Some(row) = mapped.get(start..start + w * 4) else {
                    return; // truncated buffer: skip the frame
                };
                packed.extend_from_slice(row);
            }
            if user_data.tx.try_send(ToEncoder::Frame(packed)).is_err() {
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
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("static pod serialization cannot fail")
    .0
    .into_inner();

    let mut params = [spa::pod::Pod::from_bytes(&values).expect("serialized pod is valid")];

    if let Err(e) = stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
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

/// Apply a negotiated format: (re)create the encoder, record the fps.
fn negotiate(
    encoder: &mut Option<Box<dyn VideoEncoder>>,
    fps: &mut f64,
    width: u32,
    height: u32,
    f: f64,
) {
    *fps = f;
    match create_encoder(width as usize, height as usize) {
        Ok(e) => {
            println!("share: encoder ready ({width}x{height})");
            *encoder = Some(e);
        }
        Err(e) => eprintln!("share: encoder init failed: {e}"),
    }
}

/// Encode → fragment → paced send. Owns the encoder once the format is
/// negotiated; drops frames that arrive before that. Exits when the
/// frame channel closes (the pw thread ended).
///
/// Output discipline, in order per frame:
/// 1. Pace: sleep until min_interval since the last encode START —
///    mpv consumes piped rawvideo at exactly VIDEO_FPS; sending faster
///    accumulates drift in its queue (~3 s over a session, measured).
///    Sleep, never drop: the frame is wanted, just early.
/// 2. Skip-to-newest: drain anything that queued during the sleep and
///    keep only the latest capture — encoding a stale frame while a
///    fresher one sits behind it is pure added latency.
/// 3. Encode + fragment + send with inter-fragment pacing.
fn encoder_thread(
    rx: Receiver<ToEncoder>,
    conn: quinn::Connection,
    force_idr: Arc<AtomicBool>,
    video_loss: Arc<AtomicU32>,
    die: tokio::sync::mpsc::UnboundedSender<()>,
) {
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    // 6e: loss-driven bitrate ladder (viewer reports → video_loss).
    let mut ladder = VideoLadder::new();
    let mut frame_id: u32 = 0;
    // Video seq space: local to this sender (receivers demux on
    // FLAG_VIDEO, never seq — audio's counter never interacts).
    let mut seq: u16 = 0x8000;
    let mut fps = 30.0;
    let mut encoded: u64 = 0;
    let mut stale_skipped: u64 = 0;
    let mut window = std::time::Instant::now();
    let mut last_encode: Option<std::time::Instant> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            ToEncoder::Negotiated {
                width,
                height,
                fps: f,
            } => {
                negotiate(&mut encoder, &mut fps, width, height, f);
            }
            ToEncoder::Frame(first) => {
                // 1. Rate pacing (sleep, never drop).
                let min_interval =
                    Duration::from_nanos((1_000_000_000.0 / fps.min(VIDEO_FPS as f64)) as u64);
                let now = std::time::Instant::now();
                if let Some(t) = last_encode
                    && now.duration_since(t) < min_interval
                {
                    std::thread::sleep(min_interval - now.duration_since(t));
                }

                // 2. Skip-to-newest. A Negotiated here is impossible
                //    (pw delivers the format before any buffer), but
                //    stash and apply it rather than lose it.
                let mut bgra = first;
                let mut renegotiate: Option<(u32, u32, f64)> = None;
                loop {
                    match rx.try_recv() {
                        Ok(ToEncoder::Frame(newer)) => {
                            bgra = newer;
                            stale_skipped += 1;
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
                    negotiate(&mut encoder, &mut fps, width, height, f);
                }

                let Some(enc) = encoder.as_mut() else {
                    continue;
                };
                if let Some(bps) = ladder.step(video_loss.load(Ordering::Relaxed)) {
                    enc.set_bitrate(bps);
                    println!("share: bitrate → {bps} bps (video loss ladder)");
                }
                if ladder.should_force_keyframe() {
                    enc.force_keyframe();
                    println!("share: IDR re-poke (loss epoch)");
                }
                if force_idr.swap(false, Ordering::Relaxed) {
                    enc.force_keyframe();
                    println!("share: forced keyframe (viewer joined)");
                }
                last_encode = Some(std::time::Instant::now());
                let (frame, keyframe) = match enc.encode_bgra(&bgra) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("share: encode failed: {e}");
                        continue;
                    }
                };
                let first_seq = seq;
                seq = seq.wrapping_add(frame.len().div_ceil(MAX_FRAGMENT_PAYLOAD).max(1) as u16);
                let timestamp = (frame_id as f64 * fps.recip() * 48_000.0) as u32;
                let dgs = fragment_frame(
                    frame_id, keyframe, &frame, first_seq, timestamp,
                    0, // sender_id: the server stamps the authenticated id
                );
                frame_id = frame_id.wrapping_add(1);
                let total = dgs.len();
                for (i, dg) in dgs.iter().enumerate() {
                    if let Err(e) = conn.send_datagram(dg.clone().into()) {
                        // Real reason, not a guess: TooLarge (MTU estimate
                        // collapsed under loss) and ConnectionLost are very
                        // different diagnoses.
                        println!("share: send_datagram failed: {e}");
                        let _ = die.send(());
                        return;
                    }
                    if i + 1 < total {
                        std::thread::sleep(Duration::from_micros(500));
                    }
                }
                encoded += 1;
                let el = window.elapsed();
                if el.as_secs() >= 10 {
                    println!(
                        "share: encoded {encoded} frames ({:.1} fps), {stale_skipped} stale skipped",
                        encoded as f64 / el.as_secs_f64()
                    );
                    encoded = 0;
                    stale_skipped = 0;
                    window = std::time::Instant::now();
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
        mic_route.store(true, Ordering::Relaxed);
        let mut send = match SendPipeline::new_music(256_000) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("share-audio: music encoder init failed: {e}");
                mic_route.store(false, Ordering::Relaxed);
                return mic_rx;
            }
        };
        let flags = parley_proto::media::FLAG_SPEAKING | parley_proto::media::FLAG_STEREO;
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
                        // with sys-only at delivery pace, below.
                        mic_rx = None;
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
) -> Result<(ShareHandle, ShareController), String> {
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

    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel();
    let stop_for_thread = stop_flag.clone();
    let pw_handle = std::thread::spawn(move || {
        pw_thread(node_id, fd, frame_tx, stop_for_thread, setup_tx);
    });
    // Encoder-death signal: the encoder thread owns the only sender, so
    // it fires on send failure AND on any exit (pw death → rx closes →
    // return → drop). Either way the supervisor must tear everything
    // down — a dead connection must not leave a portal session and pw
    // loop running (found in the 6e netem run: endless "encoder behind").
    let (die_tx, mut die_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let enc_handle =
        std::thread::spawn(move || encoder_thread(frame_rx, conn, force_idr, video_loss, die_tx));

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
        // Already-closed (external stop) is fine — errors ignored.
        let _ = session.close().await;
        let _ = events.send(ShareEvent::Ended { external });
    });

    Ok((handle, ShareController { stop_tx }))
}
