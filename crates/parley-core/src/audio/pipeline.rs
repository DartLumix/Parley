//! The voice pipeline: mic→Opus→datagram and datagram→Opus→speakers,
//! minus the device I/O. The client (and only the client) feeds it
//! samples and datagrams.

use crate::audio::codec::{OpusDecoder, OpusEncoder, SAMPLES_PER_FRAME};
use crate::audio::jitter::{JitterBuffer, PopOutcome};
use crate::error::Error;

pub struct SendPipeline {
    /// Accumulates resampled 48k i16 samples until a full Opus frame.
    frame: Vec<i16>,
    encoder: OpusEncoder,
    pub seq: u16,
    pub timestamp: u32,
}

impl SendPipeline {
    pub fn new(bitrate: i32) -> Result<Self, Error> {
        Ok(Self {
            frame: Vec::with_capacity(SAMPLES_PER_FRAME),
            encoder: OpusEncoder::new(bitrate)?,
            seq: 0,
            timestamp: 0,
        })
    }

    /// Feed one chunk of 48 kHz mono i16 samples. Returns zero or more
    /// (seq, timestamp, packet) tuples ready for datagrams.
    pub fn push_pcm(
        &mut self,
        pcm: &[i16],
        out: &mut Vec<(u16, u32, Vec<u8>)>,
    ) -> Result<(), Error> {
        self.frame.extend_from_slice(pcm);
        while self.frame.len() >= SAMPLES_PER_FRAME {
            let packet = self.encoder.encode(&self.frame[..SAMPLES_PER_FRAME])?;
            out.push((self.seq, self.timestamp, packet.to_vec()));
            self.seq = self.seq.wrapping_add(1);
            self.timestamp = self.timestamp.wrapping_add(SAMPLES_PER_FRAME as u32);
            self.frame.drain(..SAMPLES_PER_FRAME);
        }
        Ok(())
    }
}

/// Per-sender receive pipeline: jitter → decode → f32 out.
pub struct RecvPipeline {
    jitter: JitterBuffer,
    decoder: OpusDecoder,
    pub sender_id: u32,
}

impl RecvPipeline {
    pub fn new(sender_id: u32) -> Result<Self, Error> {
        Ok(Self {
            jitter: JitterBuffer::new(),
            decoder: OpusDecoder::new()?,
            sender_id,
        })
    }

    pub fn push_packet(&mut self, seq: u16, packet: &[u8]) {
        self.jitter.push(seq, packet);
    }

    /// Pull up to `max_frames` frames of decoded f32 (48 kHz mono).
    /// Appends to `out`. Runs PLC on gaps.
    pub fn pull_pcm(&mut self, out: &mut Vec<f32>, max_frames: usize) -> Result<(), Error> {
        let mut produced = 0;
        while produced < max_frames {
            match self.jitter.pop() {
                (PopOutcome::Packet, Some(packet)) => {
                    let samples = self.decoder.decode(&packet)?;
                    out.extend(samples.iter().map(|&s| s as f32 / 32768.0));
                }
                (PopOutcome::Gap, _) => {
                    // Conceal: decode an empty packet (libopus PLC)
                    let samples = self.decoder.decode(&[])?;
                    out.extend(samples.iter().map(|&s| s as f32 / 32768.0));
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
