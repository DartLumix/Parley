//! Display-agnostic video receive hub (6.9 client-prep; 3b hardened).
//!
//! Owns everything PROTOCOL about receiving video: per-sender
//! assembler + decoder, the stopped-set (straggler suppression after
//! ShareStopped — 6a semantics), and gap-rate reporting for the
//! sender's ladder (6e). Display-agnostic: decoded RGB frames leave
//! through a bounded queue that any consumer drains — the test
//! client's mpv feeder today, a Flutter texture bridge in Phase 7.
//! No process spawning, no mpv, no UI in here.
//!
//! 3b decode discipline:
//! - Decode runs inside `drain_streams`, called by the video task
//!   (via push_datagram) and the display consumer (via pop_frame) —
//!   NEVER by the datagram reader that also feeds the audio mixer: a
//!   1080p IDR decode is tens of ms and must not sit between datagram
//!   arrival and the jitter buffers (the laptop's rhythmic voice
//!   stutter; perf showed 39% of client CPU in openh264 decode on the
//!   reader's worker).
//! - Pre-decode skip: while the display consumer is behind (decoded
//!   queue at cap), assembled frames DEFER in the assembler's output
//!   instead of decoding immediately. When a keyframe lands in the
//!   backlog, everything older than the LAST keyframe is discarded
//!   UNDECODED — the keyframe rebases every reference those P-frames
//!   would need, and decode is the expensive part. Skips count into
//!   the gap window: a receiver too slow to decode is real signal for
//!   the sender's ladder (Robust = smaller frames + IDR re-pokes,
//!   both help it catch up). Latency is bounded by the intra period,
//!   not memory: every IDR forces a skip-drain.
//! - Dead viewers stop paying decode: the display consumer marks the
//!   hub stopped when a window dies — suppression holds until that
//!   sender's next share restart (frame_id-0 keyframe).
//!
//! Frame queue: bounded (cap 3), drop-OLDEST on overflow — a live
//! stream must never accumulate stale frames; a slow consumer loses
//! the past, never the present. This also bounds the viewer-side
//! queuing that dominated the 5e glass-to-glass tail.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use parley_proto::video::{VideoAssembler, decode_fragment};

use crate::video::VideoDecoder;

/// One decoded frame, ready for any consumer.
pub struct VideoFrame {
    pub sender_id: u32,
    /// RGB24, row-major, `width * height * 3` bytes.
    pub rgb: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Frames buffered for the consumer before the OLDEST is dropped.
const FRAME_QUEUE_CAP: usize = 3;

/// Diagnostic keyframe dump: with `PARLEY_VIDEO_DUMP=<dir>` set, every
/// keyframe this hub feeds to a decoder is appended to
/// `<dir>/sender<id>.annexb` (raw Annex-B, one file per sender).
///
/// Why: the receiver-side failures we keep chasing are stream-SHAPE
/// bugs — a mid-stream join is only decodable if the keyframe it starts
/// on repeats SPS/PPS in-band (openh264 reports `dsNoParamSets`,
/// Native:16 otherwise), and the only way to tell an encoder that
/// repeats them from one that does not is to look at what actually
/// arrived on the wire. Software openh264 and moq-vaapi differ here, so
/// a dump is the difference between guessing and knowing.
///
/// Off unless the env var is set; the dump directory is dedicated to it
/// (fixed file names). Capped per sender: keyframes are hundreds of KB
/// and a long session would otherwise fill the disk.
mod keyframe_dump {
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    /// Keyframes per sender after which dumping stops (the interesting
    /// ones — a fresh share, a late join — are always in the first few).
    const MAX_PER_SENDER: u32 = 20;

    struct Dump {
        dir: PathBuf,
        counts: Mutex<HashMap<u32, u32>>,
    }

    fn dump() -> Option<&'static Dump> {
        static DUMP: OnceLock<Option<Dump>> = OnceLock::new();
        DUMP.get_or_init(|| {
            let dir = PathBuf::from(std::env::var_os("PARLEY_VIDEO_DUMP")?);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                eprintln!("video: PARLEY_VIDEO_DUMP {}: {e}", dir.display());
                return None;
            }
            eprintln!(
                "video: dumping received keyframes to {} (PARLEY_VIDEO_DUMP, \
                 max {MAX_PER_SENDER} per sender)",
                dir.display()
            );
            Some(Dump {
                dir,
                counts: Mutex::new(HashMap::new()),
            })
        })
        .as_ref()
    }

    pub fn keyframe(sender_id: u32, frame_id: u32, data: &[u8]) {
        let Some(d) = dump() else { return };
        let n = {
            let mut counts = d.counts.lock().unwrap();
            let n = counts.entry(sender_id).or_insert(0);
            *n += 1;
            *n
        };
        if n > MAX_PER_SENDER {
            if n == MAX_PER_SENDER + 1 {
                eprintln!("video: user {sender_id}: keyframe dump full ({MAX_PER_SENDER})");
            }
            return;
        }
        let path = d.dir.join(format!("sender{sender_id}.annexb"));
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(data));
        match written {
            Ok(()) => println!(
                "video: dumped keyframe #{n} (frame_id {frame_id}, {} B) for user \
                 {sender_id} -> {}",
                data.len(),
                path.display()
            ),
            Err(e) => eprintln!("video: keyframe dump to {}: {e}", path.display()),
        }
    }
}

struct RecvStream {
    assembler: VideoAssembler,
    decoder: VideoDecoder,
    /// 3b: frames discarded assembled-but-undecoded (consumer behind).
    predecode_skipped: u64,
}

#[derive(Default)]
struct HubInner {
    streams: HashMap<u32, RecvStream>,
    /// 6a stopped-set: ShareStopped was seen for this sender; straggler
    /// datagrams are suppressed until their next keyframe fragment
    /// (every new share starts with an encoder-fresh IDR).
    stopped: HashSet<u32>,
    queue: VecDeque<VideoFrame>,
}

/// Multi-sender video receive hub. Shared as `SharedVideoReceiveHub`;
/// `push_datagram` from the video task, `pop_frame` from exactly one
/// consumer thread.
pub struct VideoReceiveHub {
    inner: Mutex<HubInner>,
    frame_ready: Condvar,
}

/// The shape every consumer shares.
pub type SharedVideoReceiveHub = Arc<VideoReceiveHub>;

impl Default for VideoReceiveHub {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoReceiveHub {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HubInner::default()),
            frame_ready: Condvar::new(),
        }
    }

    /// Convenience: hub behind an Arc.
    pub fn shared() -> SharedVideoReceiveHub {
        Arc::new(Self::new())
    }

    /// Feed one FLAG_VIDEO datagram payload (the media header already
    /// stripped — same contract the old Viewer had). Malformed
    /// fragments drop silently; the assembler's stats carry the
    /// accounting. Assembly is cheap; decode happens in
    /// `drain_streams`, on the caller's thread — the client calls this
    /// from a dedicated video task, never the audio datagram reader.
    pub fn push_datagram(&self, sender_id: u32, payload: &[u8]) {
        let Ok(frag) = decode_fragment(payload) else {
            return;
        };
        let mut inner = self.inner.lock().unwrap();
        {
            let HubInner {
                streams, stopped, ..
            } = &mut *inner;
            if stopped.contains(&sender_id) {
                if !frag.keyframe {
                    return; // straggler from the stopped share
                }
                // A NEW share always begins with an encoder-fresh IDR at
                // frame_id 0 (6a restart: fresh encoder, frame_id resets).
                // A straggler IDR from the DYING share carries a high
                // frame_id — it must not resurrect a stopped viewer (the
                // "frozen frame after stop" bug, caught in the 6.9 run).
                if frag.frame_id != 0 {
                    return; // late keyframe from the old share
                }
                stopped.remove(&sender_id); // a new share genuinely begins
            }
            if let std::collections::hash_map::Entry::Vacant(e) = streams.entry(sender_id) {
                match VideoDecoder::new() {
                    Ok(decoder) => {
                        e.insert(RecvStream {
                            assembler: VideoAssembler::new(),
                            decoder,
                            predecode_skipped: 0,
                        });
                    }
                    Err(e) => {
                        eprintln!("video: decoder init failed for user {sender_id}: {e}");
                        return;
                    }
                }
            }
            let stream = streams.get_mut(&sender_id).expect("just inserted");
            stream.assembler.push_fragment(frag);
        }
        let pushed = Self::drain_streams(&mut inner);
        drop(inner);
        if pushed {
            self.frame_ready.notify_one();
        }
    }

    /// Decode drain (3b): pull assembled frames through the decoders
    /// into the bounded display queue, honoring deferral and the
    /// pre-decode skip. Returns true when any frame was decoded (the
    /// caller notifies the consumer).
    ///
    /// Rules, per stream:
    /// - Consumer behind (queue at cap) + backlog ≤ 1 → DEFER: the
    ///   frame waits in the assembler output; one frame of latency,
    ///   decode off the critical path until it matters.
    /// - Multi-frame backlog (we fell behind) + a keyframe in it →
    ///   everything older than the LAST keyframe is discarded
    ///   undecoded and counted into the gap window; decode resumes at
    ///   the keyframe.
    /// - Multi-frame backlog without a keyframe → decode in order
    ///   (references must be preserved); the queue's drop-oldest is
    ///   the backstop. Bounded: the next IDR forces a skip-drain.
    /// - Consumer keeping up → decode everything in order (identical
    ///   to the pre-3b behavior).
    fn drain_streams(inner: &mut HubInner) -> bool {
        let HubInner { streams, queue, .. } = &mut *inner;
        let mut pushed = false;
        for (&sender_id, stream) in streams.iter_mut() {
            loop {
                if queue.len() >= FRAME_QUEUE_CAP && stream.assembler.output_len() <= 1 {
                    break; // deferred: consumer behind, nothing to judge yet
                }
                if stream.assembler.output_len() > 1
                    && let Some(k) = stream.assembler.last_keyframe_output_index()
                    && k > 0
                {
                    for _ in 0..k {
                        if let Some(f) = stream.assembler.pop_frame() {
                            stream.predecode_skipped += 1;
                            stream.assembler.note_missed_frame(f.frame_id);
                        }
                    }
                }
                let Some(frame) = stream.assembler.pop_frame() else {
                    break;
                };
                if frame.keyframe {
                    keyframe_dump::keyframe(sender_id, frame.frame_id, &frame.data);
                }
                match stream.decoder.decode_with_dims(&frame.data) {
                    Ok(Some((rgb, w, h))) => {
                        queue.push_back(VideoFrame {
                            sender_id,
                            rgb,
                            width: w,
                            height: h,
                        });
                        while queue.len() > FRAME_QUEUE_CAP {
                            queue.pop_front();
                        }
                        pushed = true;
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("video decode error: {e}"),
                }
            }
        }
        pushed
    }

    /// Next decoded frame, waiting up to `timeout`. The consumer's
    /// heartbeat: wake, drain, repeat. Freed queue space resumes a
    /// deferred backlog — the drain's skip gate sheds it to its newest
    /// keyframe, so a recovered consumer does not inherit the stall as
    /// permanent latency (the video analog of 6h's audio skip-shed,
    /// gated on keyframes because P-frames cannot be merged).
    pub fn pop_frame(&self, timeout: Duration) -> Option<VideoFrame> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(f) = inner.queue.pop_front() {
                if inner.queue.len() < FRAME_QUEUE_CAP {
                    let _ = Self::drain_streams(&mut inner);
                }
                return Some(f);
            }
            let (guard, status) = self.frame_ready.wait_timeout(inner, timeout).unwrap();
            inner = guard;
            if status.timed_out() {
                return inner.queue.pop_front();
            }
        }
    }

    /// ShareStopped seen for this sender: drop their receive state and
    /// suppress stragglers until their next keyframe (a new share).
    /// Also used by the display consumer for a dead viewer (3b): a
    /// closed window stops paying decode until the sender restarts.
    pub fn mark_stopped(&self, sender_id: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.streams.remove(&sender_id);
        inner.stopped.insert(sender_id);
    }

    /// Sender left voice entirely: drop state, forget stopped-state —
    /// nothing more can arrive from them.
    pub fn remove(&self, sender_id: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.streams.remove(&sender_id);
        inner.stopped.remove(&sender_id);
    }

    /// Worst per-sender video gap rate since the last call (resets each
    /// sender's window). None when no streams are active. Feeds the
    /// sender's 6e ladder via VideoReport.
    pub fn gap_rate_permille(&self) -> Option<u32> {
        let mut inner = self.inner.lock().unwrap();
        if inner.streams.is_empty() {
            return None;
        }
        inner
            .streams
            .values_mut()
            .map(|s| s.assembler.gap_rate_permille())
            .max()
    }

    /// Cumulative per-sender stats, sorted by sender id:
    /// (sender_id, frames_completed, frames_dropped, fragments_skipped,
    /// predecode_skipped) — for the client's stats line.
    pub fn stats(&self) -> Vec<(u32, u64, u64, u64, u64)> {
        let inner = self.inner.lock().unwrap();
        let mut v: Vec<_> = inner
            .streams
            .iter()
            .map(|(&id, s)| {
                (
                    id,
                    s.assembler.frames_completed,
                    s.assembler.frames_dropped,
                    s.assembler.fragments_skipped,
                    s.predecode_skipped,
                )
            })
            .collect();
        v.sort_unstable();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parley_proto::video::fragment_frame;

    /// Media datagrams → payloads (push_datagram takes the payload,
    /// post-media-header — same as the client's datagram reader).
    fn payloads(dgs: &[Vec<u8>]) -> Vec<Vec<u8>> {
        dgs.iter()
            .map(|dg| parley_proto::media::decode(dg).unwrap().payload.to_vec())
            .collect()
    }

    /// Noisy BGRA so keyframes are multi-fragment (flat color encodes
    /// small enough to fit one fragment).
    fn noisy_bgra(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut bgra = vec![0u8; w * h * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            let v = ((i * 7 + t * 29) % 256) as u8;
            px[0] = v;
            px[1] = v.wrapping_add(t as u8);
            px[2] = v.wrapping_mul(3);
            px[3] = 255;
        }
        bgra
    }

    /// Flat-color BGRA (value `v` in B and G, 3v in R) — decoded frames
    /// are identifiable by their color.
    fn flat_bgra(w: usize, h: usize, v: u8) -> Vec<u8> {
        let mut bgra = vec![0u8; w * h * 4];
        for px in bgra.chunks_exact_mut(4) {
            px[0] = v;
            px[1] = v;
            px[2] = v.wrapping_mul(3);
            px[3] = 255;
        }
        bgra
    }

    #[test]
    fn encoded_frames_roundtrip_through_hub() {
        let mut enc = crate::video::create_encoder(320, 240).unwrap();
        let (annexb, _key) = enc.encode_bgra(&noisy_bgra(320, 240, 0)).unwrap();
        let dgs = fragment_frame(0, true, &annexb, 0, 0, 42);

        let hub = VideoReceiveHub::new();
        for p in payloads(&dgs) {
            hub.push_datagram(9, &p);
        }

        let f = hub
            .pop_frame(Duration::from_millis(500))
            .expect("decoded frame");
        assert_eq!(f.sender_id, 9);
        assert_eq!((f.width, f.height), (320, 240));
        assert_eq!(f.rgb.len(), 320 * 240 * 3);
        // Nothing beyond the one frame.
        assert!(hub.pop_frame(Duration::from_millis(50)).is_none());
    }

    #[test]
    fn stopped_set_suppresses_stragglers_until_keyframe() {
        let mut enc = crate::video::create_encoder(320, 240).unwrap();
        // A: keyframe. B: delta (straggler fodder). C: forced keyframe.
        let (a, _) = enc.encode_bgra(&noisy_bgra(320, 240, 0)).unwrap();
        let (b, _) = enc.encode_bgra(&noisy_bgra(320, 240, 1)).unwrap();
        enc.force_keyframe();
        let (c, _) = enc.encode_bgra(&noisy_bgra(320, 240, 2)).unwrap();

        let hub = VideoReceiveHub::new();
        for p in payloads(&fragment_frame(0, true, &a, 0, 0, 42)) {
            hub.push_datagram(5, &p);
        }
        assert!(hub.pop_frame(Duration::from_millis(500)).is_some());

        // Stop arrives; B's straggler fragments must be suppressed.
        hub.mark_stopped(5);
        for p in payloads(&fragment_frame(1, false, &b, 10, 960, 42)) {
            hub.push_datagram(5, &p);
        }
        assert!(
            hub.pop_frame(Duration::from_millis(100)).is_none(),
            "straggler datagrams must not produce frames"
        );

        // A new share's keyframe un-stops and decodes standalone —
        // and per the 6.9 straggler fix, a new share ALWAYS begins at
        // frame_id 0 (fresh encoder; the sender resets the counter).
        // Un-stopping on a high frame_id would resurrect the viewer on
        // a dying share's late IDR (the "frozen frame after stop" bug).
        for p in payloads(&fragment_frame(0, true, &c, 20, 1920, 42)) {
            hub.push_datagram(5, &p);
        }
        assert!(hub.pop_frame(Duration::from_millis(500)).is_some());
    }

    #[test]
    fn slow_consumer_sheds_to_newest_keyframe() {
        // 3b: consumer absent — the queue fills to cap; further frames
        // defer; the next keyframe's drain SKIPS the stale past
        // undecoded (a slow consumer loses the past, never the present,
        // and pays no decode for what it cannot display). The queue
        // ends holding frames [1, 2, 4]: frame 3 was shed pre-decode at
        // frame 4's keyframe, frame 0 drop-oldest'd.
        let mut enc = crate::video::create_encoder(320, 240).unwrap();
        let hub = VideoReceiveHub::new();
        for i in 0..5u8 {
            enc.force_keyframe();
            let (annexb, _) = enc.encode_bgra(&flat_bgra(320, 240, i * 40)).unwrap();
            for p in payloads(&fragment_frame(i as u32, true, &annexb, 0, 0, 42)) {
                hub.push_datagram(3, &p);
            }
        }
        for expect in [1u8, 2, 4] {
            let f = hub.pop_frame(Duration::from_millis(500)).expect("frame");
            // write_rgb8 output order: R, G, B — channel 0 is RED
            // (5b ledger). Flat color v → (R,G,B) = (3v mod 256, v, v);
            // assert on GREEN: single multiply, tightest rounding.
            let g = f.rgb[1] as i32;
            assert!(
                (g - expect as i32 * 40).abs() <= 12,
                "expected frame with G≈{} got {g}",
                expect as i32 * 40
            );
        }
        assert!(hub.pop_frame(Duration::from_millis(50)).is_none());
        let &(_, completed, _dropped, _frags, predecode) = &hub.stats()[0];
        assert_eq!(completed, 5);
        assert_eq!(predecode, 1, "frame 3 must be shed undecoded");
    }

    #[test]
    fn predecode_skip_when_consumer_behind() {
        // Queue at cap; a P-frame defers; a keyframe completing behind
        // it triggers the drain, which discards the P-frame UNDECODED
        // (rebased at the keyframe) and counts it into the gap window —
        // the slow-receiver signal the sender's ladder consumes.
        let mut enc = crate::video::create_encoder(320, 240).unwrap();
        let hub = VideoReceiveHub::new();
        // Fill the decoded queue to cap: three standalone keyframes.
        for i in 0..3u32 {
            enc.force_keyframe();
            let (annexb, _) = enc
                .encode_bgra(&flat_bgra(320, 240, (i * 40) as u8))
                .unwrap();
            for p in payloads(&fragment_frame(i, true, &annexb, 0, 0, 42)) {
                hub.push_datagram(3, &p);
            }
        }
        // P-frame: queue full, single-frame backlog → deferred.
        let (p, _) = enc.encode_bgra(&noisy_bgra(320, 240, 9)).unwrap();
        for p in payloads(&fragment_frame(3, false, &p, 0, 0, 42)) {
            hub.push_datagram(3, &p);
        }
        // Keyframe behind it: multi-frame backlog + keyframe → the P
        // is skipped undecoded; the keyframe decodes (drop-oldest).
        enc.force_keyframe();
        let (k, _) = enc.encode_bgra(&flat_bgra(320, 240, 200)).unwrap();
        for p in payloads(&fragment_frame(4, true, &k, 0, 0, 42)) {
            hub.push_datagram(3, &p);
        }
        let stats = hub.stats();
        assert_eq!(stats.len(), 1);
        let &(_, completed, dropped, frag_skipped, predecode) = &stats[0];
        assert_eq!(completed, 5, "all five frames assembled");
        assert_eq!(dropped, 0);
        assert_eq!(frag_skipped, 0);
        assert_eq!(
            predecode, 1,
            "the stale P-frame must be discarded undecoded"
        );
        // Gap window: 5 completed + 1 noted-missed → ~167 permille.
        let rate = hub.gap_rate_permille().expect("active stream");
        assert!(
            (100..=250).contains(&rate),
            "expected ~167 permille, got {rate}"
        );
    }

    #[test]
    fn gap_rate_flows_through_hub() {
        // Three keyframes; frame 1 loses its first fragment → aborted
        // → completed 2, missed 1 → ~333 permille.
        let mut enc = crate::video::create_encoder(320, 240).unwrap();
        let mut frames = Vec::new();
        for t in 0..3 {
            enc.force_keyframe();
            let (annexb, _) = enc.encode_bgra(&noisy_bgra(320, 240, t)).unwrap();
            frames.push(annexb);
        }
        let hub = VideoReceiveHub::new();
        for (id, annexb) in frames.iter().enumerate() {
            let mut dgs = fragment_frame(id as u32, true, annexb, 0, 0, 42);
            if id == 1 {
                dgs.remove(0); // lose a fragment of frame 1
            }
            for p in payloads(&dgs) {
                hub.push_datagram(2, &p);
            }
        }
        // Drain the two surviving frames so the windows close.
        let mut got = 0;
        while hub.pop_frame(Duration::from_millis(200)).is_some() {
            got += 1;
        }
        assert_eq!(got, 2, "frames 0 and 2 survive");
        let rate = hub.gap_rate_permille().expect("active stream");
        assert!(
            (200..=450).contains(&rate),
            "expected ~333 permille, got {rate}"
        );
    }

    #[test]
    fn stopped_viewer_not_resurrected_by_straggler_idr() {
        // The 6.9 bug: an IDR straggler arriving AFTER ShareStopped
        // must not spawn a fresh viewer showing one frozen frame. Only
        // a NEW share's first IDR (frame_id 0) un-stops.
        let mut enc = crate::video::create_encoder(320, 240).unwrap();
        let (a, _) = enc.encode_bgra(&noisy_bgra(320, 240, 0)).unwrap();
        enc.force_keyframe();
        let (late_idr, _) = enc.encode_bgra(&noisy_bgra(320, 240, 5)).unwrap();
        enc.force_keyframe();
        // A fresh share's first IDR — same encoder, but frame_id 0 on
        // the wire (the sender resets frame_id per share).
        let hub = VideoReceiveHub::new();
        // First share runs (frame 0 decodes).
        for p in payloads(&fragment_frame(0, true, &a, 0, 0, 42)) {
            hub.push_datagram(6, &p);
        }
        assert!(hub.pop_frame(Duration::from_millis(500)).is_some());
        // Stop, then a LATE IDR straggler at a high frame_id: the hub
        // must suppress it entirely.
        hub.mark_stopped(6);
        for p in payloads(&fragment_frame(17, true, &late_idr, 0, 0, 42)) {
            hub.push_datagram(6, &p);
        }
        assert!(
            hub.pop_frame(Duration::from_millis(200)).is_none(),
            "straggler IDR at frame_id 17 must not resurrect the viewer"
        );
        // A new share's frame_id-0 keyframe DOES un-stop.
        for p in payloads(&fragment_frame(0, true, &a, 0, 0, 42)) {
            hub.push_datagram(6, &p);
        }
        assert!(hub.pop_frame(Duration::from_millis(500)).is_some());
    }
}
