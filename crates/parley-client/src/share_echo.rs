//! Digital echo guard for the share-audio capture.
//!
//! The share-audio tap is the DEFAULT OUTPUT's monitor (native PipeWire
//! `stream.capture.sink`): every client playing on this machine, which
//! includes THIS app — the voices we render and the other shares we
//! play. Sent back out as "system audio" that is a feedback loop within
//! one round trip (observed in the 7j run: two machines, both sharing,
//! howling at each other). The symptom that survived the first guard is
//! subtler: a viewer hears their own voice in the shared stream, because
//! the sharer's monitor captures the sharer's playback of it.
//!
//! PipeWire's monitor has no per-client exclusion, so the exclusion is
//! done by subtraction: the reference is the exact mono mix the output
//! callback rendered, and a normalised-LMS FIR removes it from the
//! capture. The reference is DIGITAL (no acoustic path), so the echo is
//! a short linear filter — delay, gain, the sink's own resampling — and
//! the canceller can drive it far below audibility. Only what we played
//! is correlated with the reference; other applications' audio passes
//! through untouched.
//!
//! The first version of this guard used WebRTC's AEC3 (the mic path's
//! canceller). On a digital loop it plateaus around 30 dB: its
//! suppressor is tuned for acoustic echo in the presence of near-end
//! speech, and a 30 dB copy of a voice is still audible on headphones.
//! A plain adaptive FIR on a known reference has no such ceiling, so the
//! share path uses its own canceller. The mic path keeps AEC3.
//!
//! Threading: the render callback appends the played mono mix to a ring
//! (one short mutex append); a dedicated thread turns captured stereo
//! chunks and that reference into cleaned chunks for the mux. Neither
//! side blocks the real-time callback, and the caller keeps its bounded
//! channel + drop-on-full discipline.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{LazyLock, Mutex};

/// True while a share-audio filter owns the reference ring. The render
/// callback checks this before copying anything — the hot path when
/// nobody is sharing is a single atomic load.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mono far-end reference: what the output callback has just played.
/// Newest last. Only filled while ACTIVE.
static REF: LazyLock<Mutex<VecDeque<f32>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// Two seconds of reference: the delay search needs 500 ms plus its
/// correlation window, with room for a sink that buffers generously.
const REF_CAP: usize = 96_000;

/// Echo-path correction around the estimated delay. 128 taps at 48 kHz is
/// 2.7 ms — enough for a fractional-sample delay, the sink's resampler,
/// and the small drift between the two callbacks, without giving the
/// filter so much freedom that uncorrelated audio bleeds into it.
const TAPS: usize = 128;

/// Correlation windows. The long one (85 ms) settles a known lock; the
/// short one (10.7 ms) finds a lock at all. The lag search is done at
/// the sample: a coarse (say 16-sample) scan can step straight over a
/// white-noise correlation peak, which is one sample wide — measured: it
/// found every integer delay but missed a 60.7 ms one completely.
const EST_WINDOW: usize = 4_096;
const INIT_WINDOW: usize = 512;
const MAX_LAG: usize = 24_000;

/// How often the lock is refreshed, and how far it may wander: a sink
/// switch or a clock drift moves the delay; a spurious correlation must
/// not.
const EST_EVERY: usize = 4_800; // 100 ms
const FINE_RANGE: i64 = 64; // per-estimate drift window
const WIDE_RANGE: i64 = 4_800; // re-acquire after a sink switch
const WIDE_EVERY: u64 = 10; // …at most once a second
const MIN_SCORE: f32 = 0.40;

/// Normalised-LMS step schedule, in adapted samples at 48 kHz. A large
/// first step converges the echo path in a few hundred milliseconds;
/// the step then decays so the filter's gradient noise (near-end music,
/// another voice) stops moving the weights. A fixed small step cannot
/// do both: it is either slow to converge or leaves an audible floor.
const STEPS: [(u64, f32); 4] = [
    (0, 0.05),          // first 0.5 s: converge
    (24_000, 0.01),     // 0.5–1.5 s: settle
    (72_000, 0.002),    // 1.5–3 s: refine
    (144_000, 0.000_2), // after 3 s: hold, barely move
];

/// Whether a share-audio filter is running (the render hook's guard).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Render-callback hook: record the played mono mix as the far-end
/// reference. Cheap and non-blocking-in-practice: the lock is held only
/// to append, and the filter drains it on its own thread.
pub fn push_render(mono: &[f32]) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut buf = REF.lock().unwrap();
    buf.extend(mono.iter().copied());
    if buf.len() > REF_CAP {
        let excess = buf.len() - REF_CAP;
        buf.drain(..excess);
    }
}

/// Filter the captured system audio, forwarding the result to the mux.
///
/// `raw` carries interleaved stereo from the PipeWire monitor (the
/// producer's sender drops when the share ends → this thread exits);
/// `out` feeds `spawn_share_audio_mux`. The guard arms immediately: the
/// reference has to be fresh from the first captured sample, and a
/// capture that starts before any playback simply passes through.
pub fn spawn_filter(raw: Receiver<Vec<f32>>, out: SyncSender<Vec<f32>>) {
    std::thread::spawn(move || {
        // Own the reference only for this share; the render hook is a
        // no-op again the moment the flag drops.
        REF.lock().unwrap().clear();
        ACTIVE.store(true, Ordering::Relaxed);
        println!(
            "share-audio: echo guard on — this app's own playback (voice + other shares) is \
             subtracted from the capture"
        );

        let mut guard = Guard::new();
        while let Ok(chunk) = raw.recv() {
            let pending: Vec<f32> = {
                let mut buf = REF.lock().unwrap();
                if buf.is_empty() {
                    Vec::new()
                } else {
                    buf.drain(..).collect()
                }
            };
            guard.push_reference(&pending);
            let filtered = guard.process(&chunk);
            if filtered.is_empty() {
                continue;
            }
            match out.try_send(filtered) {
                Ok(()) => {}
                // Mux behind: drop this buffer (live content — the
                // caller's discipline for the same case).
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }
        }

        ACTIVE.store(false, Ordering::Relaxed);
        REF.lock().unwrap().clear();
        println!("share-audio: echo guard off");
    });
}

/// The subtractor's state, on the filter thread.
struct Guard {
    /// Reference history, newest last.
    refs: VecDeque<f32>,
    /// Absolute count of reference samples ever pushed (the deque holds
    /// the most recent `refs.len()` of them).
    total: u64,
    /// Mono downmix of the capture, for the delay estimator.
    caps: VecDeque<f32>,
    /// Absolute count of capture frames processed.
    cap_total: u64,
    /// Capture index → reference index offset: the echo of capture
    /// sample `c` sits at reference sample `c + k`.
    k: Option<i64>,
    wl: [f32; TAPS],
    wr: [f32; TAPS],
    last_estimate: u64,
    /// Filtered samples since the lock last moved; selects the step.
    adapted: u64,
    /// Estimates taken since the lock, for the wide re-acquire cadence.
    estimates: u64,
}

impl Guard {
    fn new() -> Self {
        Self {
            refs: VecDeque::new(),
            total: 0,
            caps: VecDeque::new(),
            cap_total: 0,
            k: None,
            wl: [0.0; TAPS],
            wr: [0.0; TAPS],
            last_estimate: 0,
            adapted: 0,
            estimates: 0,
        }
    }

    /// A silent reference means there is no echo to cancel; the full lag
    /// scan is not free, so do not run it over silence.
    fn reference_has_signal(&self) -> bool {
        let n = self.refs.len().min(1_024);
        let energy: f32 = self.refs.iter().rev().take(n).map(|s| s * s).sum();
        energy > 1e-4 * n as f32
    }

    fn step_size(&self) -> f32 {
        STEPS
            .iter()
            .rev()
            .find(|(at, _)| self.adapted >= *at)
            .map_or(STEPS[0].1, |(_, mu)| *mu)
    }

    fn push_reference(&mut self, samples: &[f32]) {
        self.refs.extend(samples.iter().copied());
        self.total += samples.len() as u64;
        if self.refs.len() > REF_CAP {
            let excess = self.refs.len() - REF_CAP;
            self.refs.drain(..excess);
        }
    }

    fn process(&mut self, chunk: &[f32]) -> Vec<f32> {
        let frames = chunk.len() / 2;
        if frames == 0 {
            return Vec::new();
        }
        for pair in chunk.as_chunks::<2>().0 {
            self.caps.push_back((pair[0] + pair[1]) * 0.5);
        }
        let keep = EST_WINDOW + 8_192;
        if self.caps.len() > keep {
            let excess = self.caps.len() - keep;
            self.caps.drain(..excess);
        }

        let cap_end = self.cap_total + frames as u64;
        self.maybe_estimate(cap_end);

        let mut out = Vec::with_capacity(chunk.len());
        let k = self.k;
        for i in 0..frames {
            let (l, r) = (chunk[2 * i], chunk[2 * i + 1]);
            let filtered = k.and_then(|k| {
                let r_abs = (self.cap_total + i as u64) as i64 + k;
                self.taps_in_ring(r_abs).then(|| self.cancel(r_abs, l, r))
            });
            match filtered {
                Some((l, r)) => {
                    out.push(l);
                    out.push(r);
                }
                None => {
                    out.push(l);
                    out.push(r);
                }
            }
        }
        self.cap_total = cap_end;
        out
    }

    /// The tap window around `r_abs` must be entirely in the ring — a
    /// half-window would cancel the wrong samples.
    fn taps_in_ring(&self, r_abs: i64) -> bool {
        let start = r_abs - (TAPS as i64) / 2;
        let end = start + TAPS as i64;
        let ring_start = self.total as i64 - self.refs.len() as i64;
        start >= ring_start && end <= self.total as i64
    }

    fn cancel(&mut self, r_abs: i64, cap_l: f32, cap_r: f32) -> (f32, f32) {
        let ring_start = self.total as i64 - self.refs.len() as i64;
        let start = r_abs - (TAPS as i64) / 2;
        let mut x = [0.0f32; TAPS];
        let mut power = 0.0f32;
        for (i, slot) in x.iter_mut().enumerate() {
            let v = self.refs[(start + i as i64 - ring_start) as usize];
            *slot = v;
            power += v * v;
        }

        let mut yl = 0.0f32;
        let mut yr = 0.0f32;
        for ((wl, wr), v) in self.wl.iter().zip(&self.wr).zip(&x) {
            yl += wl * v;
            yr += wr * v;
        }
        let el = cap_l - yl;
        let er = cap_r - yr;

        // Adapt only while there is something to learn from: silence
        // would just add numerical noise. The final step is small enough
        // that near-end music cannot wander the weights audibly.
        if power > 1e-7 {
            self.adapted += 1;
            // The clamp matters only for a nearly silent reference, where
            // the normalisation would otherwise produce a huge step.
            let mu = (self.step_size() / (power + 1e-6)).min(0.5);
            for ((wl, wr), v) in self.wl.iter_mut().zip(self.wr.iter_mut()).zip(&x) {
                *wl += mu * el * v;
                *wr += mu * er * v;
            }
        }
        (el, er)
    }

    fn maybe_estimate(&mut self, cap_end: u64) {
        if self.caps.len() < EST_WINDOW || !self.reference_has_signal() {
            return;
        }
        if self.k.is_some() && cap_end.saturating_sub(self.last_estimate) < EST_EVERY as u64 {
            return;
        }
        self.last_estimate = cap_end;
        self.estimates = self.estimates.wrapping_add(1);

        match self.k {
            Some(k) => {
                if let Some((k2, score)) = self.search_around(cap_end, k)
                    && score >= MIN_SCORE
                {
                    // A sink switch moves the path by more than the tap
                    // window; start the step schedule again so the new
                    // delay converges quickly.
                    if (k2 - k).abs() > 480 {
                        self.adapted = 0;
                    }
                    self.k = Some(k2);
                }
            }
            None => {
                let Some((lag, score)) = self.search_initial(cap_end) else {
                    return;
                };
                if score < MIN_SCORE {
                    return;
                }
                self.k = Some(self.total as i64 - lag - cap_end as i64);
                println!(
                    "share-audio: echo path locked — {:.1} ms behind the reference \
                     (correlation {:.2})",
                    lag as f64 / 48.0,
                    score
                );
            }
        }
    }

    /// First lock: the echo sits somewhere in the last 500 ms of what we
    /// played. Search coarsely, then pin the winner to the sample.
    fn search_initial(&self, cap_end: u64) -> Option<(i64, f32)> {
        let mut best: Option<(i64, f32)> = None;
        for lag in 0..=MAX_LAG as i64 {
            if let Some(score) = self.corr(cap_end, self.total as i64 - lag, INIT_WINDOW, 1)
                && score > best.map_or(0.0, |b| b.1)
            {
                best = Some((lag, score));
            }
        }
        let (coarse, _) = best?;
        let mut refined = best;
        for lag in (coarse - 16)..=(coarse + 16) {
            if let Some(score) = self.corr(cap_end, self.total as i64 - lag, EST_WINDOW, 1)
                && score > refined.map_or(0.0, |b| b.1)
            {
                refined = Some((lag, score));
            }
        }
        refined
    }

    /// Refresh a known lock: the pipeline delay can move (sink switch,
    /// clock drift), but only in a window around the last estimate.
    fn search_around(&self, cap_end: u64, k: i64) -> Option<(i64, f32)> {
        let base = cap_end as i64;
        let mut best: Option<(i64, f32)> = None;
        for cand in (k - FINE_RANGE)..=(k + FINE_RANGE) {
            if let Some(score) = self.corr(cap_end, base + cand, EST_WINDOW, 1)
                && score > best.map_or(0.0, |b| b.1)
            {
                best = Some((cand, score));
            }
        }
        if best.is_some_and(|b| b.1 >= MIN_SCORE) || !self.wide_due() {
            return best;
        }

        // A sink switch can move the path by hundreds of milliseconds;
        // re-acquire with the short window (cheap enough at one sample
        // resolution) and settle the winner with the long one.
        let mut wide: Option<(i64, f32)> = best;
        for cand in (k - WIDE_RANGE)..=(k + WIDE_RANGE) {
            if let Some(score) = self.corr(cap_end, base + cand, INIT_WINDOW, 1)
                && score > wide.map_or(0.0, |b| b.1)
            {
                wide = Some((cand, score));
            }
        }
        let (coarse, _) = wide?;
        let mut refined = wide;
        for cand in (coarse - 16)..=(coarse + 16) {
            if let Some(score) = self.corr(cap_end, base + cand, EST_WINDOW, 1)
                && score > refined.map_or(0.0, |b| b.1)
            {
                refined = Some((cand, score));
            }
        }
        refined
    }

    fn wide_due(&self) -> bool {
        self.estimates.is_multiple_of(WIDE_EVERY)
    }

    /// Normalised correlation between the newest capture window and the
    /// reference window ending at `ref_end`, sampled every `step`.
    ///
    /// Both windows are walked backwards from their ends with the same
    /// stride, so the decimation phase matches even when `ref_end` is not
    /// aligned to `step`.
    fn corr(&self, cap_end: u64, ref_end: i64, window: usize, step: usize) -> Option<f32> {
        let cap_start = cap_end.checked_sub(self.caps.len() as u64)?;
        let ref_start = self.total as i64 - self.refs.len() as i64;
        let n = window / step;
        let mut dot = 0.0f32;
        let mut ec = 0.0f32;
        let mut er = 0.0f32;
        for j in 0..n {
            let back = (j * step) as u64;
            let c = cap_end.checked_sub(1 + back)?;
            let r = ref_end - 1 - back as i64;
            if c < cap_start || r < ref_start || r >= self.total as i64 {
                return None;
            }
            let cv = self.caps[(c - cap_start) as usize];
            let rv = self.refs[(r - ref_start) as usize];
            dot += cv * rv;
            ec += cv * cv;
            er += rv * rv;
        }
        let floor = 1e-8 * n as f32;
        if ec < floor || er < floor {
            return None;
        }
        Some(dot / (ec.sqrt() * er.sqrt()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: &mut u32) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        *seed as f32 / u32::MAX as f32 - 0.5
    }

    #[test]
    fn correlation_finds_a_fractional_echo() {
        let n = 40_000usize;
        let lead = 960usize;
        let delay = 2_882i64;
        let frac = 0.7f32;
        let mut seed = 0x1234_5678u32;
        let render: Vec<f32> = (0..n + lead).map(|_| noise(&mut seed)).collect();
        let mut g = Guard::new();
        g.push_reference(&render[..n + lead]);
        let mut caps: VecDeque<f32> = VecDeque::new();
        for i in 0..n {
            let e = if i as i64 > delay {
                render[i - delay as usize - 1] * frac + render[i - delay as usize] * (1.0 - frac)
            } else {
                0.0
            };
            caps.push_back(e * 0.7);
        }
        g.caps = caps;
        let lag = lead as i64 + delay + 1;
        let score = g
            .corr(n as u64, g.total as i64 - lag, EST_WINDOW, 1)
            .unwrap();
        eprintln!("direct corr at frac lag: {score:.3}");
        assert!(score > 0.8, "correlation {score:.3} at the known lag");
    }
}
