//! Phase 5d: real screen capture through the XDG ScreenCast portal.
//!
//! ashpd drives the portal (user picks a monitor or window in the
//! compositor's picker) and hands back a PipeWire node id + fd. A
//! dedicated thread runs the pipewire main loop; its process callback
//! only memcpy's the frame and try_sends it into a bounded channel —
//! if the encoder falls behind, frames are dropped, the pw graph is
//! never stalled. A second thread owns the openh264 encoder: encode →
//! fragment → paced datagrams on the existing FLAG_VIDEO path.
//!
//! Keyframe-on-join: main.rs holds a ShareHandle and pokes it when a
//! VoiceState says a viewer joined — drop-until-IDR would otherwise
//! cost a late joiner up to one full intra period (2 s).
//!
//! Only BGRx and BGRA are requested — both are byte-compatible with
//! the BGRA input VideoEncoder::encode_bgra already takes. The wire
//! format is untouched; the negotiated size is whatever the
//! compositor serves (the viewer sizes itself from the H.264 SPS).

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use parley_core::video::VideoEncoder;
use parley_proto::video::fragment_frame;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;

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

/// Listener user data, owned by the pw loop thread.
struct UserData {
    format: spa::param::video::VideoInfoRaw,
    accepted: bool,
    width: u32,
    height: u32,
    tx: SyncSender<ToEncoder>,
    drops: u64,
}

/// Portal dance: picker dialog → (node id, pw remote fd).
async fn open_portal() -> Result<(u32, OwnedFd), String> {
    let proxy = Screencast::new()
        .await
        .map_err(|e| format!("portal: {e}"))?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| format!("create_session: {e}"))?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                // cursor composited into the frames — what screen-share
                // users expect
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

/// The pipewire side, on its own thread: main loop, stream, listener.
/// Runs until the process exits (the test client never stops a share).
fn pw_thread(node_id: u32, fd: OwnedFd, tx: SyncSender<ToEncoder>) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopBox::new(None)?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)?;
    let core = context.connect_fd(fd, None)?;

    let data = UserData {
        format: Default::default(),
        accepted: false,
        width: 0,
        height: 0,
        tx,
        drops: 0,
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "parley-share",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
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
        .register()?;

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

    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    println!("share: pipewire stream connected");
    mainloop.run();
    Ok(())
}

/// Encode → fragment → paced send. Owns the encoder once the format is
/// negotiated; drops frames that arrive before that.
fn encoder_thread(
    rx: Receiver<ToEncoder>,
    conn: quinn::Connection,
    seq: Arc<Mutex<u16>>,
    force_idr: Arc<AtomicBool>,
) {
    let mut encoder: Option<VideoEncoder> = None;
    let mut frame_id: u32 = 0;
    let mut fps = 30.0;
    let mut encoded: u64 = 0;
    let mut window = std::time::Instant::now();
    for msg in rx {
        match msg {
            ToEncoder::Negotiated {
                width,
                height,
                fps: f,
            } => {
                fps = f;
                match VideoEncoder::new(width as usize, height as usize) {
                    Ok(e) => {
                        println!("share: encoder ready ({width}x{height})");
                        encoder = Some(e);
                    }
                    Err(e) => eprintln!("share: encoder init failed: {e}"),
                }
            }
            ToEncoder::Frame(bgra) => {
                let Some(enc) = encoder.as_mut() else {
                    continue;
                };
                if force_idr.swap(false, Ordering::Relaxed) {
                    enc.force_keyframe();
                    println!("share: forced keyframe (viewer joined)");
                }
                let (frame, keyframe) = match enc.encode_bgra(&bgra) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("share: encode failed: {e}");
                        continue;
                    }
                };
                let first_seq = {
                    let mut s = seq.lock().unwrap();
                    let start = *s;
                    *s = s.wrapping_add(
                        frame
                            .len()
                            .div_ceil(parley_proto::video::MAX_FRAGMENT_PAYLOAD)
                            .max(1) as u16,
                    );
                    start
                };
                let timestamp = (frame_id as f64 * fps.recip() * 48_000.0) as u32;
                let dgs = fragment_frame(
                    frame_id, keyframe, &frame, first_seq, timestamp,
                    0, // sender_id: the server stamps the authenticated id
                );
                frame_id = frame_id.wrapping_add(1);
                let total = dgs.len();
                for (i, dg) in dgs.iter().enumerate() {
                    if conn.send_datagram(dg.clone().into()).is_err() {
                        println!("share: connection closed");
                        return;
                    }
                    if i + 1 < total {
                        std::thread::sleep(Duration::from_micros(500));
                    }
                }
                // Wire-rate telemetry: what the encoder actually sustains.
                encoded += 1;
                let el = window.elapsed();
                if el.as_secs() >= 10 {
                    println!(
                        "share: encoded {encoded} frames ({:.1} fps)",
                        encoded as f64 / el.as_secs_f64()
                    );
                    encoded = 0;
                    window = std::time::Instant::now();
                }
            }
        }
    }
    println!("share: capture ended");
}

/// Entry point: portal → two threads (pw loop, encoder). Returns a
/// handle for keyframe-on-join; the threads run until process exit.
pub async fn start(conn: quinn::Connection, seq: Arc<Mutex<u16>>) -> Result<ShareHandle, String> {
    println!("share: requesting screen capture — pick a monitor or window in the portal dialog");
    let (node_id, fd) = open_portal().await?;

    let (tx, rx) = std::sync::mpsc::sync_channel::<ToEncoder>(4);
    let force_idr = Arc::new(AtomicBool::new(false));
    let handle = ShareHandle {
        flag: force_idr.clone(),
    };
    std::thread::spawn(move || {
        if let Err(e) = pw_thread(node_id, fd, tx) {
            eprintln!("share: pipewire error: {e}");
        }
    });
    std::thread::spawn(move || encoder_thread(rx, conn, seq, force_idr));
    Ok(handle)
}
