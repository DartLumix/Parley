//! Thin re-export: the mixer lives in parley-core (6.9) — pure logic,
//! device-agnostic, shared with the Phase 7 client. This shim keeps
//! every client call site (`mixer::new()`, `mixer::set_gain(...)`,
//! `mixer::mix(...)` free-function style) unchanged.

pub use parley_core::audio::mixer::{Mixer, SharedMixer};

pub fn new() -> SharedMixer {
    parley_core::audio::mixer::new()
}

pub fn set_gain(mixer: &SharedMixer, user_id: u32, gain: f32) {
    mixer.lock().unwrap().set_gain(user_id, gain);
}

pub fn remove(mixer: &SharedMixer, sender_id: u32) {
    mixer.lock().unwrap().remove(sender_id);
}

pub fn push_packet(
    mixer: &SharedMixer,
    sender_id: u32,
    seq: u16,
    packet: &[u8],
    stereo: bool,
) -> Result<(), String> {
    mixer
        .lock()
        .unwrap()
        .push_packet(sender_id, seq, packet, stereo)
}

pub fn gap_rates(mixer: &SharedMixer) -> Vec<(u32, u32)> {
    mixer.lock().unwrap().gap_rates()
}

pub fn mix(mixer: &SharedMixer, n: usize) -> Vec<f32> {
    mixer.lock().unwrap().mix(n)
}
