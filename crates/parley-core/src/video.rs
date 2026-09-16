//! H.264 encode/decode via openh264 (vendored source build).
//!
//! v1 profile: 720p30 screen content (Phase 5b bring-up); 5e moves to
//! 1440p60 + VAAPI. Encoder output is Annex-B (start codes included)
//! via `EncodedBitStream::to_vec()` — layer NALs concatenated — which
//! is exactly what our fragmenter and mpv both consume.

use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{Encoder, EncoderConfig};
use openh264::formats::{YUVBuffer, YUVSource};

use crate::error::Error;

/// v1 send profile: 720p30 screen content.
pub const VIDEO_WIDTH: usize = 1280;
pub const VIDEO_HEIGHT: usize = 720;
pub const VIDEO_FPS: f32 = 30.0;
/// Keyframe every 2 s: bounded loss-recovery window (5a's
/// drop-until-IDR rule means a late or lossy joiner waits at most
/// this long for a decodable picture).
pub const KEYFRAME_INTERVAL_FRAMES: u32 = 60;
/// Screen content at 720p30 holds up well around 2.5 Mbps.
pub const VIDEO_BITRATE_BPS: u32 = 2_500_000;

/// Wraps the openh264 encoder. One per sharing sender.
pub struct VideoEncoder {
    encoder: Encoder,
    width: usize,
    height: usize,
}

impl VideoEncoder {
    pub fn new(width: usize, height: usize) -> Result<Self, Error> {
        let config = EncoderConfig::new()
            .bitrate(openh264::encoder::BitRate::from_bps(VIDEO_BITRATE_BPS))
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(VIDEO_FPS))
            .usage_type(openh264::encoder::UsageType::ScreenContentRealTime)
            .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(
                KEYFRAME_INTERVAL_FRAMES,
            ))
            .adaptive_quantization(false) // unsupported for screen content
            .background_detection(false) // unsupported for screen content
            .num_threads(0); // auto // auto
        let encoder = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self {
            encoder,
            width,
            height,
        })
    }

    /// Encode one BGRA frame. Returns (Annex-B frame bytes, is_keyframe).
    ///
    /// `bgra` must be `width * height * 4` bytes (row-major, no stride).
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        let src = openh264::formats::BgraSliceU8::new(bgra, (self.width, self.height));
        let yuv = YUVBuffer::from_bgra8_source(src);
        let stream = self
            .encoder
            .encode(&yuv)
            .map_err(|e| Error::Codec(e.to_string()))?;
        let keyframe = matches!(
            stream.frame_type(),
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        Ok((stream.to_vec(), keyframe))
    }

    /// Request an IDR on the next encode (keyframe-on-join for late
    /// viewers, Phase 5e).
    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
}

/// Wraps the openh264 decoder. One per receiving sender.
pub struct VideoDecoder {
    decoder: Decoder,
}

impl VideoDecoder {
    pub fn new() -> Result<Self, Error> {
        // DecoderConfig::new() is presumably dimensions-free (decoder
        // learns from SPS); if the API demands them, this is the line
        // to adjust — see test compile feedback.
        let decoder =
            Decoder::with_api_config(openh264::OpenH264API::from_source(), DecoderConfig::new())
                .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self { decoder })
    }

    /// Decode one Annex-B frame (as assembled by video::VideoAssembler
    /// from 5a). Returns Some(RGB8 data) when a picture came out —
    /// H.264 decoders can legitimately emit nothing (non-reference
    /// frames) or delay output; callers treat None as "no frame yet".
    /// Decode one Annex-B frame, returning RGB8 data plus the decoded
    /// dimensions — the SPS decides, not the caller. The viewer needs
    /// these to size its display for sender-negotiated resolutions
    /// (5d: real capture sends whatever the compositor serves).
    pub fn decode_with_dims(
        &mut self,
        frame: &[u8],
    ) -> Result<Option<(Vec<u8>, usize, usize)>, Error> {
        match self
            .decoder
            .decode(frame)
            .map_err(|e| Error::Codec(e.to_string()))?
        {
            Some(yuv) => {
                let (w, h) = yuv.dimensions();
                let mut rgb = vec![0u8; w * h * 3];
                yuv.write_rgb8(&mut rgb);
                Ok(Some((rgb, w, h)))
            }
            None => Ok(None),
        }
    }

    /// Decode one Annex-B frame (as assembled by video::VideoAssembler
    /// from 5a). Returns Some(RGB8 data) when a picture came out —
    /// H.264 decoders can legitimately emit nothing (non-reference
    /// frames) or delay output; callers treat None as "no frame yet".
    pub fn decode(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.decode_with_dims(frame)?.map(|(rgb, _, _)| rgb))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Horizontal gradient + a moving bar, so frames differ and motion
    /// compensation has something to chew on.
    fn test_frame(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut bgra = vec![0u8; w * h * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            let x = i % w;
            let y = i / w;
            let bar_x = ((t * 13) % w) as i32;
            let in_bar = (x as i32 - bar_x).abs() < 24 && y % 4 == 0;
            px[0] = (x * 255 / w) as u8; // B
            px[1] = (y * 255 / h) as u8; // G
            px[2] = if in_bar { 255 } else { 64 }; // R
            px[3] = 255; // A
        }
        bgra
    }

    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (VIDEO_WIDTH, VIDEO_HEIGHT);
        let mut enc = VideoEncoder::new(w, h).unwrap();
        let mut dec = VideoDecoder::new().unwrap();

        let mut first_keyframe: Option<Vec<u8>> = None;
        for t in 0..6 {
            let bgra = test_frame(w, h, t);
            let (frame, keyframe) = enc.encode_bgra(&bgra).unwrap();
            if t == 0 {
                assert!(keyframe, "first frame must be a keyframe");
                first_keyframe = Some(frame);
            } else {
                assert!(
                    !keyframe,
                    "subsequent frames within the intra period must not be keyframes"
                );
            }
        }

        // Decode the keyframe alone — must produce a full picture.
        let rgb = dec
            .decode(&first_keyframe.unwrap())
            .unwrap()
            .expect("keyframe must decode to a frame");
        assert_eq!(rgb.len(), w * h * 3);

        // Sanity: center pixel tracks the gradients (R from y=360 → 64,
        // B from x=640 → 127). The pair also catches channel swaps.
        let cx = w / 2;
        let cy = h / 2;
        let idx = (cy * w + cx) * 3;
        let r = rgb[idx];
        let b = rgb[idx + 2];
        assert!(
            (r as i32 - 64).abs() <= 8,
            "center red off: {r} (expected ~64)"
        );
        assert!(
            (b as i32 - 127).abs() <= 8,
            "center blue off: {b} (expected ~127)"
        );
    }

    #[test]
    fn forced_keyframe_is_idr() {
        let (w, h) = (320, 240);
        let mut enc = VideoEncoder::new(w, h).unwrap();
        let mut dec = VideoDecoder::new().unwrap();

        // Prime the decoder with an initial keyframe.
        let (f0, k0) = enc.encode_bgra(&test_frame(w, h, 0)).unwrap();
        assert!(k0);
        dec.decode(&f0).unwrap();

        // Delta, then a forced keyframe mid-stream.
        let _ = enc.encode_bgra(&test_frame(w, h, 1)).unwrap();
        enc.force_keyframe();
        let (f2, k2) = enc.encode_bgra(&test_frame(w, h, 2)).unwrap();
        assert!(k2, "force_intra_frame must produce a keyframe");

        // The forced keyframe must decode standalone (late-joiner path:
        // fresh decoder, no prior state).
        let mut fresh = VideoDecoder::new().unwrap();
        let rgb = fresh
            .decode(&f2)
            .unwrap()
            .expect("forced IDR must decode standalone");
        assert_eq!(rgb.len(), w * h * 3);
    }

    #[test]
    fn fragmenter_accepts_encoder_output() {
        // The wire path needs openh264's Annex-B output to fragment
        // cleanly — mostly a smoke test binding 5a and 5b together,
        // which lives in parley-proto. Here we assert the encoder's
        // output starts with an Annex-B start code.
        let (w, h) = (320, 240);
        let mut enc = VideoEncoder::new(w, h).unwrap();
        let (frame, _k) = enc.encode_bgra(&test_frame(w, h, 0)).unwrap();
        assert!(
            frame.starts_with(&[0, 0, 0, 1]) || frame.starts_with(&[0, 0, 1]),
            "encoder output must be Annex-B (start code first), got {:?}",
            &frame[..4.min(frame.len())]
        );
    }
}
