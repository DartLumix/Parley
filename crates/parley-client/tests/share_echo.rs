//! The share-audio echo guard, end to end through the real filter.
//!
//! The captured system mix contains a delayed copy of what this app just
//! played (the default-sink monitor always does), plus whatever other
//! applications are playing. The guard must subtract the copy while
//! leaving the rest alone — that is the whole product requirement: a
//! viewer must not hear their own voice in the shared stream, and the
//! sharer's music must not be chewed up to get there.
//!
//! Synthetic, no hardware and no PipeWire: the echo path is constructed
//! (a delay, a gain, and optionally a fractional-sample offset), and the
//! reference is fed through `push_render` exactly as the output callback
//! feeds it.

use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use parley_client::share_echo;

const FRAME: usize = 480; // 10 ms @ 48 kHz
const CAPTURE_CHUNK: usize = 1_024; // one PipeWire quantum
const WARMUP: usize = 300; // 3 s
const MEASURE: usize = 200; // 2 s

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as f32 / u32::MAX as f32 - 0.5
    }
}

/// One run: echo delay in samples (fractional part in tenths), echo gain,
/// near-end amplitude. Returns (echo reduction dB, near-end preservation
/// dB) over the measured seconds.
fn run(delay: usize, frac_tenths: u32, gain: f32, near_amp: f32) -> (f32, f32) {
    let frames = (WARMUP + MEASURE) * FRAME;
    let mut r = Rng(0x1234_5678);
    let render: Vec<f32> = (0..frames).map(|_| r.next()).collect();
    let mut n = Rng(0x9e37_79b9);
    let near: Vec<f32> = (0..frames).map(|_| n.next() * near_amp).collect();

    let (raw_tx, raw_rx) = sync_channel::<Vec<f32>>(8);
    let (out_tx, out_rx) = sync_channel::<Vec<f32>>(8);
    share_echo::spawn_filter(raw_rx, out_tx);
    let armed = Instant::now();
    while !share_echo::active() && armed.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(share_echo::active(), "guard never armed");

    let frac = frac_tenths as f32 / 10.0;
    let mut out: Vec<f32> = Vec::with_capacity(frames);
    // The output callback runs ahead of the monitor by the sink's own
    // buffer; model a constant 20 ms lead. The lead must not drift: both
    // streams run at 48 kHz, so the same number of reference samples is
    // pushed per capture frame.
    let lead = 960usize;
    share_echo::push_render(&render[..lead]);
    let mut pushed = lead;
    let mut captured = 0usize;
    while captured < frames {
        let end = (captured + CAPTURE_CHUNK).min(frames);
        let count = (end - captured).min(frames - pushed);
        share_echo::push_render(&render[pushed..pushed + count]);
        pushed += count;
        let mut chunk = Vec::with_capacity((end - captured) * 2);
        for i in captured..end {
            let echo = if i > delay {
                let newer = render[i - delay];
                let older = render[i - delay - 1];
                // A fractional delay: `frac` of the older sample.
                older * frac + newer * (1.0 - frac)
            } else {
                0.0
            };
            let s = echo * gain + near[i];
            chunk.push(s);
            chunk.push(s);
        }
        raw_tx.send(chunk).unwrap();
        captured = end;
        // Pace the run: the real capture delivers one quantum at a time
        // and the filter keeps up with it. Without this the synthetic
        // producer would run seconds ahead and turn a fixed echo delay
        // into a moving one (a property no real sink has).
        let deadline = Instant::now() + Duration::from_secs(5);
        while out.len() < captured * 2 {
            while let Ok(filtered) = out_rx.try_recv() {
                out.extend(filtered);
            }
            if Instant::now() > deadline {
                panic!(
                    "filter fell behind: {} of {} frames",
                    out.len() / 2,
                    captured
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    drop(raw_tx);
    let stopped = Instant::now();
    while share_echo::active() && stopped.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(5));
    }
    while let Ok(filtered) = out_rx.try_recv() {
        out.extend(filtered);
    }

    let start = WARMUP * FRAME;
    let mut in_echo = 0.0f32;
    let mut out_echo = 0.0f32;
    let mut out_all = 0.0f32;
    let mut in_near = 0.0f32;
    for i in start..frames {
        let echo = if i > delay {
            let newer = render[i - delay];
            let older = render[i - delay - 1];
            (older * frac + newer * (1.0 - frac)) * gain
        } else {
            0.0
        };
        let n = near[i];
        in_echo += 2.0 * echo * echo;
        in_near += 2.0 * n * n;
        let o = out[2 * i];
        out_all += 2.0 * o * o;
        let residual = o - n;
        out_echo += 2.0 * residual * residual;
    }

    let reduction = 10.0 * (in_echo / out_echo.max(1e-12)).log10();
    let preservation = 10.0 * (out_all / in_near.max(1e-12)).log10();
    (reduction, preservation)
}

#[test]
fn own_playback_is_subtracted_and_other_audio_survives() {
    // Integer delay, 60 ms, strong echo, a quiet near-end: the everyday
    // case (a voice call through the sink while sharing).
    let (reduction, _) = run(2_880, 0, 0.7, 0.0);
    eprintln!("echo-only, 60 ms integer delay: {reduction:.1} dB");
    assert!(
        reduction > 30.0,
        "own playback only {reduction:.1} dB down — a viewer would still hear themselves"
    );

    // The same echo alongside other applications' audio. The near-end
    // must come through essentially unchanged; the echo must not.
    let (reduction, preservation) = run(2_880, 0, 0.7, 0.25);
    eprintln!(
        "echo + near-end: {reduction:.1} dB echo reduction, {preservation:+.1} dB near-end change"
    );
    assert!(
        reduction > 30.0,
        "echo only {reduction:.1} dB down with near-end present"
    );
    assert!(
        preservation.abs() < 1.0,
        "near-end audio changed by {preservation:+.1} dB — the share would sound processed"
    );

    // A fractional-sample echo path (the sink resampling): the adaptive
    // filter has to interpolate, not just shift.
    let (reduction, _) = run(2_881, 7, 0.7, 0.2);
    eprintln!("fractional delay (60.7 ms): {reduction:.1} dB echo reduction");
    assert!(
        reduction > 20.0,
        "fractional echo only {reduction:.1} dB down"
    );
}
