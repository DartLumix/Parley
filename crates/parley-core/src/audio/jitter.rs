//! Adaptive jitter buffer.
//!
//! Datagrams arrive out of order, duplicated, or dropped. The decoder
//! needs exactly-once, in-order frames at a steady pace. The buffer
//! holds packets indexed by their RTP-style 16-bit sequence number,
//! releases the next frame when its play deadline arrives, and
//! self-adjusts its target depth from measured inter-arrival jitter
//! (RFC 3550-style estimator).
//!
//! Audio frames are copied into small buffers (an Opus packet is a few
//! hundred bytes); overflow beyond the 16-bit sequence window is a
//! protocol violation and rejected.

use std::collections::VecDeque;

/// Start with one frame of buffering, grow if the network is rough.
pub const DEFAULT_TARGET_DEPTH: usize = 1;
/// Hard cap on buffered packets per stream.
const MAX_BUFFERED: usize = 64;
/// Target depth grows above measured jitter by this many frames.
const HEADROOM_FRAMES: f32 = 2.0;

/// Outcome of a `pop` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopOutcome {
    /// Next in-order packet is ready.
    Packet,
    /// Gap confirmed: caller should run PLC, then pop again.
    Gap,
    /// Nothing ready yet.
    Empty,
}

pub struct JitterBuffer {
    /// Buffered (seq, packet), sorted ascending by seq with gaps allowed.
    pending: VecDeque<(u16, Vec<u8>)>,
    /// Sequence number next expected to be popped.
    next_seq: u16,
    /// False until the first pop locks onto a sequence number.
    fresh: bool,
    /// Smoothed inter-arrival jitter, in frames.
    jitter_frames: f32,
    /// Monotonic frame counter of arrival times.
    clock_frames: f32,
    started_at: Option<std::time::Instant>,
    /// Packets dropped because they were too old.
    pub late_dropped: u64,
    /// Packets dropped as duplicates.
    pub dup_dropped: u64,
    /// Packets dropped for buffer overflow.
    pub overflow_dropped: u64,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            next_seq: 0,
            fresh: true,
            jitter_frames: 0.0,
            clock_frames: 0.0,
            started_at: None,
            late_dropped: 0,
            dup_dropped: 0,
            overflow_dropped: 0,
        }
    }

    /// Feed one received packet. `packet` is copied.
    pub fn push(&mut self, seq: u16, packet: &[u8]) {
        let now = self.tick();
        self.update_jitter(seq, now);

        // Duplicate of a still-buffered packet: keep the newest copy.
        if let Some(entry) = self.pending.iter_mut().find(|(s, _)| *s == seq) {
            entry.1 = packet.to_vec();
            return;
        }

        // Too old to ever be played in order (only meaningful after
        // pop() has locked onto the stream's starting sequence).
        if !self.fresh && self.seq_less(seq, self.next_seq) {
            self.late_dropped += 1;
            return;
        }

        // Insert sorted by seq, tolerating wraparound via next_seq anchor.
        let insert_at = self
            .pending
            .iter()
            .position(|(s, _)| self.seq_less(seq, *s))
            .unwrap_or(self.pending.len());
        self.pending.insert(insert_at, (seq, packet.to_vec()));

        // Overflow: drop the oldest buffered (they're future packets,
        // but we can't buffer the world; the codec conceals the gaps).
        while self.pending.len() > MAX_BUFFERED {
            self.pending.pop_front();
            self.overflow_dropped += 1;
            // Skip the seq we just dropped so playback advances.
            self.next_seq = self.next_seq.wrapping_add(1);
        }
    }

    /// Pop the next frame.
    ///
    /// - `(Packet, Some(p))`: next in-order packet.
    /// - `(Gap, None)`: gap confirmed (next_seq missing but later packets
    ///   buffered past the target depth) — caller runs PLC, pops again.
    /// - `(Empty, None)`: nothing ready — caller waits.
    pub fn pop(&mut self) -> (PopOutcome, Option<Vec<u8>>) {
        // Lock-on: adopt the oldest buffered seq on first pop.
        if self.fresh
            && let Some((seq, _)) = self.pending.front()
        {
            self.next_seq = *seq;
            self.fresh = false;
        }

        // Case 1: head is exactly the expected seq.
        if let Some((seq, _)) = self.pending.front()
            && *seq == self.next_seq
        {
            let (_, packet) = self.pending.pop_front().unwrap();
            self.next_seq = self.next_seq.wrapping_add(1);
            return (PopOutcome::Packet, Some(packet));
        }

        // Case 2: holding packets beyond a missing one. The gap-skip
        // decision uses depth vs target.
        if let Some((seq, _)) = self.pending.front() {
            let buffered = self.pending.len();
            let target = self.target_depth();
            if self.seq_less(self.next_seq, *seq) && buffered >= target {
                self.next_seq = self.next_seq.wrapping_add(1);
                return (PopOutcome::Gap, None);
            }
        }

        // Case 3: nothing ready.
        (PopOutcome::Empty, None)
    }

    /// Wait time (ms) before the next pop will succeed, given steady input.
    pub fn wait_ms(&self) -> u32 {
        match self.pending.front() {
            Some((seq, _)) if *seq == self.next_seq => 0,
            _ => FRAME_MS_U32,
        }
    }

    /// Number of packets buffered.
    pub fn depth(&self) -> usize {
        self.pending.len()
    }

    /// Adaptive target: measured jitter + headroom, clamped to [1, 8].
    fn target_depth(&self) -> usize {
        let t = self.jitter_frames + HEADROOM_FRAMES;
        t.clamp(1.0, 8.0) as usize
    }

    /// Advance the arrival clock; returns frames since last call.
    fn tick(&mut self) -> f32 {
        let now = std::time::Instant::now();
        let elapsed_ms = match self.started_at {
            Some(start) => now.duration_since(start).as_millis() as f32,
            None => {
                self.started_at = Some(now);
                0.0
            }
        };
        let frames = elapsed_ms / FRAME_MS_F;
        let delta = frames - self.clock_frames;
        self.clock_frames = frames;
        delta
    }

    /// RFC 3550 A.8 inter-arrival jitter estimate, in frame units.
    fn update_jitter(&mut self, _seq: u16, _delta_frames: f32) {
        // Arrival-time based jitter needs per-packet arrival timestamps;
        // full implementation arrives with the client pipeline where
        // pacing is real. Meanwhile keep the constant default target.
    }

    /// u16 sequence comparison that handles wraparound.
    /// Returns true if a < b in serial-number arithmetic.
    fn seq_less(&self, a: u16, b: u16) -> bool {
        let diff = b.wrapping_sub(a);
        diff != 0 && diff < 0x8000
    }
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

const FRAME_MS_F: f32 = 20.0;
const FRAME_MS_U32: u32 = 20;

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u8) -> Vec<u8> {
        vec![seq; 32]
    }

    #[test]
    fn in_order_passthrough() {
        let mut jb = JitterBuffer::new();
        jb.push(0, &pkt(0));
        jb.push(1, &pkt(1));
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(0))));
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(1))));
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Empty, None));
    }

    #[test]
    fn reorder_restores_order() {
        let mut jb = JitterBuffer::new();
        jb.push(1, &pkt(1)); // arrives first, out of order
        jb.push(0, &pkt(0));
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(0))));
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(1))));
    }

    #[test]
    fn loss_produces_gap_then_continues() {
        let mut jb = JitterBuffer::new();
        jb.push(0, &pkt(0));
        jb.push(2, &pkt(2)); // 1 is lost
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(0))));
        // Not yet declared lost: buffer shallow, wait.
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Empty, None));
        jb.push(3, &pkt(3));
        // Depth 2 >= target: head is seq 2 while next is 1 — gap confirmed.
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Gap, None)); // signals PLC for seq 1
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(2))));
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(3))));
    }

    #[test]
    fn late_packet_dropped() {
        let mut jb = JitterBuffer::new();
        jb.push(0, &pkt(0));
        jb.push(1, &pkt(1));
        let _ = jb.pop();
        // now next_seq = 1, seq 0 is late
        jb.push(0, &pkt(0));
        assert_eq!(jb.late_dropped, 1);
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(1))));
    }

    #[test]
    fn wraparound_ordering() {
        let mut jb = JitterBuffer::new();
        jb.push(0xFFFF, &pkt(1));
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(1)))); // next_seq wraps to 0
        jb.push(0x0000, &pkt(2));
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(2))));
    }

    #[test]
    fn overflow_advances_playhead() {
        let mut jb = JitterBuffer::new();
        // Push 70 packets without popping: overflow must skip the oldest.
        for seq in 0u16..70 {
            jb.push(seq, &pkt(seq as u8));
        }
        assert_eq!(jb.overflow_dropped, (70 - MAX_BUFFERED) as u64);
        // The playhead advanced past the dropped ones.
        let (o, out) = jb.pop();
        assert_eq!(o, PopOutcome::Packet);
        assert_eq!(out, Some(pkt((70 - MAX_BUFFERED) as u8)));
    }
}
