#![cfg(feature = "vaapi")]

//! 7i: does the hardware encoder's BGRA path encode the *right pixels*?
//!
//! The HD sweep proves the stream decodes; it says nothing about what came
//! out. The BGRA path changed in 7i from a CPU BT.601 conversion to a
//! packed-RGB surface uploaded with the producer's own row pitch and
//! converted by the VA-API video processor, so two things can silently break
//! while every keyframe still stands alone:
//!
//! 1. **Color**: the wrong channel order or matrix yields a decodable
//!    picture with swapped or grey colors. A red frame must decode red.
//! 2. **Pitch**: the upload now walks rows at the producer's stride. If the
//!    stride were ignored, a padded capture buffer would decode as a
//!    sheared picture — the classic "diagonal" screen share.
//!
//! Both checks encode with the real `VaapiEncoder` (via `create_encoder_auto`
//! with hardware preference on) and decode with a fresh openh264 decoder, the
//! same contract a viewer has.
//!
//! Run: PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
//!        --test vaapi_bgra_path -- --nocapture --test-threads=1

use parley_core::video::{VideoDecoder, create_encoder_auto, set_hw_encode};

const W: usize = 1280;
const H: usize = 720;

fn vaapi_available() -> bool {
    if std::env::var("PARLEY_VAAPI")
        .map(|v| v != "1")
        .unwrap_or(true)
    {
        eprintln!("vaapi_bgra_path: skipping (set PARLEY_VAAPI=1 to run; needs a VAAPI node)");
        return false;
    }
    true
}

/// Hardware encoder with hardware preference ON; panics if it silently
/// fell back to software, which would make the test pass for the wrong
/// backend.
fn hardware_encoder(w: usize, h: usize) -> Box<dyn parley_core::video::VideoEncoder> {
    set_hw_encode(true);
    let enc = create_encoder_auto(w, h).expect("encoder");
    assert!(
        parley_core::video::hardware_encoder_active(),
        "the encoder fell back to software — this test is about the VAAPI path"
    );
    enc
}

fn encode(enc: &mut dyn parley_core::video::VideoEncoder, frame: &[u8], stride: usize) -> Vec<u8> {
    let (w, h) = enc.dimensions();
    let mut out = Vec::new();
    enc.encode_bgra_strided(frame, w, h, stride, &mut out)
        .expect("encode");
    out
}

fn decode(annexb: &[u8]) -> (Vec<u8>, usize, usize) {
    let mut dec = VideoDecoder::new().expect("decoder");
    let (rgb, w, h) = dec
        .decode_with_dims(annexb)
        .expect("decode")
        .expect("a picture");
    (rgb, w, h)
}

/// Mean of one channel over the centre of the picture, where chroma
/// subsampling and edge effects cannot reach.
fn centre_mean(rgb: &[u8], w: usize, h: usize, channel: usize) -> f64 {
    let (x0, x1) = (w / 4, 3 * w / 4);
    let (y0, y1) = (h / 4, 3 * h / 4);
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            sum += u64::from(rgb[(y * w + x) * 3 + channel]);
            n += 1;
        }
    }
    sum as f64 / n as f64
}

#[test]
fn a_red_frame_decodes_red() {
    if !vaapi_available() {
        return;
    }
    let mut frame = vec![0u8; W * H * 4];
    for px in frame.as_chunks_mut::<4>().0 {
        px[0] = 0; // B
        px[1] = 0; // G
        px[2] = 255; // R
        px[3] = 255; // A (ignored: the surface is BGRX)
    }

    let mut enc = hardware_encoder(W, H);
    let annexb = encode(enc.as_mut(), &frame, W * 4);
    let (rgb, w, h) = decode(&annexb);
    assert_eq!((w, h), (W, H), "decoded size");

    let (r, g, b) = (
        centre_mean(&rgb, w, h, 0),
        centre_mean(&rgb, w, h, 1),
        centre_mean(&rgb, w, h, 2),
    );
    println!("  red frame decodes to R={r:.1} G={g:.1} B={b:.1}");
    // BT.601 red is (255, ~0, ~0) at full range; limited-range conversion
    // plus 4:2:0 chroma leaves headroom, but the ordering is unambiguous.
    assert!(
        r > 180.0,
        "red channel averaged {r:.1} — the picture lost its red"
    );
    assert!(
        g < 90.0 && b < 90.0,
        "green/blue averaged {g:.1}/{b:.1} — channels are swapped or desaturated"
    );
}

#[test]
fn a_padded_stride_decodes_the_same_picture() {
    if !vaapi_available() {
        return;
    }
    // Moving bar over a gradient: any row misalignment shows up as a
    // large difference, unlike a flat color which would survive a shear.
    let mut picture = vec![0u8; W * H * 4];
    for (i, px) in picture.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = i % W;
        let y = i / W;
        px[0] = (x * 255 / W) as u8;
        px[1] = (y * 255 / H) as u8;
        px[2] = if (x as i32 - 400).abs() < 80 && y.is_multiple_of(5) {
            255
        } else {
            48
        };
        px[3] = 255;
    }

    let tight = W * 4;
    let padded = W * 4 + 256;
    let mut padded_picture = vec![0xEEu8; padded * H];
    for y in 0..H {
        padded_picture[y * padded..y * padded + tight]
            .copy_from_slice(&picture[y * tight..(y + 1) * tight]);
    }

    let mut enc = hardware_encoder(W, H);
    // Each run starts on a forced IDR: `create_encoder_auto` measures the
    // profile by encoding frames of its own, so the session's counter is
    // already past zero and this frame is a P-frame unless we ask.
    enc.force_keyframe();
    let from_tight = encode(enc.as_mut(), &picture, tight);
    enc.force_keyframe();
    let from_padded = encode(enc.as_mut(), &padded_picture, padded);

    let (a, wa, ha) = decode(&from_tight);
    let (b, wb, hb) = decode(&from_padded);
    assert_eq!((wa, ha), (wb, hb), "the two runs decoded different sizes");

    let mut diff = 0u64;
    for (x, y) in a.iter().zip(b.iter()) {
        diff += u64::from(x.abs_diff(*y));
    }
    let mean = diff as f64 / a.len() as f64;
    println!("  tight vs padded stride: mean |Δ| = {mean:.3} per channel byte");
    assert!(
        mean < 1.0,
        "padded-stride encode differs from the tight one by {mean:.2} per byte — \
         the row pitch is not being honored"
    );
}
