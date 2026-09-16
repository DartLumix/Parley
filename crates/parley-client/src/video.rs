//! Phase 5c: synthetic screen-share over the media datagram path.
//!
//! Send side (`--share`): generate the moving-bar test pattern (same
//! generator as parley-core's video tests), encode 720p30, fragment,
//! send with FLAG_VIDEO. Paced: one frame per 33 ms tick, fragments
//! spaced to avoid bursting hundreds of datagrams into quinn at once.
//!
//! Receive side (any client): video datagrams are detected by flag,
//! assembled per sender, decoded, and piped as raw RGB24 into a
//! spawned mpv (one per sender). Video never touches the audio mixer
//! or the jitter buffer — the paths only share the seq space, and
//! receivers discriminate on FLAG_VIDEO before anything else.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parley_core::video::{VIDEO_FPS, VIDEO_HEIGHT, VIDEO_WIDTH, VideoDecoder, VideoEncoder};
use parley_proto::video::{VideoAssembler, decode_fragment, fragment_frame};

/// Spawn an mpv that reads raw RGB24 from our stdin pipe.
/// One per remote sender, named after them in the window title.
///
/// mpv removed `--rawvideo=w:h:format` (your 0.41 errors on it); the
/// options are `--demuxer-rawvideo-w/h/mp-format/fps` now (`-format`
/// is FourCC-only, `-mp-format` takes pixel-format names like rgb24).
/// stderr is inherited so a failing mpv is loud instead of silently
/// eating frames into a dead pipe.
fn spawn_mpv(sender_id: u32) -> Result<Child, String> {
    Command::new("mpv")
        .args([
            "--title",
            &format!("parley: user {sender_id}"),
            "--no-audio",
            "--demuxer=rawvideo",
            &format!("--demuxer-rawvideo-w={VIDEO_WIDTH}"),
            &format!("--demuxer-rawvideo-h={VIDEO_HEIGHT}"),
            "--demuxer-rawvideo-mp-format=rgb24",
            &format!("--demuxer-rawvideo-fps={VIDEO_FPS}"),
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to spawn mpv (is it installed?): {e}"))
}

/// Write one decoded RGB frame to an mpv child's stdin. Returns false
/// when the write fails — mpv died (window closed, bad args, crash) —
/// so the caller can mark the viewer dead.
fn feed_mpv(child: &mut Child, rgb: &[u8]) -> bool {
    use std::io::Write;
    let Some(stdin) = child.stdin.as_mut() else {
        return false;
    };
    stdin.write_all(rgb).is_ok() && stdin.flush().is_ok()
}

/// The moving-bar pattern from parley-core's tests — identical
/// generator, so the wire carries the exact pattern the tests verify.
fn test_frame_bgra(t: usize) -> Vec<u8> {
    let (w, h) = (VIDEO_WIDTH, VIDEO_HEIGHT);
    let mut bgra = vec![0u8; w * h * 4];
    for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
        let x = i % w;
        let y = i / w;
        let bar_x = ((t * 13) % w) as i32;
        let in_bar = (x as i32 - bar_x).abs() < 24 && y % 4 == 0;
        px[0] = (x * 255 / w) as u8; // B
        px[1] = (y * 255 / h) as u8; // G
        px[2] = if in_bar { 255 } else { 64 }; // R
        px[3] = 255; // A
    }
    bgra
}

// ---- send side ----

/// Owns the encoder, frame_id, and the seq counter shared with the
/// sender's audio path (same 16-bit space, discrimininated by flag).
pub struct ShareSender {
    encoder: VideoEncoder,
    frame_id: u32,
    /// Next media seq to use. Shared with the audio encode task via
    /// the mutex — both paths allocate from one wrapping counter.
    seq: Arc<Mutex<u16>>,
}

impl ShareSender {
    pub fn new(seq: Arc<Mutex<u16>>) -> Result<Self, String> {
        Ok(Self {
            encoder: VideoEncoder::new(VIDEO_WIDTH, VIDEO_HEIGHT).map_err(|e| e.to_string())?,
            frame_id: 0,
            seq,
        })
    }

    /// Encode + fragment + return the datagrams for frame `t`.
    /// Consumes one seq block; returns (datagrams, is_keyframe).
    pub fn next_frame(&mut self, t: usize) -> Result<(Vec<Vec<u8>>, bool), String> {
        let bgra = test_frame_bgra(t);
        let (frame, keyframe) = self.encoder.encode_bgra(&bgra).map_err(|e| e.to_string())?;
        let first_seq = {
            let mut s = self.seq.lock().unwrap();
            let start = *s;
            // Advance by the fragment count (computed the same way the
            // fragmenter does) so audio never reuses these seqs.
            *s = s.wrapping_add(
                frame
                    .len()
                    .div_ceil(parley_proto::video::MAX_FRAGMENT_PAYLOAD)
                    .max(1) as u16,
            );
            start
        };
        let timestamp = (t as f32 * VIDEO_FPS.recip() * 48_000.0) as u32;
        let dgs = fragment_frame(
            self.frame_id,
            keyframe,
            &frame,
            first_seq,
            timestamp,
            0, // sender_id: the server stamps the authenticated id
        );
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok((dgs, keyframe))
    }
}

// ---- receive side ----

/// Per-sender receive state: assembler + decoder + mpv child.
pub struct Viewer {
    sender_id: u32,
    assembler: VideoAssembler,
    decoder: VideoDecoder,
    mpv: Child,
    /// mpv died (window closed / exited)? Stop feeding, keep consuming
    /// datagrams so the stream stays accounted for.
    dead: bool,
    /// Frames shown since last stats print.
    shown: u64,
}

impl Viewer {
    fn new(sender_id: u32) -> Result<Self, String> {
        Ok(Self {
            sender_id,
            assembler: VideoAssembler::new(),
            decoder: VideoDecoder::new().map_err(|e| e.to_string())?,
            mpv: spawn_mpv(sender_id)?,
            dead: false,
            shown: 0,
        })
    }

    fn feed_fragment(&mut self, frag: parley_proto::video::VideoFragment<'_>) {
        self.assembler.push_fragment(frag);
        while let Some(frame) = self.assembler.pop_frame() {
            if self.dead {
                continue; // keep consuming, don't decode for a dead viewer
            }
            match self.decoder.decode(&frame.data) {
                Ok(Some(rgb)) => {
                    if feed_mpv(&mut self.mpv, &rgb) {
                        self.shown += 1;
                    } else {
                        self.dead = true;
                        let _ = self.mpv.try_wait(); // reap if already exited
                        println!(
                            "video: viewer for user {} exited — not displaying (window closed?)",
                            self.sender_id
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("video decode error: {e}"),
            }
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.mpv.kill();
        let _ = self.mpv.wait();
    }
}

/// Shared receiver map. Keyed by sender_id (server-stamped).
pub type Viewers = Arc<Mutex<HashMap<u32, Viewer>>>;

pub fn new_viewers() -> Viewers {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Route one video datagram payload. Called from the media datagram
/// reader for any datagram with FLAG_VIDEO set.
pub fn handle_video_datagram(viewers: &Viewers, sender_id: u32, payload: &[u8]) {
    let Ok(frag) = decode_fragment(payload) else {
        return; // malformed video fragment: drop, stats live in assembler
    };
    let mut viewers = viewers.lock().unwrap();
    if let Some(v) = viewers.get_mut(&sender_id) {
        v.feed_fragment(frag);
        return;
    }
    // First video from this sender: spawn a viewer.
    println!("video: starting viewer for user {sender_id}");
    match Viewer::new(sender_id) {
        Ok(mut v) => {
            v.feed_fragment(frag);
            viewers.insert(sender_id, v);
        }
        Err(e) => eprintln!("video: viewer spawn failed: {e}"),
    }
}

/// Print + reset per-sender stats. Called from a slow ticker.
pub fn stats_line(viewers: &Viewers) -> String {
    let mut viewers = viewers.lock().unwrap();
    if viewers.is_empty() {
        return "video: no active shares".to_string();
    }
    let parts: Vec<String> = viewers
        .iter_mut()
        .map(|(&id, v)| {
            let s = format!(
                "user {id}: {} shown, {} dropped, {} skipped",
                v.shown, v.assembler.frames_dropped, v.assembler.fragments_skipped
            );
            v.shown = 0;
            s
        })
        .collect();
    format!("video: {}", parts.join(" | "))
}

/// Drop a departed sender's viewer (kills their mpv).
pub fn remove_viewer(viewers: &Viewers, sender_id: u32) {
    viewers.lock().unwrap().remove(&sender_id);
}

// ---- send-side task ----

/// The `--share` loop: encode at ~30 fps, pace fragments, send.
/// `pace` sleeps between fragments so a ~200-datagram keyframe doesn't
/// hit quinn's buffers as one burst.
pub async fn share_task(conn: quinn::Connection, sender: Arc<Mutex<ShareSender>>) {
    let frame_ms = (1000.0 / VIDEO_FPS) as u64;
    let mut interval = tokio::time::interval(Duration::from_millis(frame_ms));
    // A frame that overruns its slot skips the next tick rather than
    // bursting catch-up encodes — real-time pacing, not catch-up.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut t: usize = 0;
    println!("share: sending {VIDEO_WIDTH}x{VIDEO_HEIGHT}@{VIDEO_FPS} test pattern");
    loop {
        interval.tick().await;
        let (dgs, _keyframe) = {
            let mut s = sender.lock().unwrap();
            match s.next_frame(t) {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("share encode failed: {e}");
                    return;
                }
            }
        };
        for (i, dg) in dgs.iter().enumerate() {
            if conn.send_datagram(dg.clone().into()).is_err() {
                println!("share: connection closed");
                return;
            }
            // Pace: ~500 µs between fragments ≈ 2 MB/s ceiling, well
            // under the 2.5 Mbps encode target.
            if i + 1 < dgs.len() {
                tokio::time::sleep(Duration::from_micros(500)).await;
            }
        }
        t += 1;
    }
}
