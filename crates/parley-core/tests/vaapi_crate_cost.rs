#![cfg(feature = "vaapi")]

//! 7h HD: what inside the vendored encode path costs 27 ns/pixel?
//!
//! `vaapi_encode_cost` showed the driver half of the path at ~28 ms per
//! 720p frame (~57 ms at 1080p) while ffmpeg's VAAPI encoder on the same
//! GPU runs 114 fps at 720p and 55 fps at 1080p. So the hardware is not
//! the limit; this crate's per-frame work is. The suspects, in the order
//! the code runs them:
//!
//!   1. `upload_nv12` — `query_image_formats()` (a driver round trip)
//!      plus a fresh `Image` (create/map/unmap) and two surface syncs,
//!      every frame;
//!   2. `encode_surface` — eight `vaCreateBuffer` calls per frame
//!      (sequence/picture/slice params, packed SPS+PPS on IDRs, rate
//!      control, frame rate) plus a full picture sync;
//!   3. `read_coded` — mapping the coded buffer (sized 2 s of bitrate,
//!      ≥ 1.5 MB) each frame.
//!
//! This measures them by subtraction and by varying the parameters the
//! suspects depend on. Nothing here asserts a number — it is a
//! measurement, and it prints what it found.
//!
//! Run: cargo test --release -p parley-core --features vaapi \
//!        --test vaapi_crate_cost -- --nocapture --test-threads=1
//! (Set PARLEY_VAAPI=1 to actually touch the GPU.)

use moq_vaapi::encode::{Config as VaConfig, Encoder as VaEncoder};

fn nv12(w: usize, h: usize, t: u8) -> Vec<u8> {
    let mut v = vec![0u8; w * h + w * h / 2];
    for y in 0..h {
        for x in 0..w {
            v[y * w + x] = ((x + y + usize::from(t)) % 256) as u8;
        }
    }
    for byte in &mut v[w * h..] {
        *byte = 128;
    }
    v
}

fn time_encode(label: &str, w: u32, h: u32, fps: u32, bitrate: u32, n: usize) {
    let mut enc = match VaEncoder::new(VaConfig::new(w, h, fps, bitrate, fps * 2)) {
        Ok(e) => e,
        Err(e) => {
            println!("  {label}: no encoder ({e:#})");
            return;
        }
    };
    let frame = nv12(w as usize, h as usize, 7);
    let started = std::time::Instant::now();
    let mut bytes = 0usize;
    for _ in 0..n {
        match enc.encode_nv12(&frame, false) {
            Ok(out) => bytes += out.len(),
            Err(e) => {
                println!("  {label}: encode failed ({e:#})");
                return;
            }
        }
    }
    let secs = started.elapsed().as_secs_f64();
    println!(
        "  {label}: {:.2} ms/frame → {:.1} fps, {} B/frame out (coded buffer {} B)",
        secs * 1000.0 / n as f64,
        n as f64 / secs,
        bytes / n,
        bitrate as usize * 2 / 8
    );
}

#[test]
fn crate_per_frame_cost() {
    if !std::env::var("PARLEY_VAAPI")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        eprintln!("vaapi_crate_cost: skipping (set PARLEY_VAAPI=1; needs a VAAPI node)");
        return;
    }
    let n = 60;
    // 720p30 (the software profile's shape) vs 720p60: does the frame
    // rate parameter itself cost anything?
    time_encode("720p30 @ 5 Mbps", 1280, 720, 30, 5_000_000, n);
    time_encode("720p60 @ 5 Mbps", 1280, 720, 60, 5_000_000, n);
    // Bitrate 1/10th: if this is much faster, the coded-buffer size
    // (2 s of bitrate, mapped every frame) is the cost.
    time_encode("720p60 @ 0.5 Mbps", 1280, 720, 60, 500_000, n);
    // HD profiles as the client selects them.
    time_encode("1080p60 @ 11.25 Mbps", 1920, 1080, 60, 11_250_000, n);
    time_encode("1080p60 @ 2 Mbps", 1920, 1080, 60, 2_000_000, n);
    time_encode("1440p60 @ 12 Mbps", 2560, 1440, 60, 12_000_000, n);
}
