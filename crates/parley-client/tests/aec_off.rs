//! The reduced build has no processor at all: aec.rs is a pass-through,
//! and the one thing that must stay true is that the mic audio reaches the
//! encoder untouched. That is the shape any build without a backend
//! feature takes — `audio-device` alone, as the musl CLI and a quick
//! `cargo build` do — so it is worth its own test rather than trusting
//! that the stub compiles.

#![cfg(all(
    feature = "audio-device",
    not(any(feature = "aec", feature = "aec-rust"))
))]

use parley_client::aec;

#[test]
fn the_stub_filters_nothing() {
    let ap = aec::new_processor().expect("the stub cannot fail");
    let mut asm = aec::FrameAssembler::new();
    let input: Vec<f32> = (0..1000).map(|i| i as f32 / 1000.0).collect();
    let mut out = Vec::new();
    aec::process_capture(&ap, &mut asm, &input, &mut out);
    assert_eq!(
        out, input,
        "captured audio must reach the encoder unchanged"
    );

    let echo = aec::new_echo_canceller().expect("the stub cannot fail");
    let mut carry = Vec::new();
    aec::analyze_render(&echo, &mut carry, &[0.5; aec::FRAME_SAMPLES]);
    assert!(carry.is_empty(), "the stub buffers no far-end reference");
}
