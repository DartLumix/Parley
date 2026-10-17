#![cfg(feature = "vaapi")]

//! 7j: does the encoder accept a frame it does not own?
//!
//! The DMA-BUF path is the capture side's zero-copy handoff: PipeWire gives
//! the share a descriptor describing the compositor's buffer, and the encoder
//! imports it instead of receiving pixels. That contract is testable without
//! a portal — export a VA surface as a DMA-BUF, exactly what a compositor
//! does, and feed the descriptor to the encoder through the trait.
//!
//! What it proves:
//! - `dmabuf_supported()` is true for the backend the share picks;
//! - `encode_dmabuf` imports the descriptor, encodes it and produces a
//!   picture a fresh decoder accepts;
//! - the picture is the producer's, not garbage: the left half of the
//!   surface is dark and the right half is bright, and the decode agrees.
//!
//! What it does NOT prove: that a real PipeWire producer's modifier is
//! importable, or that the buffer is still held while the GPU reads it. The
//! first needs the real compositor, the second is the capture side's
//! lifetime contract.
//!
//! Run: PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
//!        --test vaapi_dmabuf_input -- --nocapture --test-threads=1

use moq_vaapi::dmabuf::drm;
use moq_vaapi::{Display, Image, UsageHint, VA_FOURCC_NV12, VA_RT_FORMAT_YUV420};
use parley_core::video::{
    DmaBufFrame, DmaBufPlane, VideoDecoder, create_encoder_auto, set_hw_encode,
};

const W: u32 = 1280;
const H: u32 = 720;

fn vaapi_available() -> bool {
    if std::env::var("PARLEY_VAAPI")
        .map(|v| v != "1")
        .unwrap_or(true)
    {
        eprintln!("vaapi_dmabuf_input: skipping (set PARLEY_VAAPI=1 to run; needs a VAAPI node)");
        return false;
    }
    true
}

/// A producer's DMA-BUF: NV12, left half dark (Y=40), right half bright
/// (Y=200), neutral chroma — recognisable after the encoder and a decoder.
fn produce() -> Option<DmaBufFrame> {
    let display = Display::open()?;
    let mut surfaces = display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            W,
            H,
            Some(UsageHint::USAGE_HINT_EXPORT),
            vec![()],
        )
        .expect("allocate a producer surface");
    let surface = surfaces.pop().expect("one surface");

    let format = display
        .query_image_formats()
        .expect("query image formats")
        .into_iter()
        .find(|f| f.fourcc == VA_FOURCC_NV12)
        .expect("an NV12 image format");
    {
        let mut image =
            Image::create_from(&surface, format, (W, H), (W, H)).expect("create an image");
        let va_image = *image.image();
        let data = image.as_mut();
        for y in 0..H as usize {
            let row = va_image.offsets[0] as usize + y * va_image.pitches[0] as usize;
            for x in 0..W as usize {
                data[row + x] = if x < W as usize / 2 { 40 } else { 200 };
            }
        }
        for y in 0..H as usize / 2 {
            let row = va_image.offsets[1] as usize + y * va_image.pitches[1] as usize;
            data[row..row + W as usize].fill(128);
        }
    }

    let mut exported = surface.export_prime().expect("export the producer surface");
    assert_eq!(exported.objects.len(), 1, "a composed export is one object");
    let planes = (0..exported.layers[0].num_planes as usize)
        .map(|i| DmaBufPlane {
            offset: exported.layers[0].offset[i],
            pitch: exported.layers[0].pitch[i],
        })
        .collect();
    let object = exported.objects.remove(0);
    Some(DmaBufFrame {
        fd: object.fd,
        drm_format: drm::NV12,
        modifier: object.drm_format_modifier,
        width: W,
        height: H,
        planes,
    })
}

/// Mean of one channel over a horizontal band of the decoded picture.
fn band_mean(rgb: &[u8], w: usize, h: usize, channel: usize, left: bool) -> f64 {
    let (x0, x1) = if left {
        (w / 8, 3 * w / 8)
    } else {
        (5 * w / 8, 7 * w / 8)
    };
    let (y0, y1) = (h / 3, 2 * h / 3);
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
fn a_producers_dmabuf_encodes_to_its_own_picture() {
    if !vaapi_available() {
        return;
    }
    let Some(frame) = produce() else {
        eprintln!("vaapi_dmabuf_input: no VA-API display");
        return;
    };
    println!(
        "  producer DMA-BUF: {}x{} drm {:#010x} modifier {:#x}, {} plane(s)",
        frame.width,
        frame.height,
        frame.drm_format,
        frame.modifier,
        frame.planes.len()
    );

    set_hw_encode(true);
    let mut enc = create_encoder_auto(W as usize, H as usize).expect("encoder");
    assert!(
        parley_core::video::hardware_encoder_active(),
        "the encoder fell back to software — this test is about the VAAPI import"
    );
    assert!(
        enc.dmabuf_supported(),
        "the VAAPI backend must accept DMA-BUF input"
    );

    enc.force_keyframe();
    let mut annexb = Vec::new();
    let keyframe = enc
        .encode_dmabuf(frame, &mut annexb)
        .expect("encode a DMA-BUF");
    assert!(keyframe, "a forced IDR must report keyframe");

    let mut dec = VideoDecoder::new().expect("decoder");
    let (rgb, w, h) = dec
        .decode_with_dims(&annexb)
        .expect("decode")
        .expect("a picture");
    assert_eq!((w, h), (W as usize, H as usize), "decoded size");

    // Luma is the channel that carries the producer's halves.
    let (left, right) = (
        band_mean(&rgb, w, h, 0, true),
        band_mean(&rgb, w, h, 0, false),
    );
    println!("  decoded luma: left half {left:.1}, right half {right:.1}");
    assert!(
        right > left + 100.0,
        "the decoded picture does not carry the producer's halves (left {left:.1}, right {right:.1})"
    );
    // BT.601 limited: Y=40 ≈ 28, Y=200 ≈ 214. Generous bounds — this is a
    // check that the import read the right memory, not a color-accuracy one.
    assert!(
        (20.0..60.0).contains(&left),
        "left half luma {left:.1} is not the dark half the producer wrote"
    );
    assert!(
        (180.0..230.0).contains(&right),
        "right half luma {right:.1} is not the bright half the producer wrote"
    );
}
