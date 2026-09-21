//! Phase 5c/5d/6a/6e: screen-share over the media datagram path.
//!
//! Send side: `--share` (or the "share" stdin command) with the `video`
//! feature sends the synthetic moving-bar pattern; with
//! `screen-capture` (5d) it sends the real screen via the portal (see
//! screen.rs). Both fragment + send with FLAG_VIDEO, paced so a
//! keyframe doesn't burst datagrams into quinn.
//!
//! Receive side (any client): video datagrams are detected by flag,
//! assembled per sender, decoded, and piped as raw RGB24 into a spawned
//! mpv (one per sender). mpv is spawned lazily on the first decoded
//! frame and sized from the H.264 SPS — the sender's negotiated
//! resolution (720p synthetic, native monitor for real capture), never
//! a client-side guess. Video never touches the audio mixer or the
//! jitter buffer; seq counters are per-path (video's starts at 0x8000),
//! and receivers discriminate on FLAG_VIDEO before anything else.
//!
//! 6a: the stopped set closes the datagram-vs-stream ordering race at
//! share stop — see `ViewerTable`.
//!
//! 6e: the loss-driven video bitrate ladder (`VideoLadder`) mirrors
//! 4d's audio controller — viewers report assembler gap rates, senders
//! switch between Clean (2.5 Mbps) and Robust (1 Mbps) on an EMA with
//! hysteresis.

use std::collections::{HashMap, HashSet};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parley_core::video::{
    VIDEO_BITRATE_BPS, VIDEO_FPS, VIDEO_HEIGHT, VIDEO_WIDTH, VideoDecoder, VideoEncoder,
    create_encoder,
};
use parley_proto::video::{VideoAssembler, decode_fragment, fragment_frame};

/// Spawn an mpv that reads raw RGB24 from our stdin pipe, sized to the
/// decoded stream. One per remote sender, named after them in the title.
///
/// mpv 0.41: `--rawvideo=w:h:format` was REMOVED — the options are
/// `--demuxer-rawvideo-w/h`, `-mp-format` (pixel-format names; plain
/// `-format` is FourCC-only), `-fps`. `--title=value` form is
/// mandatory (space-separated is rejected). stderr is inherited so a
/// failing mpv is loud instead of silently eating frames.
fn spawn_mpv(sender_id: u32, width: usize, height: usize) -> Result<Child, String> {
    Command::new("mpv")
        .args([
            &format!("--title=parley: user {sender_id}"),
            "--cache=no",
            "--no-audio",
            "--demuxer=rawvideo",
            &format!("--demuxer-rawvideo-w={width}"),
            &format!("--demuxer-rawvideo-h={height}"),
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

// ---- 6e: loss-driven video bitrate ladder ----

/// Robust rung: ~4 fragments/frame at typical screen-content frame
/// sizes vs ~9 at Clean — frame survival under 10% datagram loss
/// roughly doubles (0.9⁴ ≈ 66% vs 0.9⁹ ≈ 39%).
pub const VIDEO_ROBUST_BITRATE_BPS: u32 = 1_000_000;

/// Mirrors 4d's audio controller on the video path: EMA over viewer
/// video-gap reports, hysteresis, cooldown. Down at EMA > 30‰, back up
/// after 20 s below 10‰, 5 s switch cooldown. Stepped at most 1 Hz
/// (per-frame callers are fine — the gate is internal) so the EMA time
/// constant matches 4d's. The set_bitrate it triggers recreates the
/// encoder: the next frame is a standalone IDR, which receivers'
/// drop-until-IDR absorbs by design.
pub struct VideoLadder {
    ema_permille: f32,
    robust: bool,
    last_switch: std::time::Instant,
    low_since: Option<std::time::Instant>,
    last_step: std::time::Instant,
    last_kf_poke: std::time::Instant,
}

impl VideoLadder {
    pub fn new() -> Self {
        Self {
            ema_permille: 0.0,
            robust: false,
            last_switch: std::time::Instant::now(),
            low_since: None,
            last_step: std::time::Instant::now(),
            last_kf_poke: std::time::Instant::now(),
        }
    }

    /// Feed the latest viewer report (permille). Returns Some(bps) when
    /// the ladder switched rungs — the caller applies it via
    /// set_bitrate.
    pub fn step(&mut self, observed_permille: u32) -> Option<u32> {
        let now = std::time::Instant::now();
        if now.duration_since(self.last_step) < Duration::from_secs(1) {
            return None;
        }
        self.last_step = now;
        self.ema_permille = 0.7 * self.ema_permille + 0.3 * observed_permille as f32;
        if !self.robust {
            if self.ema_permille > 30.0 && now.duration_since(self.last_switch).as_secs() >= 5 {
                self.robust = true;
                self.last_switch = now;
                self.low_since = None;
                return Some(VIDEO_ROBUST_BITRATE_BPS);
            }
        } else if self.ema_permille < 10.0 {
            let since = *self.low_since.get_or_insert(now);
            if now.duration_since(since).as_secs() >= 20
                && now.duration_since(self.last_switch).as_secs() >= 5
            {
                self.robust = false;
                self.last_switch = now;
                self.low_since = None;
                return Some(VIDEO_BITRATE_BPS);
            }
        } else {
            self.low_since = None;
        }
        None
    }

    /// 6e: during a sustained loss epoch, re-poke IDRs at the intra
    /// period — drop-until-IDR recovery is a per-IDR lottery (a 1080p
    /// IDR is dozens of fragments; every one must arrive), so buy a
    /// ticket every 2 s until the epoch ends. Expected recovery: ~30 s
    /// at sustained 10% loss, seconds at 5%, immediate once loss clears.
    pub fn should_force_keyframe(&mut self) -> bool {
        let now = std::time::Instant::now();
        if !self.robust || self.ema_permille <= 30.0 {
            return false;
        }
        if now.duration_since(self.last_kf_poke) >= Duration::from_secs(2) {
            self.last_kf_poke = now;
            true
        } else {
            false
        }
    }
}

impl Default for VideoLadder {
    fn default() -> Self {
        Self::new()
    }
}

// ---- send side (synthetic; real capture lives in screen.rs) ----

/// Owns the encoder, frame_id, and the video seq counter (local:
/// receivers demux on FLAG_VIDEO, never seq — audio's counter lives in
/// its own pipeline and the two never interact).
pub struct ShareSender {
    encoder: Box<dyn VideoEncoder>,
    frame_id: u32,
    seq: u16,
}

impl ShareSender {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            encoder: create_encoder(VIDEO_WIDTH, VIDEO_HEIGHT).map_err(|e| e.to_string())?,
            frame_id: 0,
            // Video seq space sits above audio's low range purely as a
            // diagnostic aid.
            seq: 0x8000,
        })
    }

    /// 6e: ladder-driven bitrate change (encoder recreation; next frame
    /// is a standalone IDR with in-band SPS/PPS).
    pub fn set_bitrate(&mut self, bps: u32) {
        self.encoder.set_bitrate(bps);
    }

    /// 6e: IDR re-poke during loss epochs (ladder-driven).
    pub fn force_keyframe(&mut self) {
        self.encoder.force_keyframe();
    }

    /// Encode + fragment + return the datagrams for frame `t`.
    /// Consumes one seq block; returns (datagrams, is_keyframe).
    pub fn next_frame(&mut self, t: usize) -> Result<(Vec<Vec<u8>>, bool), String> {
        let bgra = test_frame_bgra(t);
        let (frame, keyframe) = self.encoder.encode_bgra(&bgra).map_err(|e| e.to_string())?;
        let first_seq = self.seq;
        // Advance by the fragment count (computed the same way the
        // fragmenter does).
        self.seq = self.seq.wrapping_add(
            frame
                .len()
                .div_ceil(parley_proto::video::MAX_FRAGMENT_PAYLOAD)
                .max(1) as u16,
        );
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
/// mpv spawns lazily on the first decoded frame — its size comes from
/// the H.264 SPS, so any sender resolution displays correctly.
pub struct Viewer {
    sender_id: u32,
    assembler: VideoAssembler,
    decoder: VideoDecoder,
    mpv: Option<Child>,
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
            mpv: None,
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
            match self.decoder.decode_with_dims(&frame.data) {
                Ok(Some((rgb, w, h))) => {
                    if self.mpv.is_none() {
                        println!("video: user {} sharing {w}x{h}", self.sender_id);
                        match spawn_mpv(self.sender_id, w, h) {
                            Ok(child) => self.mpv = Some(child),
                            Err(e) => {
                                eprintln!("video: {e}");
                                self.dead = true;
                                return;
                            }
                        }
                    }
                    let Some(mpv) = self.mpv.as_mut() else {
                        return;
                    };
                    if feed_mpv(mpv, &rgb) {
                        self.shown += 1;
                    } else {
                        self.dead = true;
                        let _ = mpv.try_wait(); // reap if already exited
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
        if let Some(mut mpv) = self.mpv.take() {
            let _ = mpv.kill();
            let _ = mpv.wait();
        }
    }
}

/// Shared receiver state: per-sender viewers plus the stopped set.
///
/// The stopped set exists because QUIC gives NO ordering between
/// datagrams and stream bytes: a straggler video datagram can land
/// AFTER the client processes the ShareStopped control message, and
/// without the set the datagram reader would spawn a fresh viewer
/// showing one frozen frame. Entries clear on the sender's next
/// KEYFRAME fragment — every new share starts with an encoder-fresh
/// IDR, and an IDR whose datagram was lost is followed by another IDR
/// at the intra period, so un-stop never depends on a specific
/// frame_id arriving.
#[derive(Default)]
pub struct ViewerTable {
    viewers: HashMap<u32, Viewer>,
    stopped: HashSet<u32>,
}

/// Shared receiver map.
pub type Viewers = Arc<Mutex<ViewerTable>>;

pub fn new_viewers() -> Viewers {
    Arc::new(Mutex::new(ViewerTable::default()))
}

/// Route one video datagram payload. Called from the media datagram
/// reader for any datagram with FLAG_VIDEO set.
pub fn handle_video_datagram(viewers: &Viewers, sender_id: u32, payload: &[u8]) {
    let Ok(frag) = decode_fragment(payload) else {
        return; // malformed video fragment: drop, stats live in assembler
    };
    let mut table = viewers.lock().unwrap();
    if table.stopped.contains(&sender_id) {
        if !frag.keyframe {
            return; // straggler from the stopped share
        }
        table.stopped.remove(&sender_id); // new share begins
    }
    if let Some(v) = table.viewers.get_mut(&sender_id) {
        v.feed_fragment(frag);
        return;
    }
    // First video from this sender: spawn a viewer.
    println!("video: starting viewer for user {sender_id}");
    match Viewer::new(sender_id) {
        Ok(mut v) => {
            v.feed_fragment(frag);
            table.viewers.insert(sender_id, v);
        }
        Err(e) => eprintln!("video: viewer init failed: {e}"),
    }
}

/// A sender stopped sharing: kill their viewer (Drop kills mpv) and
/// remember the stop so late datagrams can't resurrect it.
pub fn mark_stopped(viewers: &Viewers, sender_id: u32) {
    let mut table = viewers.lock().unwrap();
    table.viewers.remove(&sender_id);
    table.stopped.insert(sender_id);
}

/// Print + reset per-sender stats. Called from a slow ticker.
pub fn stats_line(viewers: &Viewers) -> String {
    let mut table = viewers.lock().unwrap();
    if table.viewers.is_empty() {
        return "video: no active shares".to_string();
    }
    let parts: Vec<String> = table
        .viewers
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

/// Drop a departed sender's viewer (kills their mpv) and forget their
/// stopped-state — they left voice, nothing more can arrive.
pub fn remove_viewer(viewers: &Viewers, sender_id: u32) {
    let mut table = viewers.lock().unwrap();
    table.viewers.remove(&sender_id);
    table.stopped.remove(&sender_id);
}

/// 6e: worst per-sender video gap rate since the last call (resets
/// each sender's window). None when no viewers are active.
pub fn video_gap_rates(viewers: &Viewers) -> Option<u32> {
    let mut table = viewers.lock().unwrap();
    if table.viewers.is_empty() {
        return None;
    }
    table
        .viewers
        .values_mut()
        .map(|v| v.assembler.gap_rate_permille())
        .max()
}

// ---- send-side task (synthetic pattern) ----

/// The share loop: encode at ~30 fps, pace fragments, send, and step
/// the loss ladder once per frame (internally gated to 1 Hz).
/// `pace` sleeps between fragments so a ~200-datagram keyframe doesn't
/// hit quinn's buffers as one burst. Stopped by aborting the task
/// (6a): a mid-frame abort leaves at most a partial frame on the wire,
/// which receivers' drop-until-IDR absorbs.
pub async fn share_task(
    conn: quinn::Connection,
    mut sender: ShareSender,
    video_loss: Arc<AtomicU32>,
) {
    let frame_ms = (1000.0 / VIDEO_FPS) as u64;
    let mut interval = tokio::time::interval(Duration::from_millis(frame_ms));
    // A frame that overruns its slot skips the next tick rather than
    // bursting catch-up encodes — real-time pacing, not catch-up.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ladder = VideoLadder::new();
    let mut t: usize = 0;
    println!("share: sending {VIDEO_WIDTH}x{VIDEO_HEIGHT}@{VIDEO_FPS} test pattern");
    loop {
        interval.tick().await;
        if let Some(bps) = ladder.step(video_loss.load(Ordering::Relaxed)) {
            sender.set_bitrate(bps);
            println!("share: bitrate → {bps} bps (video loss ladder)");
        }

        if ladder.should_force_keyframe() {
            sender.force_keyframe();
            println!("share: IDR re-poke (loss epoch)");
        }
        let (dgs, _keyframe) = match sender.next_frame(t) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("share encode failed: {e}");
                return;
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
