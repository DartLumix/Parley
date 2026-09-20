//! Per-sender receive state, per-user output volume, and the final mix.
//! Pure logic, no cpal.
//!
//! 6d: the mix is interleaved STEREO. Mono voices are duplicated into
//! both channels at pull time, so every voice's pending buffer holds
//! interleaved pairs and the sum loop is uniform. A voice's mono/stereo
//! configuration follows the FLAG_STEREO bit on each received packet.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use parley_core::audio::pipeline::RecvPipeline;

pub struct Voice {
    pipeline: RecvPipeline,
    /// Decoded samples not yet consumed by the output callback —
    /// ALWAYS interleaved stereo pairs (mono sources duplicated).
    pending: VecDeque<f32>,
    /// Current decoder configuration, from the last packet's
    /// FLAG_STEREO.
    stereo: bool,
}

impl Voice {
    fn new(sender_id: u32, stereo: bool) -> Result<Self, String> {
        let mut pipeline = RecvPipeline::new(sender_id).map_err(|e| e.to_string())?;
        pipeline.set_stereo(stereo).map_err(|e| e.to_string())?;
        Ok(Self {
            pipeline,
            pending: VecDeque::new(),
            stereo,
        })
    }

    fn push_packet(&mut self, seq: u16, packet: &[u8], stereo: bool) -> Result<(), String> {
        if self.stereo != stereo {
            // Mode flip mid-stream = a new sender session; the decoder
            // restarts clean. Stale pending is left to drain.
            self.pipeline
                .set_stereo(stereo)
                .map_err(|e| e.to_string())?;
            self.stereo = stereo;
        }
        self.pipeline.push_packet(seq, packet);
        Ok(())
    }
}

pub struct Mixer {
    voices: HashMap<u32, Voice>,
    /// Per-user output gain (Phase 4b); unknown ids play at 1.0.
    /// One gain per user — applied to both channels of a stereo voice.
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

/// Feed one received packet, creating the sender's pipeline on first
/// sight. `stereo` is the packet's FLAG_STEREO — per-packet truth.
pub fn push_packet(
    mixer: &SharedMixer,
    sender_id: u32,
    seq: u16,
    packet: &[u8],
    stereo: bool,
) -> Result<(), String> {
    let mut m = mixer.lock().unwrap();
    if let Some(v) = m.voices.get_mut(&sender_id) {
        v.push_packet(seq, packet, stereo)?;
        return Ok(());
    }
    let mut v = Voice::new(sender_id, stereo)?;
    v.push_packet(seq, packet, stereo)?;
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

/// Pull from every sender, apply per-user gains, sum-mix, clamp.
/// `n` = FRAMES the output callback needs; returns interleaved stereo
/// (2n samples). Mono voices are duplicated into both channels.
pub fn mix(mixer: &SharedMixer, n: usize) -> Vec<f32> {
    let mut m = mixer.lock().unwrap();
    for v in m.voices.values_mut() {
        // Pull only what we're short of — pulling a fixed frame count
        // every callback would over-drain the jitter buffer and make
        // delay grow without bound. Pending is interleaved pairs:
        // len/2 = frames buffered.
        let need_frames = n.saturating_sub(v.pending.len() / 2);
        if need_frames == 0 {
            continue;
        }
        let mut tmp = Vec::new();
        // Codec error here = one bad packet; pull_pcm's PLC path keeps
        // the stream moving, so it stays quiet.
        let _ = v.pipeline.pull_pcm(&mut tmp, need_frames);
        if v.stereo {
            v.pending.extend(tmp);
        } else {
            // Mono → duplicate into both channels.
            for s in tmp {
                v.pending.push_back(s);
                v.pending.push_back(s);
            }
        }
    }
    let mut mix = vec![0.0f32; n * 2];
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
        let drop = v.pending.len().min(n * 2);
        v.pending.drain(..drop);
    }
    for s in &mut mix {
        *s = s.clamp(-1.0, 1.0);
    }
    mix
}
