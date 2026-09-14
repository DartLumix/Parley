//! Per-sender receive state, per-user output volume, and the final mix.
//! Pure logic, no cpal.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use parley_core::audio::SAMPLES_PER_FRAME;
use parley_core::audio::pipeline::RecvPipeline;

pub struct Voice {
    pipeline: RecvPipeline,
    /// Decoded samples not yet consumed by the output callback.
    pending: VecDeque<f32>,
}

impl Voice {
    fn new(sender_id: u32) -> Result<Self, String> {
        Ok(Self {
            pipeline: RecvPipeline::new(sender_id).map_err(|e| e.to_string())?,
            pending: VecDeque::new(),
        })
    }

    fn push_packet(&mut self, seq: u16, packet: &[u8]) {
        self.pipeline.push_packet(seq, packet);
    }
}

pub struct Mixer {
    voices: HashMap<u32, Voice>,
    /// Per-user output gain (Phase 4b); unknown ids play at 1.0.
    gains: HashMap<u32, f32>,
}

pub type SharedMixer = Arc<Mutex<Mixer>>;

pub fn new() -> SharedMixer {
    Arc::new(Mutex::new(Mixer {
        voices: HashMap::new(),
        gains: HashMap::new(),
    }))
}

/// Set a user's output gain; 0.0 mutes them. Survives them leaving/rejoining.
pub fn set_gain(mixer: &SharedMixer, user_id: u32, gain: f32) {
    mixer.lock().unwrap().gains.insert(user_id, gain);
}

/// Drop a departed sender's decoder state (their gain setting stays).
pub fn remove(mixer: &SharedMixer, sender_id: u32) {
    mixer.lock().unwrap().voices.remove(&sender_id);
}

/// Feed one received packet, creating the sender's pipeline on first sight.
pub fn push_packet(
    mixer: &SharedMixer,
    sender_id: u32,
    seq: u16,
    packet: &[u8],
) -> Result<(), String> {
    let mut m = mixer.lock().unwrap();
    if let Some(v) = m.voices.get_mut(&sender_id) {
        v.push_packet(seq, packet);
        return Ok(());
    }
    let mut v = Voice::new(sender_id)?;
    v.push_packet(seq, packet);
    m.voices.insert(sender_id, v);
    Ok(())
}

/// 4d: collect per-sender gap rates since the last call (resets each
/// sender's window). Returns (sender_id, permille) pairs.
pub fn gap_rates(mixer: &SharedMixer) -> Vec<(u32, u32)> {
    mixer
        .lock()
        .unwrap()
        .voices
        .iter_mut()
        .map(|(&id, v)| (id, v.pipeline.gap_rate_permille()))
        .collect()
}

/// Pull from every sender, apply per-user gains, sum-mix, clamp. `n` =
/// frames (per-channel samples) the output callback needs right now.
pub fn mix(mixer: &SharedMixer, n: usize) -> Vec<f32> {
    let mut m = mixer.lock().unwrap();
    for v in m.voices.values_mut() {
        // Pull only what we're short of — pulling a fixed frame count
        // every callback would over-drain the jitter buffer and make
        // delay grow without bound.
        let need = n.saturating_sub(v.pending.len());
        if need == 0 {
            continue;
        }
        let mut tmp = Vec::new();
        // Codec error here = one bad packet; pull_pcm's PLC path keeps
        // the stream moving, so it stays quiet.
        let _ = v
            .pipeline
            .pull_pcm(&mut tmp, need.div_ceil(SAMPLES_PER_FRAME));
        v.pending.extend(tmp);
    }
    let mut mix = vec![0.0f32; n];
    for (&id, v) in m.voices.iter() {
        let gain = m.gains.get(&id).copied().unwrap_or(1.0);
        if gain == 0.0 {
            continue; // muted — pending still drains below, timing stays honest
        }
        for (i, out) in mix.iter_mut().enumerate() {
            if let Some(&s) = v.pending.get(i) {
                *out += s * gain;
            }
        }
    }
    for v in m.voices.values_mut() {
        let drop = v.pending.len().min(n);
        v.pending.drain(..drop);
    }
    for s in &mut mix {
        *s = s.clamp(-1.0, 1.0);
    }
    mix
}
