//! Opus encode/decode wrappers.
//!
//! Two profiles: speech (48 kHz mono, Application::Voip, in-band FEC —
//! the voice channels) and music (48 kHz stereo interleaved,
//! Application::Audio, no FEC — 6d music channels). The media datagram
//! timestamp clock is 48 kHz samples per channel, matching both.
use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Bitrate, Channels, MutSignals, SampleRate, packet::Packet};

use crate::error::Error;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_MS: usize = 20;
/// 960 samples per 20 ms frame at 48 kHz (per channel).
pub const SAMPLES_PER_FRAME: usize = SAMPLE_RATE as usize * FRAME_MS / 1000;
/// Opus packets are small; 1 KiB is generous headroom (speech).
pub const MAX_PACKET_SIZE: usize = 1024;
/// 256 kbps stereo 20 ms ≈ 640 B + header; 1.5 KiB is headroom.
const MAX_PACKET_SIZE_STEREO: usize = 1536;
/// Max samples libopus may write for one packet (120 ms @ 48 kHz),
/// PER CHANNEL.
const DECODE_BUFFER: usize = 5760;

pub struct OpusEncoder {
    encoder: Encoder,
    packet: Vec<u8>,
    /// Samples (interleaved, all channels) expected per frame:
    /// 960 mono / 1920 stereo.
    frame_samples: usize,
}

impl OpusEncoder {
    /// Speech profile: mono Voip, 64_000 default, in-band FEC on.
    pub fn new(bitrate_bps: i32) -> Result<Self, Error> {
        let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .map_err(|e| Error::Codec(e.to_string()))?;
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(bitrate_bps))
            .map_err(|e| Error::Codec(e.to_string()))?;
        // In-band FEC: each packet may carry a low-rate copy of the
        // previous one; the decoder can reconstruct a single lost frame
        // from its successor (see OpusDecoder::decode_fec). The loss
        // percentage steers how aggressively the encoder spends bits on
        // redundancy. Static 15% for v1; congestion-adaptive is 4d.
        encoder
            .set_inband_fec(true)
            .map_err(|e| Error::Codec(e.to_string()))?;
        encoder
            .set_packet_loss_perc(15)
            .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self {
            encoder,
            packet: vec![0u8; MAX_PACKET_SIZE],
            frame_samples: SAMPLES_PER_FRAME,
        })
    }

    /// Music profile (6d): stereo, fidelity-tuned, NO in-band FEC —
    /// music channels favor transparency, conceal loss via PLC, and
    /// don't run the 4d ladder.
    pub fn new_music(bitrate_bps: i32) -> Result<Self, Error> {
        let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
            .map_err(|e| Error::Codec(e.to_string()))?;
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(bitrate_bps))
            .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self {
            encoder,
            packet: vec![0u8; MAX_PACKET_SIZE_STEREO],
            frame_samples: SAMPLES_PER_FRAME * 2,
        })
    }

    /// Encode exactly one frame (frame_samples interleaved i16 samples).
    pub fn encode(&mut self, pcm: &[i16]) -> Result<&[u8], Error> {
        if pcm.len() != self.frame_samples {
            return Err(Error::Codec(format!(
                "expected {} samples, got {}",
                self.frame_samples,
                pcm.len()
            )));
        }
        let n = self
            .encoder
            .encode(pcm, &mut self.packet)
            .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(&self.packet[..n])
    }

    /// Set the expected packet loss percentage (0–100). Higher values
    /// make the encoder spend more bits on FEC redundancy.
    /// Only meaningful when in-band FEC is enabled (speech profile).
    pub fn set_loss_hint(&mut self, perc: u8) {
        let _ = self.encoder.set_packet_loss_perc(perc);
    }

    /// 4d: runtime bitrate change. Cheap — libopus reconfigures in place.
    /// Driven by the client's loss-aware profile switcher.
    pub fn set_bitrate(&mut self, bps: i32) {
        let _ = self.encoder.set_bitrate(Bitrate::BitsPerSecond(bps));
    }
}

pub struct OpusDecoder {
    decoder: Decoder,
    pcm: Vec<i16>,
    /// 1 (speech) or 2 (music, interleaved).
    channels: usize,
}

impl OpusDecoder {
    pub fn new() -> Result<Self, Error> {
        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono)
            .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self {
            decoder,
            pcm: vec![0i16; DECODE_BUFFER],
            channels: 1,
        })
    }

    /// Stereo decoder (6d): decodes interleaved L/R.
    pub fn new_stereo() -> Result<Self, Error> {
        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo)
            .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self {
            decoder,
            pcm: vec![0i16; DECODE_BUFFER * 2],
            channels: 2,
        })
    }

    /// Decode one Opus packet. Returns the decoded interleaved i16
    /// samples. libopus's decode length is PER CHANNEL — stereo
    /// produces n*2 samples (guarded by the stereo roundtrip test).
    pub fn decode(&mut self, packet: &[u8]) -> Result<&[i16], Error> {
        let pkt = Packet::try_from(packet).map_err(|e| Error::Codec(e.to_string()))?;
        let n = self
            .decoder
            .decode(
                Some(pkt),
                MutSignals::try_from(&mut self.pcm[..]).map_err(|e| Error::Codec(e.to_string()))?,
                false,
            )
            .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(&self.pcm[..n * self.channels])
    }

    /// Reconstruct the previous (lost) packet's audio from this one's
    /// embedded FEC. Feed this the packet that FOLLOWS the loss.
    /// Speech profile only — music packets carry no FEC.
    pub fn decode_fec(&mut self, packet: &[u8]) -> Result<&[i16], Error> {
        let pkt = Packet::try_from(packet).map_err(|e| Error::Codec(e.to_string()))?;
        let n = self
            .decoder
            .decode(
                Some(pkt),
                MutSignals::try_from(&mut self.pcm[..]).map_err(|e| Error::Codec(e.to_string()))?,
                true,
            )
            .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(&self.pcm[..n * self.channels])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_pcm(total: usize, freq: f64) -> Vec<i16> {
        (0..total)
            .map(|i| {
                let t = i as f64 / SAMPLE_RATE as f64;
                (0.3 * f64::sin(2.0 * std::f64::consts::PI * freq * t) * i16::MAX as f64) as i16
            })
            .collect()
    }

    fn best_correlation(a: &[i16], b: &[i16]) -> f64 {
        // Pearson correlation at the best lag — Opus has ~6.5 ms lookahead,
        // so decoded audio is shifted relative to the input.
        let n = a.len().min(b.len());
        let mut best = f64::NEG_INFINITY;
        for lag in 0..600 {
            let (mut dot, mut sa, mut sb) = (0.0, 0.0, 0.0);
            for i in 0..(n - lag) {
                let x = a[i] as f64;
                let y = b[i + lag] as f64;
                dot += x * y;
                sa += x * x;
                sb += y * y;
            }
            let denom = (sa * sb).sqrt();
            if denom > 0.0 {
                best = best.max(dot / denom);
            }
        }
        best
    }

    #[test]
    fn roundtrip_preserves_signal() {
        let original = sine_pcm(5 * SAMPLES_PER_FRAME, 440.0);
        let mut enc = OpusEncoder::new(64_000).unwrap();
        let mut dec = OpusDecoder::new().unwrap();

        let mut decoded: Vec<i16> = Vec::new();
        for frame in original.chunks(SAMPLES_PER_FRAME) {
            let packet = enc.encode(frame).unwrap();
            assert!(packet.len() < SAMPLES_PER_FRAME * 2, "must compress");
            decoded.extend_from_slice(dec.decode(packet).unwrap());
        }

        assert_eq!(decoded.len(), original.len());
        let corr = best_correlation(&original, &decoded);
        assert!(corr > 0.98, "correlation too low: {corr:.3}");
    }

    #[test]
    fn rejects_wrong_frame_size() {
        let mut enc = OpusEncoder::new(64_000).unwrap();
        assert!(enc.encode(&[0i16; 100]).is_err());
    }

    #[test]
    fn fec_reconstructs_lost_frame() {
        // 24 kbps forces SILK-only mode, where libopus actually embeds
        // in-band FEC (LBRR). At higher bitrates the encoder may choose
        // hybrid/CELT, which carries no FEC. See RFC 6716 §2.1.5.
        // 25% loss hint makes the encoder spend more bits on redundancy.
        let original = sine_pcm(4 * SAMPLES_PER_FRAME, 440.0);
        let mut enc = OpusEncoder::new(24_000).unwrap();
        // Override the default 15% with a higher value for this test
        // (more aggressive FEC embedding).
        enc.set_loss_hint(25);
        let mut dec = OpusDecoder::new().unwrap();

        let packets: Vec<Vec<u8>> = original
            .chunks(SAMPLES_PER_FRAME)
            .map(|f| enc.encode(f).unwrap().to_vec())
            .collect();

        // Drop packet 1; reconstruct frame 1 from packet 2's FEC, then
        // decode packet 2 itself normally.
        dec.decode(&packets[0]).unwrap();
        let mut restored: Vec<i16> = dec.decode_fec(&packets[2]).unwrap().to_vec();
        for p in &packets[2..] {
            restored.extend_from_slice(dec.decode(p).unwrap());
        }

        // FEC reconstruction is a low-bitrate sketch of the lost frame —
        // correlation with the original is modest by design (this is
        // better than silence, not transparent reconstruction).
        let lost = &original[SAMPLES_PER_FRAME..2 * SAMPLES_PER_FRAME];
        let got = &restored[..SAMPLES_PER_FRAME];
        let corr = best_correlation(lost, got);
        assert!(corr > 0.3, "FEC reconstruction too weak: {corr:.3}");
    }

    #[test]
    fn stereo_music_roundtrip_preserves_channels() {
        // 440 Hz L / 880 Hz R through the music profile (256 kbps,
        // Application::Audio, stereo). This is also the guard on the
        // audiopus decode-length semantics: if decode returned TOTAL
        // samples instead of PER-CHANNEL, the length assert below
        // fails before the correlations even run.
        let total = 5 * SAMPLES_PER_FRAME;
        let mut stereo = Vec::with_capacity(total * 2);
        for i in 0..total {
            let t = i as f64 / SAMPLE_RATE as f64;
            let l =
                (0.3 * f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * i16::MAX as f64) as i16;
            let r =
                (0.3 * f64::sin(2.0 * std::f64::consts::PI * 880.0 * t) * i16::MAX as f64) as i16;
            stereo.push(l);
            stereo.push(r);
        }

        let mut enc = OpusEncoder::new_music(256_000).unwrap();
        let mut dec = OpusDecoder::new_stereo().unwrap();

        let mut decoded: Vec<i16> = Vec::new();
        for frame in stereo.chunks(SAMPLES_PER_FRAME * 2) {
            let packet = enc.encode(frame).unwrap();
            assert!(packet.len() < MAX_PACKET_SIZE_STEREO, "must compress");
            decoded.extend_from_slice(dec.decode(packet).unwrap());
        }

        assert_eq!(decoded.len(), stereo.len());
        let l_orig: Vec<i16> = stereo.iter().step_by(2).copied().collect();
        let r_orig: Vec<i16> = stereo.iter().skip(1).step_by(2).copied().collect();
        let l: Vec<i16> = decoded.iter().step_by(2).copied().collect();
        let r: Vec<i16> = decoded.iter().skip(1).step_by(2).copied().collect();
        assert!(
            best_correlation(&l_orig, &l) > 0.9,
            "left must survive as left: {:.3}",
            best_correlation(&l_orig, &l)
        );
        assert!(
            best_correlation(&r_orig, &r) > 0.9,
            "right must survive as right: {:.3}",
            best_correlation(&r_orig, &r)
        );
        // Separation: a swap fails the two asserts above; a mono sum
        // fails them too; this catches a duplicated-single-channel.
        assert!(
            best_correlation(&l, &r_orig) < 0.5,
            "channels must stay separate: {:.3}",
            best_correlation(&l, &r_orig)
        );
    }
}
