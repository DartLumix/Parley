//! Synthetic AEC verification (Phase 4a): no hardware needed. The mic on
//! the dev machine cannot acoustically hear its own headphones, so real
//! echo cancellation can't be observed by ear — this test
//! constructs the echo path mathematically instead: render a tone, feed
//! the processor a delayed attenuated copy as "capture", expect the
//! processor to cancel it.
//!
//! Runs against both backends: `aec` (WebRTC APM, Unix) and `aec-rust`
//! (the port Windows ships, which compiles on any host).

#![cfg(any(feature = "aec", feature = "aec-rust"))]

use std::f32::consts::PI;

use parley_client::aec::{self, FRAME_SAMPLES};

#[test]
fn frame_assembler_chunks_arbitrary_lengths() {
    let mut asm = aec::FrameAssembler::new();
    asm.push(&[0.0; 100]);
    asm.push(&[1.0; 500]); // 600 buffered
    let f1 = asm.pop_frame().unwrap(); // 480: 100 zeros + 380 ones
    assert_eq!(f1.len(), FRAME_SAMPLES);
    assert!(f1[..100].iter().all(|&s| s == 0.0));
    assert!(f1[100..].iter().all(|&s| s == 1.0));
    assert!(asm.pop_frame().is_none()); // 120 left — not a full frame
    asm.push(&[1.0; 360]);
    let f2 = asm.pop_frame().unwrap(); // 480 ones
    assert!(f2.iter().all(|&s| s == 1.0));
    assert!(asm.pop_frame().is_none());
}

#[test]
fn synthetic_echo_is_cancelled() {
    let ap = aec::new_processor().expect("processor init");

    let delay_frames = 6; // 60 ms synthetic echo path
    let mut history: Vec<Vec<f32>> = vec![vec![0.0; FRAME_SAMPLES]; delay_frames];
    let warmup = 150;
    let measure = 100;

    let (mut in_energy, mut out_energy) = (0.0f32, 0.0f32);
    for i in 0..(warmup + measure) {
        // Render: 440 Hz tone.
        let render: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|n| {
                let t = (i * FRAME_SAMPLES + n) as f32 / 48_000.0;
                0.5 * (2.0 * PI * 440.0 * t).sin()
            })
            .collect();
        // Capture: the oldest history frame, attenuated — pure echo.
        let mut capture = history.remove(0);
        for s in capture.iter_mut() {
            *s *= 0.7;
        }
        history.push(render.clone());
        let raw_energy: f32 = capture.iter().map(|s| s * s).sum();

        let mut carry = Vec::new();
        aec::analyze_render(&ap, &mut carry, &render);
        let mut asm = aec::FrameAssembler::new();
        let mut processed = Vec::new();
        aec::process_capture(&ap, &mut asm, &capture, &mut processed);
        assert_eq!(processed.len(), FRAME_SAMPLES);

        if i >= warmup {
            in_energy += raw_energy;
            out_energy += processed.iter().map(|s| s * s).sum::<f32>();
        }
    }

    let reduction_db = 10.0 * (in_energy / out_energy.max(1e-12)).log10();
    // Not shown on a green run (the harness captures stdout); it is here
    // so a failing assertion has the number next to it, and so the two
    // backends can be compared by running this test with each feature.
    println!("echo reduction: {reduction_db:.1} dB");
    assert!(
        reduction_db > 10.0,
        "echo reduction only {reduction_db:.1} dB after {warmup} frames warmup"
    );
}
