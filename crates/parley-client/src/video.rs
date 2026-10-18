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
    VIDEO_BITRATE_BPS, VIDEO_HEIGHT, VIDEO_WIDTH, VideoEncoder, bitrate_for, create_encoder_auto,
    target_fps,
};
use parley_core::video_receive::{SharedVideoReceiveHub, VideoReceiveHub};
use parley_proto::video::{MAX_FRAGMENT_PAYLOAD, fragment_frame_with_max, hinted_frame_rate};

/// Spawn an mpv that reads raw RGB24 from our stdin pipe, sized to the
/// decoded stream. One per remote sender, named after them in the title.
///
/// mpv 0.41: `--rawvideo=w:h:format` was REMOVED — the options are
/// `--demuxer-rawvideo-w/h`, `-mp-format` (pixel-format names; plain
/// `-format` is FourCC-only), `-fps`. `--title=value` form is
/// mandatory (space-separated is rejected). stderr is inherited so a
/// failing mpv is loud instead of silently eating frames.
fn spawn_mpv(
    sender_id: u32,
    width: usize,
    height: usize,
    // 7h HD: the sender's cadence (the stream's FRAG_HIGH_RATE hint).
    // mpv's rawvideo demuxer needs a nominal rate: told 30 for a 60 fps
    // stream it plays at half speed, which reads as a client bug.
    high_rate: bool,
) -> Result<Child, String> {
    let demuxer_fps = hinted_frame_rate(high_rate);
    Command::new("mpv")
        .args([
            &format!("--title=parley: user {sender_id}"),
            "--cache=no",
            "--no-audio",
            "--demuxer=rawvideo",
            &format!("--demuxer-rawvideo-w={width}"),
            &format!("--demuxer-rawvideo-h={height}"),
            "--demuxer-rawvideo-mp-format=rgb24",
            &format!("--demuxer-rawvideo-fps={demuxer_fps}"),
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
    for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
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
    /// 7h HD: the two rungs in bits per second. Derived from the live
    /// profile (resolution × rate) rather than fixed at 720p30's
    /// numbers — see [`Self::set_base`].
    clean_bps: u32,
    robust_bps: u32,
    last_switch: std::time::Instant,
    low_since: Option<std::time::Instant>,
    last_step: std::time::Instant,
    last_kf_poke: std::time::Instant,
}

impl VideoLadder {
    /// The 720p30 rungs (the synthetic sender's profile).
    pub fn new() -> Self {
        Self::with_base(VIDEO_BITRATE_BPS)
    }

    /// Rungs scaled to a profile: clean at the caller's target, robust
    /// at 40% of it — the ratio 1.0/2.5 Mbps had at 720p30, kept because
    /// it is what makes a frame ~4 fragments instead of ~9 (the 6e
    /// frame-survival arithmetic).
    pub fn with_base(clean_bps: u32) -> Self {
        Self {
            ema_permille: 0.0,
            robust: false,
            clean_bps,
            robust_bps: robust_rung(clean_bps),
            last_switch: std::time::Instant::now(),
            low_since: None,
            last_step: std::time::Instant::now(),
            last_kf_poke: std::time::Instant::now(),
        }
    }

    /// 7h HD: a negotiation changed the resolution or the effective
    /// frame rate, so the rungs move with it. Returns the bitrate to
    /// apply right now — the new clean target, or the scaled robust rung
    /// when the ladder is in its loss state (a rebuild must not silently
    /// promote a loss epoch back to full rate).
    pub fn set_base(&mut self, clean_bps: u32) -> u32 {
        self.clean_bps = clean_bps;
        self.robust_bps = robust_rung(clean_bps);
        if self.robust {
            self.robust_bps
        } else {
            self.clean_bps
        }
    }

    /// Test hook: put the ladder in its loss state without waiting out
    /// the 1 s step / 5 s switch gates (the transitions themselves are
    /// time-based — the netem runs exercise those).
    #[cfg(test)]
    pub(crate) fn select_robust_for_test(&mut self) {
        self.robust = true;
    }

    /// The rung the ladder is on.
    pub fn current_bps(&self) -> u32 {
        if self.robust {
            self.robust_bps
        } else {
            self.clean_bps
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
                return Some(self.robust_bps);
            }
        } else if self.ema_permille < 10.0 {
            let since = *self.low_since.get_or_insert(now);
            if now.duration_since(since).as_secs() >= 20
                && now.duration_since(self.last_switch).as_secs() >= 5
            {
                self.robust = false;
                self.last_switch = now;
                self.low_since = None;
                return Some(self.clean_bps);
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

/// The robust rung for a clean target: 40% of it, floored so a small
/// stream still gets a usable picture and capped by the ladder's own
/// clean value (a 800 kbps clean target cannot have a higher robust
/// rung).
fn robust_rung(clean_bps: u32) -> u32 {
    ((clean_bps as f64 * 0.4) as u32).clamp(300_000, clean_bps)
}

// ---- send side (synthetic; real capture lives in screen.rs) ----

/// Owns the encoder, frame_id, and the video seq counter (local:
/// receivers demux on FLAG_VIDEO, never seq — audio's counter lives in
/// its own pipeline and the two never interact).
pub struct ShareSender {
    encoder: Box<dyn VideoEncoder>,
    frame_id: u32,
    seq: u16,
    /// 7h: live fragment payload budget (the connection's current
    /// path-MTU estimate, capped by the compiled-in maximum). `set_
    /// fragment_budget` re-reads it after a TooLarge rejection.
    max_payload: usize,
}

impl ShareSender {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            encoder: create_encoder_auto(VIDEO_WIDTH, VIDEO_HEIGHT).map_err(|e| e.to_string())?,
            frame_id: 0,
            // Video seq space sits above audio's low range purely as a
            // diagnostic aid.
            seq: 0x8000,
            max_payload: MAX_FRAGMENT_PAYLOAD,
        })
    }

    /// 7h: the fragmenter's byte budget (see the capture path's
    /// `fragment_budget_for`).
    pub fn set_fragment_budget(&mut self, max_payload: usize) {
        self.max_payload = max_payload.clamp(200, MAX_FRAGMENT_PAYLOAD);
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
        // fragmenter does — same budget or the two drift apart).
        self.seq = self
            .seq
            .wrapping_add(frame.len().div_ceil(self.max_payload).max(1) as u16);
        let fps = target_fps();
        let timestamp = (t as f32 * fps.recip() * 48_000.0) as u32;
        let dgs = fragment_frame_with_max(
            self.frame_id,
            keyframe,
            &frame,
            first_seq,
            timestamp,
            0, // sender_id: the server stamps the authenticated id
            self.max_payload,
            fps > 45.0,
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
            match spawn_mpv(frame.sender_id, frame.width, frame.height, frame.high_rate) {
                Ok(child) => {
                    println!(
                        "video: user {} sharing {}x{} @ {} fps",
                        frame.sender_id,
                        frame.width,
                        frame.height,
                        hinted_frame_rate(frame.high_rate)
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
            // 3b: a dead viewer stops paying decode too — hub-side
            // suppression holds until the sender's next share restart
            // (frame_id-0 keyframe), so a closed window costs nothing.
            hub.mark_stopped(frame.sender_id);
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
pub fn handle_video_datagram(viewers: &Viewers, sender_id: u32, codec: u8, payload: &[u8]) {
    viewers
        .hub
        .push_datagram_with_codec(sender_id, codec, payload);
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
        viewers.shown.lock().unwrap().clear();
        return "video: no active shares".to_string();
    }
    // 7h: the decode counters join the assembler counters. "Frames
    // arrived" and "pictures came out" are different facts, and only the
    // second one means a viewer saw anything — a stream with thousands
    // assembled and zero decoded is the fingerprint of a bitstream a
    // decoder cannot use, and this line now says so.
    let decode: std::collections::HashMap<u32, (u64, u64, u64)> = viewers
        .hub
        .decode_stats()
        .into_iter()
        .map(|(id, decoded, no_picture, errors)| (id, (decoded, no_picture, errors)))
        .collect();
    let mut shown = viewers.shown.lock().unwrap();
    let parts: Vec<String> = stats
        .iter()
        .map(|&(id, _completed, dropped, skipped, shed)| {
            let (decoded, no_picture, errors) = decode.get(&id).copied().unwrap_or((0, 0, 0));
            format!(
                "user {id}: {} shown ({decoded} decoded, {no_picture} no-picture, {errors} decode \
                 errors), {dropped} dropped, {skipped} skipped, {shed} shed",
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
    // 7h HD: the profile's rate (60 fps with a live hardware encoder),
    // not the software constant.
    let fps = target_fps();
    let frame_ms = (1000.0 / fps) as u64;
    let mut interval = tokio::time::interval(Duration::from_millis(frame_ms));
    // A frame that overruns its slot skips the next tick rather than
    // bursting catch-up encodes — real-time pacing, not catch-up.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ladder = VideoLadder::with_base(bitrate_for(VIDEO_WIDTH, VIDEO_HEIGHT, fps));
    let mut t: usize = 0;
    println!(
        "share: sending {VIDEO_WIDTH}x{VIDEO_HEIGHT}@{fps} test pattern (clean rung {} bps)",
        ladder.current_bps()
    );
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
        // 7h: spread the frame over half its interval, evenly across its
        // fragments, instead of a fixed 500 µs each — the fixed sleep
        // cost a 1080p keyframe ~148 fragments × 500 µs = 74 ms, more
        // than twice the frame interval it was meant to fit inside.
        let total = dgs.len();
        let per_frag = if total > 1 {
            ((frame_ms / 2).max(1) * 1000 / (total - 1) as u64).min(500)
        } else {
            0
        };
        let mut send_failed = false;
        for (i, dg) in dgs.into_iter().enumerate() {
            if let Err(e) = conn.send_datagram(dg.into()) {
                // Real reason, not a guess (the 6e lesson: TooLarge and
                // ConnectionLost are very different diagnoses) — and only
                // the second one ends the share; a TooLarge just means
                // the path-MTU estimate moved, so drop the frame, adopt
                // the new budget, and let the next keyframe rebase the
                // viewer.
                println!(
                    "share: send_datagram failed: {e}; dropping this frame and re-reading \
                     the datagram budget"
                );
                sender.set_fragment_budget(fragment_budget_for(&conn));
                send_failed = true;
                break;
            }
            if i + 1 < total && per_frag > 0 {
                tokio::time::sleep(Duration::from_micros(per_frag)).await;
            }
        }
        if send_failed {
            if conn.close_reason().is_some() {
                // The connection is gone: the share is over.
                println!("share: connection closed — stopping test-pattern send");
                return;
            }
            // A dropped frame is one the viewer never sees; the next
            // keyframe (2 s intra period) rebases it.
            t += 1;
            continue;
        }
        t += 1;
    }
}

/// 7h: largest video-fragment payload the connection will accept right
/// now.
///
/// quinn's path-MTU estimate is the authority — it *shrinks* when the
/// path demands it, and the 6e netem run recorded it collapsing to the
/// 1200-byte floor under sustained loss — while the compiled-in
/// `MAX_FRAGMENT_PAYLOAD` is the ceiling, because an estimate larger
/// than the constant buys nothing and a one-shot rejection costs a
/// frame. Re-read after every rejection.
pub(crate) fn fragment_budget_for(conn: &quinn::Connection) -> usize {
    use parley_proto::media::{HEADER_LEN, MTU_SAFE_DATAGRAM_SIZE};
    use parley_proto::video::VIDEO_HEADER_LEN;
    let mtu = conn.max_datagram_size().unwrap_or(MTU_SAFE_DATAGRAM_SIZE);
    mtu.saturating_sub(HEADER_LEN)
        .saturating_sub(VIDEO_HEADER_LEN)
        .clamp(200, MAX_FRAGMENT_PAYLOAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robust_rung_keeps_the_720p30_ratio_and_a_floor() {
        // 7h HD: the rungs scale with the profile now, but the RATIO is
        // the one 6e measured (1 Mbps robust : 2.5 Mbps clean — the
        // frame-survival arithmetic), and a small stream still gets a
        // usable rung.
        assert_eq!(robust_rung(VIDEO_BITRATE_BPS), VIDEO_ROBUST_BITRATE_BPS);
        assert_eq!(robust_rung(11_250_000), 4_500_000); // 1080p60 clean
        assert_eq!(robust_rung(500_000), 300_000, "floor, not 200 kbps");
    }

    #[test]
    fn ladder_rungs_follow_the_profile() {
        let mut ladder = VideoLadder::new();
        assert_eq!(ladder.current_bps(), VIDEO_BITRATE_BPS);

        // A hardware HD profile moves both rungs.
        assert_eq!(
            ladder.set_base(11_250_000),
            11_250_000,
            "clean state → clean rung"
        );
        assert_eq!(ladder.current_bps(), 11_250_000);

        // In a loss epoch the rebuild must NOT promote back to full rate:
        // set_base returns the scaled robust rung.
        ladder.select_robust_for_test();
        assert_eq!(ladder.set_base(5_631_000), robust_rung(5_631_000));
        assert_eq!(ladder.current_bps(), robust_rung(5_631_000));

        // And a negotiation that lowers the profile still respects the
        // loss state.
        assert_eq!(ladder.set_base(2_500_000), robust_rung(2_500_000));
        assert_eq!(ladder.current_bps(), VIDEO_ROBUST_BITRATE_BPS);
    }
}
