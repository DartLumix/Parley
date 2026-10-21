#![cfg(feature = "video")]

//! Can the software encoder keep a share's pacing budget?
//!
//! A screen share is paced: one frame every `1/paced_fps`. If an encode
//! takes longer than that, the next capture is already stale when the
//! encoder picks it up and the sender skips it — the viewer sees the
//! stream step instead of flow. So the number that matters is not "how
//! fast is the encoder" but "how often does a frame overrun its own
//! budget".
//!
//! This measures that directly: 720p30 and 1080p30 (the software
//! profile's working range, and the resolutions the auto encoder is
//! allowed to pick), then 1440p for information only — the software
//! backend is not given 1440p because of what the last line shows.
//! It is `#[ignore]`d because it encodes hundreds of real frames (a few
//! seconds of CPU) and `cargo test --workspace` should stay quick; run
//! it by hand when touching the encoder path:
//!
//! ```sh
//! cargo test -p parley-core --release --features video \
//!   --test software_encode_cost -- --ignored --nocapture
//! ```

use std::time::Instant;

use parley_core::video::{
    SOFTWARE_MAX_HEIGHT, SoftwareEncoder, VIDEO_FPS_SOFTWARE, VideoEncoder, bitrate_for,
};

/// A screen-content-ish BGRA frame.
///
/// Flat backgrounds encode for almost nothing, so a synthetic frame that
/// is only a moving rectangle flatters the encoder. Real desktops are
/// walls of small high-contrast edges (glyph strokes) that a dead-zone
/// quantiser cannot skip, and they shift a pixel at a time — which is the
/// case that actually costs an encoder its frame budget. This paints rows
/// of 1–3 px "text" runs and jitters them horizontally per frame.
fn frame(width: usize, height: usize, t: usize) -> Vec<u8> {
    let mut bgra = vec![0u8; width * height * 4];
    for px in bgra.chunks_exact_mut(4) {
        px[0] = 24; // B
        px[1] = 26; // G
        px[2] = 30; // R
        px[3] = 255;
    }
    let mut seed = 0x2545_f491u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let shift = t % 2;
    for line in 0..(height / 16) {
        let y0 = line * 16 + 2;
        let mut x = 40 + shift;
        while x + 8 < width {
            let run = 2 + (next() % 6) as usize;
            let bright = (next() & 1) == 0;
            let (b, g, r) = if bright {
                (250, 250, 245)
            } else {
                (120, 150, 230)
            };
            for y in y0..(y0 + 10).min(height) {
                for xx in x..(x + run).min(width) {
                    let i = (y * width + xx) * 4;
                    bgra[i] = b;
                    bgra[i + 1] = g;
                    bgra[i + 2] = r;
                }
            }
            // A descender: vertical detail a horizontal MC search misses.
            if next() & 3 == 0 {
                let xx = (x + run).min(width - 1);
                for y in y0..(y0 + 14).min(height) {
                    let i = (y * width + xx) * 4;
                    bgra[i] = 200;
                    bgra[i + 1] = 200;
                    bgra[i + 2] = 190;
                }
            }
            x += run + 4 + (next() % 7) as usize;
        }
    }
    bgra
}

struct Measured {
    mean: f64,
    p95: f64,
    over: usize,
    over_key: usize,
    mbps: f64,
}

/// Encode `frames` frames at `w`x`h` and report the timing spread against
/// the software profile's frame budget.
fn measure(w: usize, h: usize, frames: usize) -> Measured {
    let bps = bitrate_for(w, h, VIDEO_FPS_SOFTWARE);
    let mut encoder =
        SoftwareEncoder::with_bitrate(w, h, bps).unwrap_or_else(|e| panic!("{w}x{h} encoder: {e}"));
    let budget_ms = 1000.0 / f64::from(VIDEO_FPS_SOFTWARE);
    let mut out = Vec::new();
    let mut times = Vec::with_capacity(frames);
    let mut bytes = 0usize;
    let (mut over, mut over_key) = (0usize, 0usize);

    // One warm-up frame: first-call setup is not pacing.
    let warm = frame(w, h, 0);
    let _ = encoder.encode_bgra_strided(&warm, w, h, w * 4, &mut out);

    for t in 1..=frames {
        let src = frame(w, h, t);
        let start = Instant::now();
        let keyframe = encoder
            .encode_bgra_strided(&src, w, h, w * 4, &mut out)
            .unwrap_or_else(|e| panic!("{w}x{h} encode: {e}"));
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        if ms > budget_ms {
            over += 1;
            if keyframe {
                over_key += 1;
            }
        }
        times.push(ms);
        bytes += out.len();
    }

    times.sort_by(f64::total_cmp);
    Measured {
        mean: times.iter().sum::<f64>() / times.len() as f64,
        p95: times[(times.len() * 95) / 100],
        over,
        over_key,
        mbps: (bytes * 8) as f64 / (frames as f64 / f64::from(VIDEO_FPS_SOFTWARE)) / 1e6,
    }
}

#[test]
#[ignore = "encodes hundreds of frames; run by hand when touching the encoder path"]
fn software_encode_sustains_the_30fps_budget() {
    let budget_ms = 1000.0 / f64::from(VIDEO_FPS_SOFTWARE);
    let frames = 240; // ~8 s of stream at 30 fps

    // The two sizes the software profile is actually allowed to run.
    for (w, h) in [(1280usize, 720usize), (1920, 1080)] {
        let m = measure(w, h, frames);
        println!(
            "{w}x{h}: {:.1} Mbps — encode mean {:.1} ms, p95 {:.1} ms, {}/{frames} over the \
             {budget_ms:.1} ms budget ({over_key} of them keyframes)",
            m.mbps,
            m.mean,
            m.p95,
            m.over,
            over_key = m.over_key
        );
        assert!(
            m.mean < budget_ms * 0.8,
            "{w}x{h} software encode averages {:.1} ms, over 80% of its {budget_ms:.1} ms frame \
             budget — the profile must be capped lower (SOFTWARE_MAX_HEIGHT)",
            m.mean
        );
    }

    // Information only: this is why SOFTWARE_MAX_HEIGHT exists. A 1440p
    // software share is not offered at all; if this ever starts fitting
    // the budget, the cap deserves another look.
    let (w, h) = (2560usize, 1440usize);
    let m = measure(w, h, frames / 2);
    println!(
        "{w}x{h} (not offered in software, cap is {SOFTWARE_MAX_HEIGHT}p): {:.1} Mbps — mean \
         {:.1} ms, p95 {:.1} ms, {}/{frames} over budget",
        m.mbps,
        m.mean,
        m.p95,
        m.over,
        frames = frames / 2
    );
    println!(
        "(software ceiling is {SOFTWARE_MAX_HEIGHT}p; if the 1440p line ever fits the budget, \
         the cap deserves another look)"
    );
}
