//! 7k: our own playback, subtracted from the share-audio capture.
//!
//! The share-audio tap is the DEFAULT OUTPUT's monitor (native PipeWire
//! `stream.capture.sink`): every client playing on this machine, which
//! includes THIS app — the voices we render and the other shares we
//! play. Sent back out as "system audio" that is a feedback loop within
//! one round trip (observed in the 7j run: two machines, both sharing,
//! howling at each other).
//!
//! PipeWire's monitor has no per-client exclusion, so the exclusion is
//! done the way a headset does it: an echo canceller whose far-end
//! reference is the exact mix we hand to the output device. The
//! reference is DIGITAL (no acoustic path), so cancellation is
//! near-total — and it can only ever remove what we ourselves played;
//! other applications' audio passes through untouched.
//!
//! Stereo is processed as two mono AEC instances (the shape the mic path
//! already uses) sharing one mono reference. The filter sits between the
//! PipeWire capture and the share-audio mux, so both the system mix and
//! the mic (AEC'd in the voice tee) reach the wire clean of our own
//! output.
//!
//! Cost: two AEC3 instances on a buffer-rate thread, only while a share
//! is running. With no share the render hook is one relaxed atomic load.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{LazyLock, Mutex};

use parley_client::aec::{self, FrameAssembler};

/// True while a share-audio filter owns the reference ring. The render
/// callback checks this before copying anything — the hot path when
/// nobody is sharing is a single atomic load.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mono far-end reference: what the output callback has just played.
/// Newest last. Only filled while ACTIVE.
static REF: LazyLock<Mutex<VecDeque<f32>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// One second of mono reference is orders of magnitude more than the
/// canceller needs; beyond it the OLDEST samples go (the mux's drift-cap
/// rule) rather than growing without bound if the filter stalls.
const REF_CAP: usize = 48_000;

/// Whether a share-audio filter is running (the render hook's guard).
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Render-callback hook: record the played mono mix as the far-end
/// reference. Cheap and non-blocking-in-practice: the lock is held only
/// to append, and the filter drains it on its own thread.
pub(crate) fn push_render(mono: &[f32]) {
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
/// `out` feeds `spawn_share_audio_mux`. AEC init failure degrades to a
/// straight pass-through with a loud line: share audio keeps working,
/// the loop guard does not.
pub(crate) fn spawn_filter(raw: Receiver<Vec<f32>>, out: SyncSender<Vec<f32>>) {
    std::thread::spawn(move || {
        let (left_aec, right_aec) = match (aec::new_echo_canceller(), aec::new_echo_canceller()) {
            (Ok(l), Ok(r)) => (Some(l), Some(r)),
            _ => {
                eprintln!(
                    "share-audio: echo canceller init failed — this share will capture Parley's \
                     own playback (feedback loop risk); route other audio to a second sink or \
                     stop sharing"
                );
                (None, None)
            }
        };
        // Own the reference only for this share; the render hook is a
        // no-op again the moment the flag drops.
        {
            let mut buf = REF.lock().unwrap();
            buf.clear();
        }
        ACTIVE.store(true, Ordering::Relaxed);
        if left_aec.is_some() {
            println!(
                "share-audio: echo guard on — this app's own playback (voice + other shares) is \
                 subtracted from the capture"
            );
        }

        let mut asm_l = FrameAssembler::new();
        let mut asm_r = FrameAssembler::new();
        let mut carry_l: Vec<f32> = Vec::new();
        let mut carry_r: Vec<f32> = Vec::new();
        let mut filtered: Vec<f32> = Vec::new();

        while let Ok(chunk) = raw.recv() {
            if chunk.len() < 2 {
                continue;
            }
            // Reference first: the canceller pairs each capture frame
            // with the render frames that preceded it.
            if let (Some(l), Some(r)) = (&left_aec, &right_aec) {
                let refs: Vec<f32> = {
                    let mut buf = REF.lock().unwrap();
                    if buf.is_empty() {
                        Vec::new()
                    } else {
                        buf.drain(..).collect()
                    }
                };
                if !refs.is_empty() {
                    aec::analyze_render(l, &mut carry_l, &refs);
                    aec::analyze_render(r, &mut carry_r, &refs);
                }

                let mut pl: Vec<f32> = Vec::with_capacity(chunk.len() / 2);
                let mut pr: Vec<f32> = Vec::with_capacity(chunk.len() / 2);
                for pair in chunk.as_chunks::<2>().0.iter() {
                    pl.push(pair[0]);
                    pr.push(pair[1]);
                }
                let mut ol: Vec<f32> = Vec::new();
                let mut or: Vec<f32> = Vec::new();
                aec::process_capture(l, &mut asm_l, &pl, &mut ol);
                aec::process_capture(r, &mut asm_r, &pr, &mut or);
                let frames = ol.len().min(or.len());
                filtered.clear();
                for i in 0..frames {
                    filtered.push(ol[i]);
                    filtered.push(or[i]);
                }
            } else {
                filtered.clear();
                filtered.extend_from_slice(&chunk);
            }

            if filtered.is_empty() {
                continue;
            }
            match out.try_send(std::mem::take(&mut filtered)) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;
    use std::time::{Duration, Instant};

    const FRAME: usize = 480; // 10 ms @ 48 kHz — one AEC frame

    fn tone(frame: usize) -> Vec<f32> {
        (0..FRAME)
            .map(|n| {
                let t = (frame * FRAME + n) as f32 / 48_000.0;
                0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect()
    }

    /// The loop guard, end to end through the real filter: the captured
    /// system mix contains a delayed copy of what we just played (the
    /// default-sink monitor always does), and the filter has to take it
    /// out — otherwise every share with audio feeds the voice channel
    /// back into itself.
    ///
    /// Synthetic, no hardware and no PipeWire — the echo path is
    /// constructed, exactly like crates/parley-client/tests/aec.rs: the
    /// reference is fed through `push_render` (the output callback's
    /// hook) and the capture through the filter's channel.
    #[test]
    fn own_playback_is_cancelled_from_the_capture() {
        let (raw_tx, raw_rx) = sync_channel::<Vec<f32>>(64);
        let (out_tx, out_rx) = sync_channel::<Vec<f32>>(64);
        spawn_filter(raw_rx, out_tx);

        // spawn_filter arms from its own thread; references pushed
        // before that are dropped by design.
        let armed = Instant::now();
        while !active() && armed.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(active(), "filter never armed");

        const DELAY: usize = 6; // 60 ms synthetic echo path
        let warmup = 150; // 3 s of adaptation
        let measure = 100;
        let mut in_energy = 0.0f32;
        let mut played: Vec<Vec<f32>> = Vec::new();
        let mut filtered: Vec<f32> = Vec::new();

        for i in 0..(warmup + measure) {
            let render = tone(i);
            push_render(&render);

            // What the monitor tap hears: the playback from DELAY
            // frames ago, attenuated, in both channels.
            let source = if i >= DELAY {
                &played[i - DELAY]
            } else {
                &render // always younger than the reference store can hold
            };
            let capture: Vec<f32> = source.iter().flat_map(|&s| [s * 0.7, s * 0.7]).collect();
            if i >= warmup {
                in_energy += capture.iter().map(|s| s * s).sum::<f32>();
            }
            raw_tx.send(capture).unwrap();
            played.push(render);

            // Drain as we go: the filter must never block on its output.
            while let Ok(chunk) = out_rx.try_recv() {
                filtered.extend(chunk);
            }
            if i + 1 == warmup {
                filtered.clear(); // discard adaptation output
            }
        }

        // Drain anything the filter still holds (one frame of assembly).
        drop(raw_tx);
        let stopped = Instant::now();
        while active() && stopped.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!active(), "filter did not exit after the capture closed");
        while let Ok(chunk) = out_rx.try_recv() {
            filtered.extend(chunk);
        }

        let out_energy: f32 = filtered.iter().map(|s| s * s).sum();

        let reduction_db = 10.0 * (in_energy / out_energy.max(1e-12)).log10();
        eprintln!(
            "share-audio echo test: {reduction_db:.1} dB reduction (in {in_energy:.3}, out {out_energy:.3})"
        );
        assert!(
            reduction_db > 6.0,
            "own playback only {reduction_db:.1} dB down after {warmup} frames \
             (in {in_energy:.3}, out {out_energy:.3}) — a share would still loop"
        );
    }
}
