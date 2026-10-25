#![cfg(feature = "vaapi")]

//! Does a keyframe from the VAAPI encoder stand alone — at every profile
//! the client can ask for?
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
//! 7h HD: the sweep runs the profiles the client actually selects —
//! 720p, 1080p and 1440p at the hardware profile's 60 fps — because a
//! stream shape that decodes at one resolution says nothing about the
//! level, the macroblock geometry or the rate control at another.
//! `PARLEY_VAAPI_SWEEP` overrides the list (e.g. `2560x1440`), and
//! `PARLEY_VAAPI_STREAM` dumps the last hardware stream as Annex-B for
//! an outside decoder (ffmpeg/ffprobe).
//!
//! Run: PARLEY_VAAPI=1 cargo test -p parley-core --features vaapi \
//!        --test vaapi_keyframe_shape -- --nocapture

use parley_core::video::{VideoDecoder, create_encoder_auto, set_hw_encode};

/// (width, height) profiles to sweep.
const PROFILES: &[(usize, usize)] = &[(1280, 720), (1920, 1080), (2560, 1440)];

fn profiles() -> Vec<(usize, usize)> {
    match std::env::var("PARLEY_VAAPI_SWEEP") {
        Ok(v) => v
            .split(',')
            .filter_map(|p| {
                let (w, h) = p.trim().split_once('x')?;
                Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
            })
            .collect(),
        Err(_) => PROFILES.to_vec(),
    }
}

/// Moving pattern, so the encoder has real inter-frame work to do.
fn frame(w: usize, h: usize, t: usize) -> Vec<u8> {
    let mut bgra = vec![0u8; w * h * 4];
    for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
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

/// Encode `n` frames at `w`x`h`, forcing keyframes at 0 and `n / 2`, and
/// return the (frame index, was-flagged-keyframe, Annex-B bytes) of each
/// encoded frame plus the wall time the encode loop took.
fn encode_run(hardware: bool, w: usize, h: usize, n: usize) -> (Vec<(usize, bool, Vec<u8>)>, f64) {
    set_hw_encode(hardware);
    let mut enc = create_encoder_auto(w, h).expect("encoder");
    // Generate outside the timed region: drawing the pattern is a
    // per-pixel CPU loop of its own, and at 1440p it dwarfs the encode.
    let frames: Vec<Vec<u8>> = (0..n).map(|t| frame(w, h, t)).collect();
    let mut out = Vec::new();
    let mut annexb = Vec::new();
    let started = std::time::Instant::now();
    for (t, f) in frames.iter().enumerate() {
        if t == 0 || t == n / 2 {
            enc.force_keyframe();
        }
        annexb.clear();
        let keyframe = enc
            .encode_bgra_strided(f, w, h, w * 4, &mut annexb)
            .expect("encode");
        out.push((t, keyframe, annexb.clone()));
    }
    let secs = started.elapsed().as_secs_f64();
    (out, secs)
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
                "  {name}: keyframe t={t} ({} B, {} NALs {types:?}) stands alone",
                annexb.len(),
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

fn vaapi_available() -> bool {
    if std::env::var("PARLEY_VAAPI")
        .map(|v| v != "1")
        .unwrap_or(true)
    {
        eprintln!("vaapi_keyframe_shape: skipping (set PARLEY_VAAPI=1 to run; needs a VAAPI node)");
        return false;
    }
    true
}

/// The original check at the software profile's resolution: both
/// backends, plus the negative control that proves the check bites.
#[test]
fn forced_keyframes_stand_alone_on_both_backends() {
    if !vaapi_available() {
        return;
    }
    let (w, h) = (1280, 720);

    // Arm 1: software — the known-good control.
    let (software, _) = encode_run(false, w, h, 40);
    assert_keyframes_stand_alone("software", &software);

    // Arm 2: hardware — the arm under test. create_encoder_auto falls
    // back silently, so a software result here would be a false pass:
    // make the backend claim its codec and then confirm it really is
    // the VAAPI one by checking only the frames it flagged.
    let (hardware, secs) = encode_run(true, w, h, 40);

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
    println!(
        "  hardware: {w}x{h}, 40 frames in {secs:.2} s → {:.1} fps encode",
        40.0 / secs
    );

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

/// 7h HD: every profile the client can select must produce standalone
/// keyframes, and the encoder must actually keep up with the rate it
/// advertises (a 60 fps profile that encodes at 20 fps is a lie the
/// share path pays for in dropped frames).
#[test]
fn hardware_hd_profiles_stand_alone() {
    if !vaapi_available() {
        return;
    }
    let frames = 120; // 2 s at the hardware profile's rate
    let mut failures = Vec::new();
    for &(w, h) in &profiles() {
        set_hw_encode(true);
        let (run, secs) = encode_run(true, w, h, frames);
        let achieved = frames as f64 / secs;
        let avg_kb =
            run.iter().map(|(_, _, a)| a.len()).sum::<usize>() as f64 / run.len() as f64 / 1024.0;
        println!(
            "  hd {w}x{h}: {frames} frames in {secs:.2} s → {achieved:.1} fps, {avg_kb:.1} KB/frame avg"
        );
        // Same escape hatch as the profile test: leave the stream behind
        // for an outside decoder (ffprobe -show_frames), named per
        // profile so a sweep does not overwrite itself.
        if let Ok(path) = std::env::var("PARLEY_VAAPI_STREAM") {
            let all: Vec<u8> = run.iter().flat_map(|(_, _, a)| a.clone()).collect();
            let path = format!("{path}.{w}x{h}.annexb");
            std::fs::write(&path, &all).expect("write hardware stream");
            println!("  hd {w}x{h}: wrote {} B to {path}", all.len());
        }
        // Keyframes must stand alone at this resolution (a fresh
        // decoder, exactly like a viewer joining).
        let mut keyframes = 0;
        for (t, keyframe, annexb) in &run {
            if !*keyframe {
                continue;
            }
            keyframes += 1;
            let types = nal_types(annexb);
            if !(types.contains(&7) && types.contains(&8)) {
                failures.push(format!("{w}x{h} t={t}: no SPS/PPS in keyframe"));
                continue;
            }
            match decodes_standalone(annexb) {
                Ok(true) => println!(
                    "  hd {w}x{h}: keyframe t={t} ({} B) stands alone",
                    annexb.len()
                ),
                Ok(false) => println!("  hd {w}x{h}: keyframe t={t} accepted, no picture"),
                Err(e) => failures.push(format!("{w}x{h} t={t}: {e}")),
            }
        }
        if keyframes == 0 {
            failures.push(format!("{w}x{h}: the encoder flagged no keyframe at all"));
        }
        // A silent fallback to software would otherwise be measured
        // against a bar software can clear: name it.
        if !parley_core::video::hardware_encoder_active() {
            failures.push(format!(
                "{w}x{h}: encoder fell back to SOFTWARE — hardware encode unavailable"
            ));
        }
        // The profile advertises a MEASURED rate (7h). The check is that
        // the encoder sustains what it advertised: a stream paced at a
        // rate the encoder cannot hold is late frames and a latency
        // ladder blaming the network for it. 15% of headroom absorbs
        // the difference between the measurement's content and this
        // run's.
        let advertised = f64::from(parley_core::video::target_fps());
        let floor = advertised * 0.85;
        if achieved < floor {
            failures.push(format!(
                "{w}x{h}: {achieved:.1} fps measured, but the profile advertises {advertised:.1} \
                 fps (floor {floor:.1}) — the rate the share paces at must be one the encoder \
                 can hold"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "hardware HD profiles failed:\n  {}",
        failures.join("\n  ")
    );
}
