//! Phase 5c/5d/6a/6e/6.9: screen-share over the media datagram path.
//!
//! Send side: `--share` (or the "share" stdin command) with the `video`
//! feature sends the synthetic moving-bar pattern; with
//! `screen-capture` (5d) it sends the real screen via the portal (see
//! screen.rs). Both fragment + send with FLAG_VIDEO, paced so a
//! keyframe doesn't burst datagrams into quinn.
//!
//! Receive side (6.9): everything protocol lives in
//! parley-core's `VideoReceiveHub` (assembler, decoder, stopped-set,
//! gap rates, bounded frame queue). This file is a DISPLAY consumer:
//! one std thread drains decoded frames and feeds a spawned mpv per
//! sender (lazily, sized from the decoded frame). The Phase 7 Flutter
//! client becomes a second consumer of the same hub — no mpv knowledge
//! anywhere near the protocol path.
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
    VIDEO_BITRATE_BPS, VIDEO_FPS, VIDEO_HEIGHT, VIDEO_WIDTH, VideoEncoder, create_encoder,
};
use parley_core::video_receive::{SharedVideoReceiveHub, VideoReceiveHub};
use parley_proto::video::fragment_frame;

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

// ---- receive side: hub + mpv display consumer (6.9) ----

/// The client-side receive state: the protocol hub plus the display
/// consumer's shared bookkeeping. Public API is unchanged from the
/// pre-6.9 ViewerTable era — main.rs needs no changes.
pub struct ClientViewers {
    hub: SharedVideoReceiveHub,
    /// Per-sender frames fed to mpv since the last stats read.
    shown: Arc<Mutex<HashMap<u32, u64>>>,
    /// mpv died for this sender (window closed): don't respawn until
    /// they stop-and-restart or leave-and-return.
    dead: Arc<Mutex<HashSet<u32>>>,
    /// One-shot kill requests (stop/leave): the consumer thread reaps
    /// the mpv child. Drained every loop iteration.
    retire: Arc<Mutex<HashSet<u32>>>,
}

/// Shared receiver handle (name preserved from the ViewerTable era).
pub type Viewers = Arc<ClientViewers>;

pub fn new_viewers() -> Viewers {
    let hub = VideoReceiveHub::shared();
    let shown: Arc<Mutex<HashMap<u32, u64>>> = Arc::new(Mutex::new(HashMap::new()));
    let dead: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
    let retire: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
    std::thread::spawn({
        // mpv feeding is blocking IO (write_all to a child's stdin) —
        // a dedicated std thread, never a tokio worker. Runs for the
        // process lifetime; on client exit the stdin pipe closes and
        // mpv self-terminates on EOF (verified in the 6.9 acceptance).
        let hub = hub.clone();
        let shown = shown.clone();
        let dead = dead.clone();
        let retire = retire.clone();
        move || mpv_consumer(hub, shown, dead, retire)
    });
    Arc::new(ClientViewers {
        hub,
        shown,
        dead,
        retire,
    })
}

/// The display consumer: drain decoded frames from the hub, spawn one
/// mpv per sender (lazily, sized from the first decoded frame — the
/// sender's real resolution), feed it. This is the ONLY place in the
/// client that knows mpv exists; Phase 7's Flutter consumer replaces
/// exactly this function against the same hub.
fn mpv_consumer(
    hub: SharedVideoReceiveHub,
    shown: Arc<Mutex<HashMap<u32, u64>>>,
    dead: Arc<Mutex<HashSet<u32>>>,
    retire: Arc<Mutex<HashSet<u32>>>,
) {
    let mut mpvs: HashMap<u32, Child> = HashMap::new();
    loop {
        // Reap requests first: stop/leave kill their window.
        for id in retire.lock().unwrap().drain() {
            if let Some(mut c) = mpvs.remove(&id) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        let Some(frame) = hub.pop_frame(Duration::from_secs(1)) else {
            continue;
        };
        if dead.lock().unwrap().contains(&frame.sender_id) {
            continue; // window closed earlier: keep draining, display nothing
        }
        if let std::collections::hash_map::Entry::Vacant(e) = mpvs.entry(frame.sender_id) {
            println!("video: starting viewer for user {}", frame.sender_id);
            match spawn_mpv(frame.sender_id, frame.width, frame.height) {
                Ok(child) => {
                    println!(
                        "video: user {} sharing {}x{}",
                        frame.sender_id, frame.width, frame.height
                    );
                    e.insert(child);
                }
                Err(e) => {
                    eprintln!("video: {e}");
                    dead.lock().unwrap().insert(frame.sender_id);
                    continue;
                }
            }
        }
        let Some(mpv) = mpvs.get_mut(&frame.sender_id) else {
            continue;
        };
        if feed_mpv(mpv, &frame.rgb) {
            *shown.lock().unwrap().entry(frame.sender_id).or_insert(0) += 1;
        } else {
            dead.lock().unwrap().insert(frame.sender_id);
            let _ = mpv.try_wait(); // reap if already exited
            println!(
                "video: viewer for user {} exited — not displaying (window closed?)",
                frame.sender_id
            );
            if let Some(mut c) = mpvs.remove(&frame.sender_id) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// Route one video datagram payload. Called from the media datagram
/// reader for any datagram with FLAG_VIDEO set. Protocol handling
/// (stopped-set, assembly, decode) lives in the hub; display happens
/// on the consumer thread.
pub fn handle_video_datagram(viewers: &Viewers, sender_id: u32, payload: &[u8]) {
    viewers.hub.push_datagram(sender_id, payload);
}

/// A sender stopped sharing: the hub suppresses stragglers, the
/// consumer kills their mpv. A later keyframe (new share) spawns a
/// fresh window — so `dead` clears here too.
pub fn mark_stopped(viewers: &Viewers, sender_id: u32) {
    viewers.hub.mark_stopped(sender_id);
    viewers.retire.lock().unwrap().insert(sender_id);
    viewers.dead.lock().unwrap().remove(&sender_id);
}

/// Drop a departed sender's viewer (kills their mpv) and forget their
/// stopped-state — they left voice, nothing more can arrive.
pub fn remove_viewer(viewers: &Viewers, sender_id: u32) {
    viewers.hub.remove(sender_id);
    viewers.retire.lock().unwrap().insert(sender_id);
    viewers.dead.lock().unwrap().remove(&sender_id);
}

/// Print + reset per-sender stats. Called from a slow ticker.
pub fn stats_line(viewers: &Viewers) -> String {
    let stats = viewers.hub.stats();
    if stats.is_empty() {
        return "video: no active shares".to_string();
    }
    let mut shown = viewers.shown.lock().unwrap();
    let parts: Vec<String> = stats
        .iter()
        .map(|&(id, _completed, dropped, skipped)| {
            format!(
                "user {id}: {} shown, {dropped} dropped, {skipped} skipped",
                shown.get(&id).copied().unwrap_or(0)
            )
        })
        .collect();
    shown.clear();
    format!("video: {}", parts.join(" | "))
}

/// 6e: worst per-sender video gap rate since the last call. None when
/// no streams are active.
pub fn video_gap_rates(viewers: &Viewers) -> Option<u32> {
    viewers.hub.gap_rate_permille()
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
            if let Err(e) = conn.send_datagram(dg.clone().into()) {
                // Real reason, not a guess (the 6e lesson: TooLarge and
                // ConnectionLost are very different diagnoses).
                println!("share: send_datagram failed: {e}");
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
