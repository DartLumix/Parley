//! Small, dependency-free microphone automatic gain.
//!
//! This is deliberately not the AEC's AGC: the voice processor leaves AGC
//! off because a fixed, transparent chain is easier to trust. This stage is
//! opt-in and only adjusts the level sent to Opus — it never touches the
//! far-end AEC reference or the played mix.
//!
//! The target is RMS dBFS. Gain moves toward the target with a fast attack
//! (loud input is turned down promptly) and a slower release (quiet speech
//! is brought up without pumping on every syllable), then the chunk is
//! limited so a transient cannot clip. A very quiet input is left alone:
//! there is no useful speech to normalize below the gate, and boosting a
//! noise floor only makes it audible.

/// Lowest gain we will apply, in dB (turn a loud source down).
const MIN_GAIN_DB: f32 = -18.0;
/// Highest gain we will apply, in dB (bring a quiet source up).
const MAX_GAIN_DB: f32 = 18.0;
/// Below this RMS the input is treated as room noise/silence.
const GATE_DBFS: f32 = -55.0;
/// Per-chunk coefficient when the gain must come down.
const ATTACK: f32 = 0.45;
/// Per-chunk coefficient when the gain must come up.
const RELEASE: f32 = 0.08;
/// Leave a little headroom for the encoder and the output limiter.
const LIMIT: f32 = 0.98;

/// A per-capture-stream automatic gain stage. Cheap to construct and move
/// into the real-time callback; `process` allocates nothing.
#[derive(Debug, Clone, Copy)]
pub struct MicAutogain {
    enabled: bool,
    target_dbfs: f32,
    gain_db: f32,
}

impl MicAutogain {
    /// `target_dbfs` is clamped to the UI's range (-36..-6 dBFS). A disabled
    /// instance is a true pass-through: it performs no analysis or limiting.
    pub fn new(enabled: bool, target_dbfs: i32) -> Self {
        Self {
            enabled,
            target_dbfs: (target_dbfs as f32).clamp(-36.0, -6.0),
            gain_db: 0.0,
        }
    }

    /// Apply the gain in place. Input and output are mono f32 samples.
    pub fn process(&mut self, samples: &mut [f32]) {
        if !self.enabled || samples.is_empty() {
            return;
        }

        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        let rms_dbfs = 20.0 * rms.max(1e-9).log10();
        if rms_dbfs > GATE_DBFS {
            let target_gain = (self.target_dbfs - rms_dbfs).clamp(MIN_GAIN_DB, MAX_GAIN_DB);
            let coefficient = if target_gain < self.gain_db {
                ATTACK
            } else {
                RELEASE
            };
            let next_gain = self.gain_db + (target_gain - self.gain_db) * coefficient;

            // Ramp the linear gain across the chunk instead of stepping it
            // at a buffer boundary, which would be an audible click.
            let start = 10f32.powf(self.gain_db / 20.0);
            let end = 10f32.powf(next_gain / 20.0);
            let step = (end - start) / samples.len() as f32;
            let mut gain = start;
            for sample in samples.iter_mut() {
                *sample *= gain;
                gain += step;
            }
            self.gain_db = next_gain;
        }

        // A single loud consonant can overshoot even after the RMS decision.
        // Scaling the whole chunk keeps that linear (no hard clipping).
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if peak > LIMIT {
            let scale = LIMIT / peak;
            for sample in samples.iter_mut() {
                *sample *= scale;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    fn run(mut agc: MicAutogain, amplitude: f32, chunks: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..chunks {
            let mut chunk = vec![amplitude; 960];
            agc.process(&mut chunk);
            out.extend(chunk);
        }
        out
    }

    #[test]
    fn boosts_a_quiet_source_toward_the_target() {
        let input_rms = 0.01; // -40 dBFS
        let out = run(MicAutogain::new(true, -18), input_rms, 80);
        let tail = &out[out.len() - 960 * 20..];
        assert!(
            rms(tail) > input_rms * 3.0,
            "quiet input should be lifted, got {}",
            rms(tail)
        );
    }

    #[test]
    fn turns_a_loud_source_down() {
        let input_rms = 0.5; // -6 dBFS
        let out = run(MicAutogain::new(true, -18), input_rms, 20);
        let tail = &out[out.len() - 960 * 5..];
        assert!(
            rms(tail) < input_rms * 0.6,
            "loud input should be attenuated, got {}",
            rms(tail)
        );
    }

    #[test]
    fn leaves_silence_silent() {
        let out = run(MicAutogain::new(true, -18), 0.0, 4);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn disabled_is_a_true_pass_through() {
        let mut samples = vec![0.8f32; 960];
        MicAutogain::new(false, -18).process(&mut samples);
        assert!(samples.iter().all(|s| *s == 0.8));
    }

    #[test]
    fn target_is_clamped_to_the_ui_range() {
        let agc = MicAutogain::new(true, -200);
        assert_eq!(agc.target_dbfs, -36.0);
        let agc = MicAutogain::new(true, 100);
        assert_eq!(agc.target_dbfs, -6.0);
    }
}
