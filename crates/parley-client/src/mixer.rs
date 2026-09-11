//! Per-sender receive state and the output mix. Pure logic, no cpal.

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
    pub fn new(sender_id: u32) -> Result<Self, String> {
        Ok(Self {
            pipeline: RecvPipeline::new(sender_id).map_err(|e| e.to_string())?,
            pending: VecDeque::new(),
        })
    }

    pub fn push_packet(&mut self, seq: u16, packet: &[u8]) {
        self.pipeline.push_packet(seq, packet);
    }
}

pub type VoicesMap = Arc<Mutex<HashMap<u32, Voice>>>;

pub fn new_voices() -> VoicesMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Drop a departed sender's decoder state.
pub fn remove(voices: &VoicesMap, sender_id: u32) {
    voices.lock().unwrap().remove(&sender_id);
}

/// Pull from every sender, sum-mix, clamp. `n` = frames (per-channel
/// samples) the output callback needs right now.
pub fn mix(voices: &VoicesMap, n: usize) -> Vec<f32> {
    let mut voices = voices.lock().unwrap();
    for v in voices.values_mut() {
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
    for v in voices.values() {
        for (i, m) in mix.iter_mut().enumerate() {
            if let Some(&s) = v.pending.get(i) {
                *m += s;
            }
        }
    }
    for v in voices.values_mut() {
        let drop = v.pending.len().min(n);
        v.pending.drain(..drop);
    }
    for s in &mut mix {
        *s = s.clamp(-1.0, 1.0);
    }
    mix
}
