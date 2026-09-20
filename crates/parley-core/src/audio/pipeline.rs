//! The voice pipeline: mic→Opus→datagram and datagram→Opus→speakers,
//! minus the device I/O. The client (and only the client) feeds it
//! samples and datagrams.
//!
//! Two profiles (6d): speech (mono, FEC, ladder-driven bitrate) and
//! music (stereo interleaved, no FEC, fixed 256 kbps). Same seq/
//! timestamp discipline; timestamps advance 960 per 20 ms frame in
//! BOTH profiles (per-channel sample clock).

use crate::audio::codec::{OpusDecoder, OpusEncoder, SAMPLES_PER_FRAME};
use crate::audio::jitter::{JitterBuffer, PopOutcome};
use crate::error::Error;

pub struct SendPipeline {
    /// Accumulates captured interleaved i16 samples until a full frame.
    frame: Vec<i16>,
    /// 960 (speech) or 1920 (music) samples per frame.
    frame_samples: usize,
    encoder: OpusEncoder,
    pub seq: u16,
    pub timestamp: u32,
}

impl SendPipeline {
    fn new_inner(bitrate: i32, music: bool) -> Result<Self, Error> {
        let encoder = if music {
            OpusEncoder::new_music(bitrate)?
        } else {
            OpusEncoder::new(bitrate)?
        };
        let frame_samples = if music {
            SAMPLES_PER_FRAME * 2
        } else {
            SAMPLES_PER_FRAME
        };
        Ok(Self {
            frame: Vec::with_capacity(frame_samples),
            frame_samples,
            encoder,
            seq: 0,
            timestamp: 0,
        })
    }

    /// Speech profile (mono, in-band FEC).
    pub fn new(bitrate: i32) -> Result<Self, Error> {
        Self::new_inner(bitrate, false)
    }

    /// Music profile (6d): stereo interleaved input, 256 kbps typical.
    pub fn new_music(bitrate: i32) -> Result<Self, Error> {
        Self::new_inner(bitrate, true)
    }

    /// 4d: runtime bitrate switch (libopus reconfigures in place).
    pub fn set_bitrate(&mut self, bps: i32) {
        self.encoder.set_bitrate(bps);
    }

    /// 4d: expected-loss hint steering FEC aggressiveness (SILK mode only).
    pub fn set_loss_hint(&mut self, perc: u8) {
        self.encoder.set_loss_hint(perc);
    }

    /// Feed one chunk of interleaved 48 kHz i16 samples (mono: 1 per
    /// sample-clock, music: 2). Returns zero or more (seq, timestamp,
    /// packet) tuples ready for datagrams.
    pub fn push_pcm(
        &mut self,
        pcm: &[i16],
        out: &mut Vec<(u16, u32, Vec<u8>)>,
    ) -> Result<(), Error> {
        self.frame.extend_from_slice(pcm);
        while self.frame.len() >= self.frame_samples {
            let packet = self.encoder.encode(&self.frame[..self.frame_samples])?;
            out.push((self.seq, self.timestamp, packet.to_vec()));
            self.seq = self.seq.wrapping_add(1);
            // 20 ms per frame regardless of channel count: the
            // timestamp clock is per-channel samples.
            self.timestamp = self.timestamp.wrapping_add(SAMPLES_PER_FRAME as u32);
            self.frame.drain(..self.frame_samples);
        }
        Ok(())
    }
}

/// Per-sender receive pipeline: jitter → decode → f32 out.
/// Output is interleaved (mono senders produce mono samples; stereo
/// senders produce L/R pairs — the caller normalizes).
pub struct RecvPipeline {
    jitter: JitterBuffer,
    decoder: OpusDecoder,
    pub sender_id: u32,
    /// Frames concealed (gap declared) since the last gap_rate_permille()
    /// reset — the 4d loss signal.
    pub gaps: u64,
    /// Frames decoded since the last gap_rate_permille() reset.
    pub frames: u64,
}

impl RecvPipeline {
    pub fn new(sender_id: u32) -> Result<Self, Error> {
        Ok(Self {
            jitter: JitterBuffer::new(),
            decoder: OpusDecoder::new()?,
            sender_id,
            gaps: 0,
            frames: 0,
        })
    }

    /// Switch the decoder between mono and stereo (6d). Called when a
    /// packet's FLAG_STEREO disagrees with the current configuration —
    /// per-packet flag, no negotiated state. Recreating the decoder is
    /// correct: a mode flip mid-stream means a new sender session.
    pub fn set_stereo(&mut self, stereo: bool) -> Result<(), Error> {
        self.decoder = if stereo {
            OpusDecoder::new_stereo()?
        } else {
            OpusDecoder::new()?
        };
        Ok(())
    }

    pub fn push_packet(&mut self, seq: u16, packet: &[u8]) {
        self.jitter.push(seq, packet);
    }

    /// Gap rate over the window since the last call, in thousandths of
    /// frames (e.g. 100 = 10%). Resets the window — callers report it
    /// downstream every reporting period.
    pub fn gap_rate_permille(&mut self) -> u32 {
        let total = self.gaps + self.frames;
        let rate = if total == 0 {
            0
        } else {
            ((self.gaps as f64 / total as f64) * 1000.0).round() as u32
        };
        self.gaps = 0;
        self.frames = 0;
        rate
    }

    /// Pull up to `max_frames` frames of decoded f32 (interleaved for
    /// stereo senders). Appends to `out`. Gaps conceal via in-band FEC
    /// when the successor packet is buffered, plain PLC otherwise.
    pub fn pull_pcm(&mut self, out: &mut Vec<f32>, max_frames: usize) -> Result<(), Error> {
        let mut produced = 0;
        while produced < max_frames {
            match self.jitter.pop() {
                (PopOutcome::Packet, Some(packet)) => {
                    let samples = self.decoder.decode(&packet)?;
                    out.extend(samples.iter().map(|&s| s as f32 / 32768.0));
                    self.frames += 1;
                }
                (PopOutcome::Gap, _) => {
                    self.gaps += 1;
                    // Lost frame. If the immediate successor is buffered,
                    // its in-band FEC reconstructs the lost audio (better
                    // than PLC); the successor itself decodes on the next
                    // pop.
                    if let Some((_, pkt)) = self.jitter.peek() {
                        let samples = match self.decoder.decode_fec(pkt) {
                            Ok(s) => s,
                            // No FEC embedded in that packet: PLC.
                            Err(_) => self.decoder.decode(&[])?,
                        };
                        out.extend(samples.iter().map(|&s| s as f32 / 32768.0));
                    } else {
                        let samples = self.decoder.decode(&[])?;
                        out.extend(samples.iter().map(|&s| s as f32 / 32768.0));
                    }
                }
                (PopOutcome::Empty, _) => break,
                (PopOutcome::Packet, None) => unreachable!(),
            }
            produced += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_pipeline_frames_and_sequences() {
        let mut send = SendPipeline::new(64_000).unwrap();
        // 2.5 frames worth of samples in one chunk
        let pcm: Vec<i16> = (0..(SAMPLES_PER_FRAME * 5 / 2))
            .map(|i| (i as f32 * 0.05).sin() as i16 * 8000)
            .collect();
        let mut packets = Vec::new();
        send.push_pcm(&pcm, &mut packets).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].0, 0);
        assert_eq!(packets[1].0, 1);
        assert_eq!(packets[0].1, 0);
        assert_eq!(packets[1].1, SAMPLES_PER_FRAME as u32);

        // leftover half-frame stays buffered
        let leftover = send.frame.len();
        assert_eq!(leftover, SAMPLES_PER_FRAME / 2);
    }

    #[test]
    fn music_pipeline_frames_interleaved() {
        // 6d: stereo input frames at 1920 samples, same 960-per-frame
        // timestamp clock as speech.
        let mut send = SendPipeline::new_music(256_000).unwrap();
        let pcm: Vec<i16> = (0..(SAMPLES_PER_FRAME * 2 * 5 / 2))
            .map(|i| (i as f32 * 0.03).sin() as i16 * 8000)
            .collect();
        let mut packets = Vec::new();
        send.push_pcm(&pcm, &mut packets).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[1].1, SAMPLES_PER_FRAME as u32);
        assert_eq!(send.frame.len(), SAMPLES_PER_FRAME); // half-frame = 960 stereo samples
    }

    #[test]
    fn recv_pipeline_decodes_in_order() {
        let mut send = SendPipeline::new(64_000).unwrap();
        let pcm: Vec<i16> = vec![1000i16; SAMPLES_PER_FRAME * 3];
        let mut packets = Vec::new();
        send.push_pcm(&pcm, &mut packets).unwrap();
        assert_eq!(packets.len(), 3);

        let mut recv = RecvPipeline::new(7).unwrap();
        for (seq, _ts, packet) in &packets {
            recv.push_packet(*seq, packet);
        }
        let mut out = Vec::new();
        recv.pull_pcm(&mut out, 10).unwrap();
        assert_eq!(out.len(), 3 * SAMPLES_PER_FRAME);
        // Constant input → near-constant output after codec warm-up
        let tail: Vec<f32> = out[SAMPLES_PER_FRAME..].to_vec();
        let spread = tail.iter().cloned().fold(f32::MIN, f32::max)
            - tail.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread < 0.02, "decoded constant signal spread {spread}");
    }
}
