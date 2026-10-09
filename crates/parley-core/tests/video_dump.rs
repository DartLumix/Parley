#![cfg(feature = "video")]

//! `PARLEY_VIDEO_DUMP`: the receiver's keyframe dump writes exactly the
//! Annex-B bytes that were fed to the decoder, keyframes only, one file
//! per sender.
//!
//! Integration test rather than a unit test on purpose: the dump reads
//! its env var once per process, and the unit tests in
//! `video_receive.rs` already push frames through a hub — in one test
//! binary the "no env var" answer would win the race. This file is its
//! own process, so the env var is set before the hub first looks.
//!
//! Run: cargo test -p parley-core --features video --test video_dump

use std::time::Duration;

use parley_core::video_receive::VideoReceiveHub;
use parley_proto::video::fragment_frame;

/// Media datagrams → payloads, the contract `push_datagram` takes.
fn payloads(dgs: &[Vec<u8>]) -> Vec<Vec<u8>> {
    dgs.iter()
        .map(|dg| parley_proto::media::decode(dg).unwrap().payload.to_vec())
        .collect()
}

/// Push one wire frame (keyframe or not) and let the hub drain it.
fn push_frame(hub: &VideoReceiveHub, sender: u32, frame_id: u32, keyframe: bool, data: &[u8]) {
    for p in payloads(&fragment_frame(frame_id, keyframe, data, 0, 0, sender)) {
        hub.push_datagram(sender, &p);
    }
    // Drains (and decodes) whatever is assembled; the dump happens
    // before the decoder sees the bytes, so decode failures — the
    // synthetic payloads below are not real H.264 — are irrelevant.
    let _ = hub.pop_frame(Duration::from_millis(50));
}

#[test]
fn keyframes_are_dumped_in_arrival_order_and_deltas_are_not() {
    let dir = std::env::temp_dir().join(format!("parley-video-dump-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // SAFETY: single-threaded at this point, and the hub reads it lazily.
    unsafe { std::env::set_var("PARLEY_VIDEO_DUMP", &dir) };

    let hub = VideoReceiveHub::shared();
    // Noisy (multi-fragment) so the dump path is exercised with a frame
    // that took several datagrams to assemble.
    let key0: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let key1: Vec<u8> = (0..3000u32).map(|i| (i % 199) as u8).collect();
    let delta: Vec<u8> = vec![0x41; 100];

    push_frame(&hub, 7, 0, true, &key0);
    push_frame(&hub, 7, 30, true, &key1);
    push_frame(&hub, 7, 31, false, &delta);

    let path = dir.join("sender7.annexb");
    let dumped =
        std::fs::read(&path).unwrap_or_else(|e| panic!("no dump at {}: {e}", path.display()));
    let mut expected = key0.clone();
    expected.extend_from_slice(&key1);
    assert_eq!(
        dumped, expected,
        "dump must be the keyframes' own Annex-B bytes, in order, and nothing else"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
