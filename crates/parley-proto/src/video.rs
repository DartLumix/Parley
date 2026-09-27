//! Video fragmentation layer (Phase 5).
//!
//! One H.264 frame is too big for a single QUIC datagram (1440p
//! keyframes are hundreds of KB), so frames are split into fragments
//! that ride the existing media datagram format with FLAG_VIDEO set.
//!
//! Wire layout of the video payload (after the 11-byte media header,
//! all big-endian):
//!   [0..4]    frame_id: u32        (sender-scoped, monotonically increasing)
//!   [4..6]    frag_idx: u16        (0-based index within the frame)
//!   [6..8]    frag_count: u16      (total fragments in this frame)
//!   [8]       flags: u8            (bit 0 = keyframe)
//!   [9..]     data
//!
//! Media datagram `seq` counters are per-path (audio lives inside the
//! client's send pipeline; video senders keep a local counter starting
//! at 0x8000) — receivers discriminate on FLAG_VIDEO before consulting
//! seq, so the two spaces never interact. Audio jitter/loss paths
//! never see video datagrams.

use crate::media::MTU_SAFE_DATAGRAM_SIZE;

/// Video fragment sub-header length.
pub const VIDEO_HEADER_LEN: usize = 9;

/// Maximum payload bytes per video fragment (after both headers).
/// Sized for MTU_SAFE_DATAGRAM_SIZE — fragments must survive a
/// collapsed path-MTU estimate, not just a healthy loopback (6e).
pub const MAX_FRAGMENT_PAYLOAD: usize =
    MTU_SAFE_DATAGRAM_SIZE - crate::media::HEADER_LEN - VIDEO_HEADER_LEN;

/// Video fragment flags.
pub const FRAG_KEYFRAME: u8 = 1 << 0;

/// A single video fragment ready to be wrapped in a media datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFragment<'a> {
    pub frame_id: u32,
    pub frag_idx: u16,
    pub frag_count: u16,
    pub keyframe: bool,
    pub data: &'a [u8],
}

/// Encode a video fragment payload (the media layer wraps this with its
/// own header and FLAG_VIDEO).
pub fn encode_fragment(
    frame_id: u32,
    frag_idx: u16,
    frag_count: u16,
    keyframe: bool,
    data: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(VIDEO_HEADER_LEN + data.len());
    buf.extend_from_slice(&frame_id.to_be_bytes());
    buf.extend_from_slice(&frag_idx.to_be_bytes());
    buf.extend_from_slice(&frag_count.to_be_bytes());
    let flags = if keyframe { FRAG_KEYFRAME } else { 0 };
    buf.push(flags);
    buf.extend_from_slice(data);
    buf
}

/// Decode a video fragment from a media datagram payload.
pub fn decode_fragment(payload: &[u8]) -> Result<VideoFragment<'_>, &'static str> {
    if payload.len() < VIDEO_HEADER_LEN {
        return Err("video fragment too short");
    }
    let frag_count = u16::from_be_bytes([payload[6], payload[7]]);
    let frag_idx = u16::from_be_bytes([payload[4], payload[5]]);
    if frag_count == 0 {
        return Err("fragment count must be nonzero");
    }
    if frag_idx >= frag_count {
        return Err("fragment index out of range");
    }
    Ok(VideoFragment {
        frame_id: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
        frag_idx,
        frag_count,
        keyframe: payload[8] & FRAG_KEYFRAME != 0,
        data: &payload[VIDEO_HEADER_LEN..],
    })
}

/// Splits one encoded H.264 frame into fragments, each pre-wrapped with
/// the full media header (11 bytes) + video header (9), ready for
/// `conn.send_datagram`. `seq` starts at `first_seq` and increments per
/// fragment; `timestamp` is the frame's capture timestamp (same clock
/// as audio timestamps — sender's 48 kHz sample count at capture time).
pub fn fragment_frame(
    frame_id: u32,
    keyframe: bool,
    frame_data: &[u8],
    first_seq: u16,
    timestamp: u32,
    sender_id: u32,
) -> Vec<Vec<u8>> {
    let frag_count = frame_data.len().div_ceil(MAX_FRAGMENT_PAYLOAD).max(1);
    let frag_count = frag_count.min(u16::MAX as usize) as u16;
    let mut out = Vec::with_capacity(frag_count as usize);
    // chunks() on an empty slice yields NOTHING — special-case so an
    // empty frame still produces one fragment (codecs do emit such).
    let mut chunks: Vec<&[u8]> = frame_data.chunks(MAX_FRAGMENT_PAYLOAD).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    for (idx, chunk) in chunks.into_iter().take(frag_count as usize).enumerate() {
        let payload = encode_fragment(frame_id, idx as u16, frag_count, keyframe, chunk);
        out.push(crate::media::encode(
            crate::media::FLAG_VIDEO,
            first_seq.wrapping_add(idx as u16),
            timestamp,
            sender_id,
            &payload,
        ));
    }
    out
}

/// Reassembles video fragments into complete frames.
///
/// The classic drop-until-IDR rule: if any fragment of a frame is lost,
/// the frame (and all subsequent non-keyframes that depend on it) is
/// discarded. Only after a fresh keyframe arrives does decoding resume.
/// This mirrors what hardware decoders do with corrupted reference chains.
pub struct VideoAssembler {
    /// Fragments of the current in-progress frame, by index.
    current: std::collections::HashMap<u16, Vec<u8>>,
    current_frame_id: Option<u32>,
    current_frag_count: u16,
    current_is_keyframe: bool,
    /// Set when a frame was dropped due to loss; cleared on keyframe.
    waiting_for_keyframe: bool,
    /// Completed frames ready for the caller, in order.
    output: std::collections::VecDeque<AssembledFrame>,
    /// Frames fully reassembled (stat).
    pub frames_completed: u64,
    /// Frames dropped due to fragment loss (stat).
    pub frames_dropped: u64,
    /// Fragments dropped because we're waiting for a keyframe (stat).
    pub fragments_skipped: u64,
    /// 6e window counters for gap_rate_permille: frames displayed /
    /// frames known-missed since the last read.
    win_completed: u64,
    win_missed: u64,
    /// Frame ids already counted as missed in the current loss epoch —
    /// dedupe: one count per frame despite many skipped fragments.
    /// Cleared whenever a keyframe fragment begins a frame (new epoch).
    missed_ids: std::collections::HashSet<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledFrame {
    pub frame_id: u32,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

impl VideoAssembler {
    pub fn new() -> Self {
        Self {
            current: std::collections::HashMap::new(),
            current_frame_id: None,
            current_frag_count: 0,
            current_is_keyframe: false,
            waiting_for_keyframe: true,
            output: std::collections::VecDeque::new(),
            frames_completed: 0,
            frames_dropped: 0,
            fragments_skipped: 0,
            win_completed: 0,
            win_missed: 0,
            missed_ids: std::collections::HashSet::new(),
        }
    }

    /// Feed one decoded video fragment. Completed frames appear in
    /// `pop_frame()`.
    pub fn push_fragment(&mut self, frag: VideoFragment<'_>) {
        // Accept fragments for the current frame in ANY order — the
        // network reorders, and completion is set-based, not sequence-based.
        if self.waiting_for_keyframe {
            if frag.keyframe {
                // Any keyframe fragment can begin recovery: start the
                // frame fresh (earlier same-frame fragments were skipped
                // and are lost — the frame may still complete if all
                // remaining fragments arrive).
                self.waiting_for_keyframe = false;
                self.start_frame(&frag);
                self.store_fragment(frag);
            } else {
                self.fragments_skipped += 1;
                self.note_missed(frag.frame_id);
            }
            return;
        }

        match self.current_frame_id {
            Some(id) if id == frag.frame_id => self.store_fragment(frag),
            Some(old) => {
                // New frame started before the current completed: loss.
                self.frames_dropped += 1;
                if frag.keyframe {
                    self.note_missed(old);
                    self.start_frame(&frag);
                    self.store_fragment(frag);
                } else {
                    // Hygiene: drop the partial frame's fragments so
                    // nothing of it lingers (start_frame also clears,
                    // but only when a keyframe eventually arrives).
                    self.current.clear();
                    self.current_frame_id = None;
                    self.waiting_for_keyframe = true;
                    self.fragments_skipped += 1;
                    self.note_missed(old);
                    self.note_missed(frag.frame_id);
                }
            }
            None => {
                // Previous frame completed cleanly (or fresh start that
                // already passed the keyframe gate): any frame may begin,
                // delta or keyframe. Requiring a keyframe here would drop
                // every delta after every completed frame.
                self.start_frame(&frag);
                self.store_fragment(frag);
            }
        }
    }

    fn start_frame(&mut self, frag: &VideoFragment<'_>) {
        // A keyframe fragment begins a new loss epoch: the dedupe set
        // resets (its counts are already banked in win_missed).
        if frag.keyframe {
            self.missed_ids.clear();
        }
        self.current.clear();
        self.current_frame_id = Some(frag.frame_id);
        self.current_frag_count = frag.frag_count;
        self.current_is_keyframe = frag.keyframe;
    }

    /// Count a frame as never-displayable, deduped by frame_id within
    /// the current loss epoch.
    fn note_missed(&mut self, frame_id: u32) {
        if self.missed_ids.insert(frame_id) {
            self.win_missed += 1;
        }
    }

    fn store_fragment(&mut self, frag: VideoFragment<'_>) {
        self.current.insert(frag.frag_idx, frag.data.to_vec());
        if self.current.len() == self.current_frag_count as usize {
            // Frame complete: concatenate in order.
            let mut data = Vec::new();
            for idx in 0..self.current_frag_count {
                // Unwrap: len == frag_count guarantees every idx present.
                data.extend_from_slice(&self.current[&idx]);
            }
            let frame = AssembledFrame {
                frame_id: self.current_frame_id.unwrap(),
                keyframe: self.current_is_keyframe,
                data,
            };
            self.output.push_back(frame);
            self.frames_completed += 1;
            self.win_completed += 1;
            self.current.clear();
            self.current_frame_id = None;
        }
    }

    /// Pop one completed frame, if any.
    pub fn pop_frame(&mut self) -> Option<AssembledFrame> {
        self.output.pop_front()
    }

    /// 6.9 3b: completed frames currently awaiting the decode drain.
    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    /// 6.9 3b: index of the LAST keyframe in the completed-frame
    /// output, if any. Pre-decode skip gate: when the display consumer
    /// is behind, frames older than this keyframe are safe to discard
    /// undecoded — the keyframe rebases every reference they'd need.
    pub fn last_keyframe_output_index(&self) -> Option<usize> {
        self.output.iter().rposition(|f| f.keyframe)
    }

    /// 6.9 3b: count a frame discarded pre-decode into the gap window —
    /// a receiver too slow to decode IS loss signal for the sender's
    /// ladder (Robust = smaller frames + IDR re-pokes, both help).
    pub fn note_missed_frame(&mut self, frame_id: u32) {
        self.note_missed(frame_id);
    }

    /// 6e: video loss signal for the sender's bitrate ladder — the
    /// share of frames that could NOT be displayed over the window
    /// (aborted frames + skip cascades, deduped by frame_id), in
    /// permille. Resets the window on read. Frames whose fragments
    /// were ALL lost are invisible to the assembler — the metric
    /// undercounts true loss, never overcounts.
    pub fn gap_rate_permille(&mut self) -> u32 {
        let total = self.win_completed + self.win_missed;
        let rate = if total == 0 {
            0
        } else {
            ((self.win_missed as f64 / total as f64) * 1000.0).round() as u32
        };
        self.win_completed = 0;
        self.win_missed = 0;
        rate
    }
}

impl Default for VideoAssembler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragments(frame_id: u32, keyframe: bool, data: &[u8], first_seq: u16) -> Vec<Vec<u8>> {
        fragment_frame(frame_id, keyframe, data, first_seq, 0, 42)
    }

    fn decode_all(dgs: &[Vec<u8>]) -> Vec<VideoFragment<'_>> {
        dgs.iter()
            .map(|dg| {
                let m = crate::media::decode(dg).unwrap();
                assert!(m.flags & crate::media::FLAG_VIDEO != 0);
                decode_fragment(m.payload).unwrap()
            })
            .collect()
    }

    #[test]
    fn fragment_roundtrip() {
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let dgs = fragments(7, true, &data, 100);
        assert_eq!(dgs.len(), 3); // 3000 / 1179 = 2.55 → 3
        for (i, dg) in dgs.iter().enumerate() {
            let m = crate::media::decode(dg).unwrap();
            assert_eq!(m.seq, 100 + i as u16);
        }
        let frags = decode_all(&dgs);
        let mut asm = VideoAssembler::new();
        for f in frags {
            asm.push_fragment(f);
        }
        let frame = asm.pop_frame().unwrap();
        assert_eq!(frame.data, data);
        assert!(frame.keyframe);
        assert_eq!(frame.frame_id, 7);
        assert_eq!(asm.frames_completed, 1);
    }

    #[test]
    fn empty_frame_single_fragment() {
        let dgs = fragments(1, true, &[], 0);
        assert_eq!(dgs.len(), 1);
        let frags = decode_all(&dgs);
        let mut asm = VideoAssembler::new();
        asm.push_fragment(frags[0].clone());
        let frame = asm.pop_frame().unwrap();
        assert!(frame.data.is_empty());
        assert!(frame.keyframe);
    }

    #[test]
    fn fragment_loss_drops_frame_and_requires_keyframe() {
        let keyframe: Vec<u8> = vec![0xAA; 2000];
        let delta1: Vec<u8> = vec![0xBB; 1500];
        let delta2: Vec<u8> = vec![0xCC; 2500];

        let mut all_dgs = fragments(0, true, &keyframe, 0);
        all_dgs.extend(fragments(1, false, &delta1, 10));
        all_dgs.extend(fragments(2, false, &delta2, 20));

        // Decode, then drop fragment 1 of delta1 (frame_id 1).
        let mut frags = decode_all(&all_dgs);
        frags.retain(|f| !(f.frame_id == 1 && f.frag_idx == 1));

        let mut asm = VideoAssembler::new();
        for f in frags {
            asm.push_fragment(f);
        }

        // Keyframe completed; delta1 dropped; delta2 skipped (no keyframe).
        assert_eq!(asm.frames_completed, 1);
        assert_eq!(asm.frames_dropped, 1);
        assert!(asm.fragments_skipped >= 1);
        // Only the keyframe comes out.
        let frames = asm.pop_frame().unwrap();
        assert_eq!(frames.data, keyframe);
        assert!(asm.pop_frame().is_none());
    }

    #[test]
    fn recovery_after_keyframe() {
        let key1: Vec<u8> = vec![0x11; 1000];
        let delta: Vec<u8> = vec![0x22; 1000];
        let key2: Vec<u8> = vec![0x33; 1000];

        // key1, delta (loss), key2: assembler recovers on key2.
        let mut dgs = fragments(0, true, &key1, 0);
        dgs.extend(fragments(1, false, &delta, 5));
        dgs.extend(fragments(2, true, &key2, 10));

        let mut frags = decode_all(&dgs);
        // Lose both fragments of the delta entirely.
        frags.retain(|f| f.frame_id != 1);

        let mut asm = VideoAssembler::new();
        for f in frags {
            asm.push_fragment(f);
        }
        assert_eq!(asm.frames_completed, 2);
        let f1 = asm.pop_frame().unwrap();
        let f2 = asm.pop_frame().unwrap();
        assert_eq!(f1.data, key1);
        assert_eq!(f2.data, key2);
    }

    #[test]
    fn rejects_malformed() {
        assert!(decode_fragment(&[0u8; 5]).is_err()); // too short
        let mut f = encode_fragment(1, 2, 3, false, &[0xFF; 10]);
        f[6..8].copy_from_slice(&0u16.to_be_bytes()); // frag_count = 0
        assert!(decode_fragment(&f).is_err());
        let f = encode_fragment(1, 5, 3, false, &[0xFF; 10]); // idx >= count
        assert!(decode_fragment(&f).is_err());
    }

    #[test]
    fn reorder_within_frame_tolerated() {
        let data: Vec<u8> = vec![0x55; 2400];
        let dgs = fragments(3, true, &data, 0);
        let mut frags = decode_all(&dgs);
        // Swap fragments 0 and 1 (network reorder).
        frags.swap(0, 1);
        let mut asm = VideoAssembler::new();
        for f in frags {
            asm.push_fragment(f);
        }
        let frame = asm.pop_frame().unwrap();
        assert_eq!(frame.data, data);
    }

    #[test]
    fn assembler_stats_start_zero() {
        let asm = VideoAssembler::new();
        assert_eq!(asm.frames_completed, 0);
        assert_eq!(asm.frames_dropped, 0);
        assert_eq!(asm.fragments_skipped, 0);
    }

    #[test]
    fn partial_frame_cleared_on_abort() {
        // Frame 0 (keyframe) starts but never completes; a delta for
        // frame 1 arrives: the partial frame's fragments must be
        // discarded, not left lingering.
        let key_part: Vec<u8> = vec![0x77; 2 * MAX_FRAGMENT_PAYLOAD + 100]; // 3 fragments
        let dgs = fragments(0, true, &key_part, 0);
        let mut frags = decode_all(&dgs);
        frags.retain(|f| f.frag_idx != 2); // drop the last: frame 0 incomplete

        let delta: Vec<u8> = vec![0x88; 500];
        let delta_dgs = fragments(1, false, &delta, 10);
        frags.extend(decode_all(&delta_dgs));

        let mut asm = VideoAssembler::new();
        for f in frags {
            asm.push_fragment(f);
        }

        assert_eq!(asm.frames_dropped, 1);
        assert!(asm.waiting_for_keyframe);
        assert!(asm.current.is_empty());
        assert!(asm.current_frame_id.is_none());
        assert!(asm.pop_frame().is_none());
    }

    #[test]
    fn gap_rate_counts_abort_and_cascade() {
        // key0 completes; frame1 assembles 2 of 3 fragments then aborts
        // (frame2 arrives mid-assembly); frames 2-5 are skipped while
        // waiting; key6 recovers. Window: 2 completed, 5 missed → 714‰.
        let key: Vec<u8> = vec![0x11; 1000];
        let big: Vec<u8> = vec![0x22; 2400]; // 3 fragments
        let small: Vec<u8> = vec![0x33; 500]; // 1 fragment

        let mut dgs = fragments(0, true, &key, 0);
        let f1 = fragments(1, false, &big, 5);
        dgs.extend([f1[0].clone(), f1[1].clone()]); // frag 2 lost → incomplete
        dgs.extend(fragments(2, false, &small, 10)); // aborts frame 1
        dgs.extend(fragments(3, false, &small, 12));
        dgs.extend(fragments(4, false, &small, 14));
        dgs.extend(fragments(5, false, &small, 16));
        dgs.extend(fragments(6, true, &key, 20)); // recovery keyframe

        let frags = decode_all(&dgs);
        let mut asm = VideoAssembler::new();
        for f in frags {
            asm.push_fragment(f);
        }

        assert_eq!(asm.frames_completed, 2);
        assert_eq!(asm.gap_rate_permille(), 714, "5 missed of 7 total frames");
        assert_eq!(asm.gap_rate_permille(), 0, "window resets on read");
    }

    #[test]
    fn gap_rate_zero_on_clean_stream() {
        let key: Vec<u8> = vec![0x11; 1000];
        let delta: Vec<u8> = vec![0x22; 900];
        let mut dgs = fragments(0, true, &key, 0);
        dgs.extend(fragments(1, false, &delta, 5));
        dgs.extend(fragments(2, false, &delta, 10));

        let frags = decode_all(&dgs);
        let mut asm = VideoAssembler::new();
        for f in frags {
            asm.push_fragment(f);
        }
        assert_eq!(asm.gap_rate_permille(), 0);
    }

    #[test]
    fn keyframe_output_index_tracks_backlog() {
        let mut asm = VideoAssembler::new();
        assert_eq!(asm.output_len(), 0);
        assert_eq!(asm.last_keyframe_output_index(), None);
        for f in decode_all(&fragments(0, true, &[0x11; 1000], 0)) {
            asm.push_fragment(f);
        }
        for f in decode_all(&fragments(1, false, &[0x22; 900], 5)) {
            asm.push_fragment(f);
        }
        for f in decode_all(&fragments(2, true, &[0x33; 900], 10)) {
            asm.push_fragment(f);
        }
        assert_eq!(asm.output_len(), 3);
        assert_eq!(asm.last_keyframe_output_index(), Some(2));
        asm.note_missed_frame(1);
        // 3 completed + 1 externally noted missed → 250 permille.
        assert_eq!(asm.gap_rate_permille(), 250);
    }
}
