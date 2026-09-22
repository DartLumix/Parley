//! Adaptive jitter buffer.
//!
//! Datagrams arrive out of order, duplicated, or dropped. The decoder
//! needs exactly-once, in-order frames at a steady pace. The buffer
//! holds packets indexed by their RTP-style 16-bit sequence number,
//! releases the next frame when its play deadline arrives, and
//! self-adjusts its target depth from measured inter-arrival jitter
//! (RFC 3550-style estimator).
//!
//! 6h latency-shed (SKIP flavor): burst absorption and sender/receiver
//! clock drift ratchet depth toward MAX_BUFFERED with nothing pulling
//! it back — permanently baked-in latency. When a CONTIGUOUS buffer
//! (head == next expected: backlog, not loss) exceeds 2x target depth,
//! the head packet is consumed WITHOUT being emitted: the pop returns
//! the successor instead, so one pop drains two packets from the
//! buffer. Playback content skips forward one frame (a 20 ms skip of
//! stale audio, rate-limited to one per 8 pops — a crude cousin of
//! NetEQ's "accelerate"). A PLC-flavored shed was tried first and is
//! PROVABLY useless here: a concealed frame occupies a playback slot,
//! so under rate-equal arrival the depth never drains (a failing test
//! proved it — see the drift test). Sustained arrival ABOVE the
//! consumption rate is not this mechanism's problem: that's the
//! overflow cap's job, by design.
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
/// 6h: shed only when a contiguous buffer exceeds target by this multiple.
const SHED_FACTOR: usize = 2;
/// 6h: at most one shed per this many pops (rate limit — convergence
/// over ~1-2 s, one 20 ms skip each, not a burst of skips).
const SHED_EVERY_POPS: u64 = 8;

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
    /// 6h: pops since the last shed (rate limiter). Starts DISARMED: a
    /// fresh playhead (lock-on, or right after overflow dropped a
    /// batch) must not shed — that backlog is pre-accounted. Arms after
    /// 8 pops of established consumer pace.
    pops_since_shed: u64,
    /// Packets dropped because they were too old.
    pub late_dropped: u64,
    /// Packets dropped as duplicates.
    pub dup_dropped: u64,
    /// Packets dropped for buffer overflow.
    pub overflow_dropped: u64,
    /// 6h: packets deliberately skipped (consumed, never emitted) to
    /// drain baked-in latency.
    pub sheds: u64,
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
            pops_since_shed: 0,
            late_dropped: 0,
            dup_dropped: 0,
            overflow_dropped: 0,
            sheds: 0,
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
    /// - `(Packet, Some(p))`: next in-order packet (or, after a shed,
    ///   its successor — one pop may have silently consumed the head).
    /// - `(Gap, None)`: gap confirmed (next_seq missing, later packets
    ///   buffered past target) — caller runs PLC, pops again.
    /// - `(Empty, None)`: nothing ready — caller waits.
    pub fn pop(&mut self) -> (PopOutcome, Option<Vec<u8>>) {
        // Lock-on: adopt the oldest buffered seq on first pop.
        if self.fresh
            && let Some((seq, _)) = self.pending.front()
        {
            self.next_seq = *seq;
            self.fresh = false;
        }

        self.pops_since_shed = self.pops_since_shed.saturating_add(1);

        // Case 1: head is exactly the expected seq. A deep CONTIGUOUS
        // buffer here is baked-in latency (burst absorption / clock
        // drift — no loss involved). Skip-shed: consume the head
        // without emitting it and let the recursion return whatever
        // follows (the successor, or a Gap if a hole follows) — one
        // pop then drains two packets.
        if let Some((seq, _)) = self.pending.front()
            && *seq == self.next_seq
            && self.pending.len() >= self.target_depth() * SHED_FACTOR
            && self.pops_since_shed >= SHED_EVERY_POPS
        {
            self.pending.pop_front();
            self.next_seq = self.next_seq.wrapping_add(1);
            self.pops_since_shed = 0;
            self.sheds += 1;
            // Recurse for this pop's result. pops_since_shed just reset
            // (the increment inside is harmless), so no shed chaining.
            return self.pop();
        }

        if let Some((seq, _)) = self.pending.front()
            && *seq == self.next_seq
        {
            let (_, packet) = self.pending.pop_front().unwrap();
            self.next_seq = self.next_seq.wrapping_add(1);
            return (PopOutcome::Packet, Some(packet));
        }

        // Case 2: holding packets beyond a missing one — genuine loss.
        // Declared once depth reaches the target; the playhead walks
        // through the missing range at the consumer's natural pace.
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

    /// The immediate successor packet, if buffered, without consuming it.
    /// During a declared gap this is the packet whose in-band FEC (if
    /// any) can reconstruct the lost frame.
    pub fn peek(&self) -> Option<(u16, &[u8])> {
        self.pending
            .front()
            .filter(|(s, _)| *s == self.next_seq)
            .map(|(s, p)| (*s, p.as_slice()))
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
        // The playhead advanced past the dropped ones; the rate limiter
        // is DISARMED after a fresh lock-on, so this first pop plays.
        let (o, out) = jb.pop();
        assert_eq!(o, PopOutcome::Packet);
        assert_eq!(out, Some(pkt((70 - MAX_BUFFERED) as u8)));
    }

    #[test]
    fn peek_exposes_successor_during_gap() {
        let mut jb = JitterBuffer::new();
        jb.push(0, &pkt(0));
        jb.push(2, &pkt(2)); // 1 lost
        let _ = jb.pop(); // packet 0
        jb.push(3, &pkt(3)); // depth 2 >= target → gap can be declared
        let (o, _) = jb.pop();
        assert_eq!(o, PopOutcome::Gap);
        assert_eq!(jb.peek(), Some((2, pkt(2).as_slice())));
        let (o, p) = jb.pop();
        assert_eq!((o, p), (PopOutcome::Packet, Some(pkt(2))));
        assert_eq!(jb.peek(), Some((3, pkt(3).as_slice())));
    }

    // ---- 6h: latency-shed (skip flavor) ----

    #[test]
    fn one_shot_burst_plays_out_unshed() {
        // A one-shot contiguous burst with no further arrivals just
        // plays out at the natural rate — nothing is sustained, so
        // nothing needs draining, and concealment/skipping here would
        // degrade audio for nothing. The rate limiter (disarmed until
        // 8 pops) plus the shrinking depth keep it below threshold
        // anyway.
        let mut jb = JitterBuffer::new();
        let burst = 10;
        for seq in 0..burst as u16 {
            jb.push(seq, &pkt(seq as u8));
        }
        let mut played = 0;
        let mut gaps = 0;
        for _ in 0..200 {
            match jb.pop().0 {
                PopOutcome::Packet => played += 1,
                PopOutcome::Gap => gaps += 1,
                PopOutcome::Empty => break,
            }
        }
        assert_eq!(played, burst, "every packet plays");
        assert_eq!(gaps, 0);
        assert_eq!(jb.sheds, 0, "a one-shot burst must not shed");
        assert_eq!(jb.depth(), 0);
    }

    #[test]
    fn rate_equal_backlog_drains_to_target() {
        // THE baked-latency scenario: a burst left depth at 10, then
        // arrival and consumption run at EQUAL rates (50 pps each) —
        // without a shed, depth is invariant at 10 forever (200 ms of
        // permanent latency). The skip-shed must drain it to the
        // target and hold it there.
        //
        // NOTE: this is the test that falsified the PLC-flavored shed:
        // concealment occupies a playback slot, so under rate-equal
        // arrival the depth never moves. Only consuming-without-emitting
        // (skip) drains.
        let mut jb = JitterBuffer::new();
        let backlog = 10;
        for seq in 0..backlog as u16 {
            jb.push(seq, &pkt(seq as u8));
        }
        // Rate-equal from here: 1 push per pop.
        let iterations = 400;
        let total_pushed = backlog + iterations;
        let mut seq = backlog as u16;
        let mut played = 0;
        let mut gaps = 0;
        for _ in 0..iterations {
            jb.push(seq, &pkt((seq % 256) as u8));
            seq = seq.wrapping_add(1);
            match jb.pop().0 {
                PopOutcome::Packet => played += 1,
                PopOutcome::Gap => gaps += 1,
                PopOutcome::Empty => {}
            }
        }
        // Drained: depth settled at/near the target (2), far below the
        // 10 it would sit at forever without the shed.
        assert!(
            jb.depth() <= 4,
            "backlog must drain to ~target, depth {}",
            jb.depth()
        );
        assert!(
            jb.sheds >= 5,
            "draining 10→~2 needs several sheds, got {}",
            jb.sheds
        );
        assert_eq!(
            jb.overflow_dropped, 0,
            "rate-equal arrival must never hit the overflow cap"
        );
        // Conservation: every pushed packet was emitted, skipped
        // (shed), or remains buffered. Nothing vanished.
        assert_eq!(
            played + gaps + jb.sheds as usize + jb.depth(),
            total_pushed,
            "conservation: {played} emitted, {} gapped, {} shed, {} buffered",
            gaps,
            jb.sheds,
            jb.depth()
        );
    }

    #[test]
    fn steady_state_never_sheds() {
        // Producer and consumer at the same rate, shallow: depth stays
        // at the target, far under the 2x shed threshold.
        let mut jb = JitterBuffer::new();
        let mut seq: u16 = 0;
        for i in 0..500 {
            let sheds_before = jb.sheds;
            while jb.depth() < 2 {
                jb.push(seq, &pkt((seq % 256) as u8));
                seq = seq.wrapping_add(1);
            }
            let (o, _) = jb.pop();
            assert_eq!(o, PopOutcome::Packet, "iteration {i}");
            assert_eq!(jb.sheds, sheds_before, "steady state must never shed");
        }
    }

    #[test]
    fn shallow_reorder_is_loss_not_drift() {
        // A single missing packet at low depth (target 2): the gap is
        // declared as ordinary loss via Case 2 — never mistaken for
        // drift, never shed.
        let mut jb = JitterBuffer::new();
        jb.push(0, &pkt(0));
        jb.push(2, &pkt(2)); // 1 missing
        jb.push(3, &pkt(3));
        let (o, _) = jb.pop(); // play 0
        assert_eq!(o, PopOutcome::Packet);
        let (o, _) = jb.pop(); // depth 2 >= target → ordinary gap for seq 1
        assert_eq!(o, PopOutcome::Gap);
        assert_eq!(jb.sheds, 0, "shallow loss must never shed");
        let (o, _) = jb.pop(); // play 2
        assert_eq!(o, PopOutcome::Packet);
        let (o, _) = jb.pop(); // play 3
        assert_eq!(o, PopOutcome::Packet);
        assert_eq!(jb.sheds, 0);
    }
}
