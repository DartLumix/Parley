//! Per-sender receive mixing: jitter/decode pipelines per voice,
//! per-user output gains, interleaved-stereo sum-mix, clamp.
//! Pure logic — no device I/O, no cpal. Any audio backend drives it:
//! the test client's cpal render callback today, a Flutter platform
//! audio stack in Phase 7.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::audio::SAMPLES_PER_FRAME;
use crate::audio::pipeline::RecvPipeline;

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
    /// Per-user output gain; unknown ids play at 1.0. One gain per
    /// user — applied to both channels of a stereo voice.
    gains: HashMap<u32, f32>,
}

/// Shared mixer handle.
pub type SharedMixer = Arc<Mutex<Mixer>>;

pub fn new() -> SharedMixer {
    Arc::new(Mutex::new(Mixer {
        voices: HashMap::new(),
        gains: HashMap::new(),
    }))
}

impl Mixer {
    /// Set a user's output gain; 0.0 mutes them. Survives them
    /// leaving/rejoining.
    pub fn set_gain(&mut self, user_id: u32, gain: f32) {
        self.gains.insert(user_id, gain);
    }

    /// Drop a departed sender's decoder state (their gain setting stays).
    pub fn remove(&mut self, sender_id: u32) {
        self.voices.remove(&sender_id);
    }

    /// Feed one received packet, creating the sender's pipeline on first
    /// sight. `stereo` is the packet's FLAG_STEREO — per-packet truth.
    pub fn push_packet(
        &mut self,
        sender_id: u32,
        seq: u16,
        packet: &[u8],
        stereo: bool,
    ) -> Result<(), String> {
        if let Some(v) = self.voices.get_mut(&sender_id) {
            v.push_packet(seq, packet, stereo)
        } else {
            let mut v = Voice::new(sender_id, stereo)?;
            v.push_packet(seq, packet, stereo)?;
            self.voices.insert(sender_id, v);
            Ok(())
        }
    }

    /// Collect per-sender gap rates since the last call (resets each
    /// sender's window). Returns (sender_id, permille) pairs.
    pub fn gap_rates(&mut self) -> Vec<(u32, u32)> {
        self.voices
            .iter_mut()
            .map(|(&id, v)| (id, v.pipeline.gap_rate_permille()))
            .collect()
    }

    /// Pull from every sender, apply per-user gains, sum-mix, clamp.
    /// `n` = AUDIO frames the output callback needs (one per channel;
    /// returns 2n interleaved samples). Mono voices are duplicated into
    /// both channels.
    ///
    /// UNIT CONVERSION — load-bearing: pull_pcm counts 20 ms CODEC
    /// frames (SAMPLES_PER_FRAME audio frames each). The div_ceil
    /// below converts audio-frames-needed → codec-frames-to-pull.
    /// Dropping it makes every callback request ~960× more codec
    /// frames than intended, over-draining the jitter buffer (the
    /// 6.9 promotion dropped it once; the mixer unit tests caught it).
    pub fn mix(&mut self, n: usize) -> Vec<f32> {
        for v in self.voices.values_mut() {
            // Pull only what we're short of — pending is interleaved
            // pairs: len/2 = audio frames buffered.
            let need_frames = n.saturating_sub(v.pending.len() / 2);
            if need_frames == 0 {
                continue;
            }
            let mut tmp = Vec::new();
            // Codec error here = one bad packet; pull_pcm's PLC path
            // keeps the stream moving, so it stays quiet.
            let _ = v
                .pipeline
                .pull_pcm(&mut tmp, need_frames.div_ceil(SAMPLES_PER_FRAME));
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
        for (&id, v) in self.voices.iter() {
            let gain = self.gains.get(&id).copied().unwrap_or(1.0);
            if gain == 0.0 {
                continue; // muted — pending still drains below, timing stays honest
            }
            for (i, out) in mix.iter_mut().enumerate() {
                if let Some(&s) = v.pending.get(i) {
                    *out += s * gain;
                }
            }
        }
        for v in self.voices.values_mut() {
            let drop = v.pending.len().min(n * 2);
            v.pending.drain(..drop);
        }
        for s in &mut mix {
            *s = s.clamp(-1.0, 1.0);
        }
        mix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::pipeline::SendPipeline;

    /// Sine PCM frames via the speech (mono) pipeline, seq starting at
    /// `seq0`. Returns (seq, ts, packet) triples.
    fn speech_packets_from(seq0: u16, frames: usize) -> Vec<(u16, u32, Vec<u8>)> {
        let mut send = SendPipeline::new(64_000).unwrap();
        let pcm: Vec<i16> = (0..frames * SAMPLES_PER_FRAME)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
            .collect();
        let mut out = Vec::new();
        send.push_pcm(&pcm, &mut out).unwrap();
        for (i, p) in out.iter_mut().enumerate() {
            p.0 = seq0 + i as u16;
        }
        out
    }

    fn speech_packets(frames: usize) -> Vec<(u16, u32, Vec<u8>)> {
        speech_packets_from(0, frames)
    }

    /// Stereo with GENUINELY different channels (440 Hz L / 880 Hz R —
    /// a near-mono input cannot prove non-duplication), via the music
    /// pipeline, seq starting at `seq0`.
    fn music_packets_from(seq0: u16, frames: usize) -> Vec<(u16, u32, Vec<u8>)> {
        let mut send = SendPipeline::new_music(256_000).unwrap();
        let mut pcm = Vec::with_capacity(frames * SAMPLES_PER_FRAME * 2);
        for i in 0..frames * SAMPLES_PER_FRAME {
            let t = i as f64 / 48_000.0;
            let l = (0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 32767.0) as i16;
            let r = (0.3 * (2.0 * std::f64::consts::PI * 880.0 * t).sin() * 32767.0) as i16;
            pcm.push(l);
            pcm.push(r);
        }
        let mut out = Vec::new();
        send.push_pcm(&pcm, &mut out).unwrap();
        for (i, p) in out.iter_mut().enumerate() {
            p.0 = seq0 + i as u16;
        }
        out
    }

    fn music_packets(frames: usize) -> Vec<(u16, u32, Vec<u8>)> {
        music_packets_from(0, frames)
    }

    /// Click-burst frames (the 3f click waveform: 1.4 kHz, 5 ms, loud)
    /// — the ONE signal class with end-to-end level evidence: netem
    /// runs show 0.95-amplitude clicks detected at 0.8–0.98 after
    /// encode→decode→mix. Constants are unusable for level asserts
    /// (pure DC — Opus high-passes them to ≈0, HPF ring-down only);
    /// mid-band sine level is observationally unreliable (see ledger).
    /// Margins below tolerate decode gain anywhere in ~0.2–1.0.
    fn burst_packets(seq0: u16, frames: usize) -> Vec<(u16, u32, Vec<u8>)> {
        let mut send = SendPipeline::new(64_000).unwrap();
        let mut pcm = Vec::with_capacity(frames * SAMPLES_PER_FRAME);
        for f in 0..frames {
            for i in 0..SAMPLES_PER_FRAME {
                // Burst in the first 5 ms of each frame, silence after.
                let s = if i < 240 {
                    let t = i as f32 / 48_000.0;
                    (0.97 * (2.0 * std::f32::consts::PI * 1_400.0 * t).sin() * 32767.0) as i16
                } else {
                    0
                };
                pcm.push(s);
                let _ = f;
            }
        }
        let mut out = Vec::new();
        send.push_pcm(&pcm, &mut out).unwrap();
        for (i, p) in out.iter_mut().enumerate() {
            p.0 = seq0 + i as u16;
        }
        out
    }

    /// Push packets for `sender`, then consume exactly ONE FULL CODEC
    /// frame (SAMPLES_PER_FRAME audio frames) so the decoder is warm —
    /// Opus starts near-silent on a fresh decoder's first frame (the
    /// 3e-era tests slice their tails for exactly this reason); every
    /// real session's decoders are warm, so the tests model that.
    /// NB: mix(SAMPLES_PER_FRAME) burns the whole first codec frame —
    /// mix(1) would burn only 2 samples and leave the cold region in
    /// pending (the first warm-up attempt made exactly that mistake).
    fn warm_up(m: &mut Mixer, sender: u32, packets: &[(u16, u32, Vec<u8>)], stereo: bool) {
        for (seq, _ts, pkt) in packets {
            m.push_packet(sender, *seq, pkt, stereo).unwrap();
        }
        let _ = m.mix(SAMPLES_PER_FRAME);
    }

    #[test]
    fn mono_voice_dupes_to_both_channels() {
        let mixer = new();
        {
            let mut m = mixer.lock().unwrap();
            warm_up(&mut m, 7, &speech_packets(4), false);
        }
        // Pull 2 audio frames (interleaved stereo out) from the warm
        // stream — these come from codec frame 1.
        let mixed = {
            let mut m = mixer.lock().unwrap();
            m.mix(2)
        };
        assert_eq!(mixed.len(), 4);
        // Left == Right for a mono source (allowing codec rounding).
        for pair in mixed.chunks(2) {
            assert!(
                (pair[0] - pair[1]).abs() < 0.02,
                "mono must be duplicated: {} vs {}",
                pair[0],
                pair[1]
            );
        }
        assert!(mixed.iter().any(|&s| s.abs() > 0.01), "expected signal");
    }

    #[test]
    fn stereo_voice_stays_independent() {
        let mixer = new();
        {
            let mut m = mixer.lock().unwrap();
            warm_up(&mut m, 8, &music_packets(4), true);
        }
        let mixed = {
            let mut m = mixer.lock().unwrap();
            m.mix(2)
        };
        assert_eq!(mixed.len(), 4);
        // 440 Hz L vs 880 Hz R: a mono-duplication bug (or a swap,
        // caught by the codec test) shows as L==R; genuinely different
        // channels must differ.
        let pairs_differ = mixed.chunks(2).any(|p| (p[0] - p[1]).abs() > 0.05);
        assert!(
            pairs_differ,
            "stereo voice must not be mono-duplicated: {mixed:?}"
        );
    }

    #[test]
    fn gain_zero_mutes_but_drains() {
        let mixer = new();
        {
            let mut m = mixer.lock().unwrap();
            warm_up(&mut m, 9, &speech_packets(4), false);
            m.set_gain(9, 0.0);
        }
        let mixed = {
            let mut m = mixer.lock().unwrap();
            m.mix(1)
        };
        assert!(
            mixed.iter().all(|&s| s.abs() < 1e-6),
            "muted voice must be silent"
        );
        // Restore gain, push FRESH-seq packets (the jitter buffer's
        // playhead is past seq 0..3 — reusing them would late-drop),
        // mix — the voice must be live and audible.
        {
            let mut m = mixer.lock().unwrap();
            m.set_gain(9, 1.0);
            for (seq, _ts, pkt) in speech_packets_from(10, 2) {
                m.push_packet(9, seq, &pkt, false).unwrap();
            }
        }
        let mixed3 = {
            let mut m = mixer.lock().unwrap();
            m.mix(1)
        };
        assert!(
            mixed3.iter().any(|&s| s.abs() > 0.001),
            "voice survives mute"
        );
    }

    #[test]
    fn two_voices_sum_and_clamp() {
        let mixer = new();
        {
            let mut m = mixer.lock().unwrap();
            warm_up(&mut m, 1, &burst_packets(0, 5), false);
            warm_up(&mut m, 2, &burst_packets(0, 5), false);
            // Both warm; slam both to max gain — the clamp must engage.
            m.set_gain(1, 4.0);
            m.set_gain(2, 4.0);
        }
        // One full codec frame: each frame carries a 240-sample burst;
        // two identical voices at 4x → burst samples land far past ±1
        // for any plausible decode gain, clamping to ±1. Assert by
        // COUNT with wide margins — level-dependent single asserts are
        // what sank the previous three versions of this test.
        let mixed = {
            let mut m = mixer.lock().unwrap();
            m.mix(SAMPLES_PER_FRAME)
        };
        assert_eq!(mixed.len(), SAMPLES_PER_FRAME * 2);
        // Signal present.
        assert!(
            mixed.iter().any(|&s| s.abs() > 0.5),
            "no signal: {:?}",
            &mixed[..8.min(mixed.len())]
        );
        // Clamp bounds every sample.
        assert!(
            mixed.iter().all(|&s| s.abs() <= 1.0),
            "clamp must hold: {:?}",
            &mixed[..8.min(mixed.len())]
        );
        // Saturation on a meaningful count of burst samples.
        let saturated = mixed.iter().filter(|&&s| s.abs() >= 0.999).count();
        assert!(
            saturated >= 60,
            "expected burst saturation, got {saturated} samples of {}",
            mixed.len()
        );
    }

    #[test]
    fn remove_drops_voice_gain_survives() {
        let mixer = new();
        {
            let mut m = mixer.lock().unwrap();
            m.set_gain(5, 0.25);
            warm_up(&mut m, 5, &speech_packets(3), false);
            m.remove(5);
        }
        // No voices → silence.
        let mixed = {
            let mut m = mixer.lock().unwrap();
            m.mix(2)
        };
        assert!(mixed.iter().all(|&s| s.abs() < 1e-6));
        // Voice re-created on next packet (fresh seq after the reset);
        // gain 0.25 still applies structurally (gains map untouched).
        {
            let mut m = mixer.lock().unwrap();
            for (seq, _ts, pkt) in speech_packets_from(0, 2) {
                m.push_packet(5, seq, &pkt, false).unwrap();
            }
        }
        let _ = {
            let mut m = mixer.lock().unwrap();
            m.mix(1)
        }; // must not panic; re-created voice mixes
    }
}
