#![cfg(feature = "vaapi")]

//! 7h HD: where does the VAAPI encode path spend its time?
//!
//! The HD sweep measured 30 fps at 720p, 15 at 1080p, 8.6 at 1440p —
//! i.e. cost scaling with pixels, not a frame-rate cap. The two
//! candidates are the CPU BGRA→NV12 conversion (scalar, per pixel) and
//! the driver's own encode. This test separates them, because the fix
//! is completely different for each: a conversion fix is a loop, a
//! driver fix is a DMA-BUF path.
//!
//! Run: cargo test --release -p parley-core --features vaapi \
//!        --test vaapi_encode_cost -- --nocapture --test-threads=1
//! (PARLEY_VAAPI=1 additionally times the driver half.)

use parley_core::video_vaapi::bgra_to_nv12;

fn frame(w: usize, h: usize, t: usize) -> Vec<u8> {
    let mut bgra = vec![0u8; w * h * 4];
    for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
        let x = i % w;
        let y = i / w;
        let bar = ((t * 37) % w) as i32;
        px[0] = (x * 255 / w) as u8;
        px[1] = (y * 255 / h) as u8;
        px[2] = if (x as i32 - bar).abs() < 60 && y.is_multiple_of(5) {
            255
        } else {
            48
        };
        px[3] = 255;
    }
    bgra
}

#[test]
fn conversion_and_encode_cost_per_profile() {
    let profiles = [(1280usize, 720usize), (1920, 1080), (2560, 1440)];
    let n = 30;
    println!(
        "  n_threads available: {}",
        std::thread::available_parallelism().is_ok()
    );

    for &(w, h) in &profiles {
        let frames: Vec<Vec<u8>> = (0..n).map(|t| frame(w, h, t)).collect();

        // Half 1: the CPU conversion alone.
        let started = std::time::Instant::now();
        let mut sink = 0usize;
        for f in &frames {
            let nv12 = bgra_to_nv12(f, w, h).expect("convert");
            sink += nv12.len();
        }
        let conv = started.elapsed().as_secs_f64();
        println!(
            "  {w}x{h}: BGRA→NV12 {:.2} ms/frame ({:.1} fps ceiling), {} bytes/frame",
            conv * 1000.0 / n as f64,
            n as f64 / conv,
            sink / n
        );

        // Half 2: the whole encoder (conversion + upload + driver),
        // only when a VAAPI node is asked for — this half can wedge a
        // GPU whose VCE is broken, which is the whole reason the
        // hardware path is opt-in.
        if std::env::var("PARLEY_VAAPI")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            parley_core::video::set_hw_encode(true);
            let Ok(mut enc) = parley_core::video::create_encoder_auto(w, h) else {
                println!("  {w}x{h}: no encoder");
                continue;
            };
            let (ew, eh) = enc.dimensions();
            // Pre-generate OUTSIDE the timed region: generation is a
            // per-pixel CPU loop of its own and belongs to the test,
            // not to the encoder (timing it once produced a "driver
            // half" that was mostly pattern-drawing).
            let frames: Vec<Vec<u8>> = (0..n).map(|t| frame(ew, eh, t)).collect();
            let started = std::time::Instant::now();
            let mut bytes = 0usize;
            let mut out = Vec::new();
            for f in &frames {
                enc.encode_bgra_strided(f, ew, eh, ew * 4, &mut out)
                    .expect("encode");
                bytes += out.len();
            }
            let total = started.elapsed().as_secs_f64();
            println!(
                "  {w}x{h}: full encode {:.2} ms/frame → {:.1} fps (hardware={}), driver half \
                 ≈ {:.2} ms/frame, {} B/frame out",
                total * 1000.0 / n as f64,
                n as f64 / total,
                parley_core::video::hardware_encoder_active(),
                (total - conv) * 1000.0 / n as f64,
                bytes / n
            );
            println!(
                "  {w}x{h}: profile advertised rate = {:.1} fps (measured {:?})",
                parley_core::video::target_fps(),
                parley_core::video::hardware_measured_fps()
            );
        }
    }
}
