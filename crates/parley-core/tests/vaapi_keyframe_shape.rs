#![cfg(feature = "vaapi")]

//! Does a keyframe from the VAAPI encoder stand alone?
//!
//! The receiver's contract: a viewer's decoder is created mid-stream and
//! fed its first frame, which is always a wire-flagged keyframe. That
//! only works when the keyframe repeats SPS/PPS in-band — otherwise
//! openh264 answers `dsNoParamSets` (Native:16), which is exactly the
//! failure seen decoding a hardware-encoded share.
//!
//! This reproduces the situation locally with no second machine: encode
//! with the real `VaapiEncoder` (the backend the sender selects), then
//! decode each forced keyframe with a FRESH `VideoDecoder`.
//!
//! Three arms, so a pass means something:
//!   * software (openh264)  — the known-good control; must stand alone
//!   * hardware (VAAPI)     — the arm under test
//!   * hardware, parameter sets stripped — the negative control; must
//!     FAIL, proving the check can detect a bad keyframe
//!
//! Run: PARLEY_VAAPI=1 cargo test -p parley-core --features vaapi \
//!        --test vaapi_keyframe_shape -- --nocapture

use parley_core::video::{VideoDecoder, create_encoder_auto, set_hw_encode};

const W: usize = 1280;
const H: usize = 720;

/// Moving pattern, so the encoder has real inter-frame work to do.
fn frame(t: usize) -> Vec<u8> {
    let mut bgra = vec![0u8; W * H * 4];
    for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
        let x = i % W;
        let y = i / W;
        let bar = ((t * 37) % W) as i32;
        px[0] = (x * 255 / W) as u8;
        px[1] = (y * 255 / H) as u8;
        px[2] = if (x as i32 - bar).abs() < 60 && y % 5 == 0 {
            255
        } else {
            48
        };
        px[3] = 255;
    }
    bgra
}

/// NAL unit types in an Annex-B buffer (H.264: low 5 bits of byte 0).
fn nal_types(annexb: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < annexb.len() {
        let sc = if annexb[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if annexb[i..i + 3] == [0, 0, 1] {
            3
        } else {
            i += 1;
            continue;
        };
        let start = i + sc;
        if start < annexb.len() {
            types.push(annexb[start] & 0x1F);
        }
        i = start;
    }
    types
}

/// Drop SPS(7)/PPS(8) NALs, keeping everything else — the negative
/// control's "encoder that does not repeat parameter sets".
fn strip_parameter_sets(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut boundaries = Vec::new();
    let mut i = 0;
    while i + 3 < annexb.len() {
        let sc = if annexb[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if annexb[i..i + 3] == [0, 0, 1] {
            3
        } else {
            i += 1;
            continue;
        };
        boundaries.push((i, sc));
        i += sc;
    }
    for (n, &(start, sc)) in boundaries.iter().enumerate() {
        let end = boundaries
            .get(n + 1)
            .map(|&(s, _)| s)
            .unwrap_or(annexb.len());
        let ty = annexb[start + sc] & 0x1F;
        if ty != 7 && ty != 8 {
            out.extend_from_slice(&annexb[start..end]);
        }
    }
    out
}

/// Encode `n` frames, forcing keyframes at 0 and `n / 2`, and return the
/// (frame index, was-flagged-keyframe, Annex-B bytes) of each encoded
/// frame. `hardware_stripped` removes SPS/PPS from reported keyframes.
fn encode_run(hardware: bool, n: usize) -> Vec<(usize, bool, Vec<u8>)> {
    set_hw_encode(hardware);
    let mut enc = create_encoder_auto(W, H).expect("encoder");
    let mut out = Vec::new();
    for t in 0..n {
        if t == 0 || t == n / 2 {
            enc.force_keyframe();
        }
        let mut annexb = Vec::new();
        let keyframe = enc
            .encode_bgra_strided(&frame(t), W, H, W * 4, &mut annexb)
            .expect("encode");
        out.push((t, keyframe, annexb));
    }
    out
}

/// The receiver's real question: does a FRESH decoder accept this frame?
fn decodes_standalone(annexb: &[u8]) -> Result<bool, String> {
    let mut dec = VideoDecoder::new().map_err(|e| e.to_string())?;
    match dec.decode_with_dims(annexb) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false), // accepted, nothing emitted yet
        Err(e) => Err(e.to_string()),
    }
}

/// Assert every keyframe-flagged frame of a run carries SPS+PPS and is
/// accepted by a fresh decoder. Returns the number of keyframes checked.
fn assert_keyframes_stand_alone(name: &str, run: &[(usize, bool, Vec<u8>)]) -> usize {
    let mut checked = 0;
    for (t, keyframe, annexb) in run {
        if !*keyframe {
            continue;
        }
        checked += 1;
        let types = nal_types(annexb);
        assert!(
            types.contains(&7) && types.contains(&8),
            "{name}: keyframe at t={t} is missing SPS/PPS in-band (NAL types {types:?}) — \
             a mid-stream receiver cannot decode it"
        );
        match decodes_standalone(annexb) {
            Ok(true) => println!(
                "  {name}: keyframe t={t} ({} NALs {types:?}) stands alone",
                types.len()
            ),
            Ok(false) => println!("  {name}: keyframe t={t} accepted, no picture yet"),
            Err(e) => panic!("{name}: fresh decoder rejected the keyframe at t={t}: {e}"),
        }
    }
    assert!(
        checked >= 2,
        "{name}: expected forced keyframes, got {checked}"
    );
    checked
}

#[test]
fn forced_keyframes_stand_alone_on_both_backends() {
    if std::env::var("PARLEY_VAAPI")
        .map(|v| v != "1")
        .unwrap_or(true)
    {
        eprintln!("vaapi_keyframe_shape: skipping (set PARLEY_VAAPI=1 to run; needs a VAAPI node)");
        return;
    }

    // Arm 1: software — the known-good control.
    let software = encode_run(false, 40);
    assert_keyframes_stand_alone("software", &software);

    // Arm 2: hardware — the arm under test. create_encoder_auto falls
    // back silently, so a software result here would be a false pass:
    // make the backend claim its codec and then confirm it really is
    // the VAAPI one by checking only the frames it flagged.
    let hardware = encode_run(true, 40);

    // Hand the whole hardware stream to whoever wants to look at it with a
    // decoder of their own (ffmpeg/ffprobe) — this is how the stream shape
    // gets checked against something that is not openh264. Written before the
    // assertions below, so a failing run still leaves the evidence behind.
    if let Ok(path) = std::env::var("PARLEY_VAAPI_STREAM") {
        let all: Vec<u8> = hardware.iter().flat_map(|(_, _, a)| a.clone()).collect();
        std::fs::write(&path, &all).expect("write hardware stream");
        println!("  hardware: wrote {} B of Annex-B to {path}", all.len());
    }
    for (t, keyframe, annexb) in hardware.iter().take(6) {
        println!(
            "  hardware: t={t} keyframe={keyframe} {} B NALs {:?}",
            annexb.len(),
            nal_types(annexb)
        );
    }
    let hw_keyframes: Vec<_> = hardware.iter().filter(|(_, k, _)| *k).collect();
    assert_eq!(
        hw_keyframes.len(),
        2,
        "expected the two forced keyframes, got {}",
        hw_keyframes.len()
    );
    for (t, _, annexb) in &hw_keyframes {
        let types = nal_types(annexb);
        assert!(
            types.contains(&7) && types.contains(&8),
            "hardware: keyframe at t={t} is missing SPS/PPS in-band (NAL types {types:?})"
        );
        if let Err(e) = decodes_standalone(annexb) {
            panic!("hardware: fresh decoder rejected the keyframe at t={t}: {e}");
        }
        println!(
            "  hardware: keyframe t={t} ({} B, NALs {types:?}) stands alone",
            annexb.len()
        );
    }

    // Arm 3: negative control — the same bytes with parameter sets
    // removed MUST be rejected, else the check above proves nothing.
    let (t, _, annexb) = hw_keyframes[0];
    let stripped = strip_parameter_sets(annexb);
    let stripped_types = nal_types(&stripped);
    assert!(
        !stripped_types.contains(&7) && !stripped_types.contains(&8),
        "negative control: SPS/PPS still present after stripping ({stripped_types:?})"
    );
    match decodes_standalone(&stripped) {
        Err(e) => println!("  negative control: stripped keyframe t={t} correctly rejected: {e}"),
        Ok(_) => panic!(
            "negative control: a keyframe without SPS/PPS was accepted — the standalone \
             check cannot detect the bug it is meant to detect"
        ),
    }
}
