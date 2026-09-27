//! H.264 encode/decode via openh264 (vendored source build).
//!
//! 5e architecture: `trait VideoEncoder` is the contract every backend
//! implements; `SoftwareEncoder` (openh264) is the universal fallback;
//! `create_encoder()` selects the best available backend at runtime.
//! Hardware backends (VAAPI, MediaCodec, VideoToolbox) slot in behind
//! the same trait — a client whose encode path can wedge the whole
//! desktop (the polaris VCE kernel hang) is unusable, so software is
//! the default and hardware is opt-in with runtime fallback.
//!
//! v1 profile: 720p30+ screen content. Encoder output is Annex-B
//! (start codes included) via `EncodedBitStream::to_vec()` — exactly
//! what our fragmenter and mpv both consume.

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

/// Codec identity for capability reporting (future-proofing for
/// HEVC/AV1 backends — the media header's flag bits grow one bit per
/// codec, additive, never a rewrite).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    H264,
}

/// The contract every video encoder backend implements. Backends are
/// platform-specific; callers hold `Box<dyn VideoEncoder>` and never
/// know which one they got.
pub trait VideoEncoder: Send {
    /// Encoder identity for capability reporting.
    fn codec(&self) -> CodecId;

    /// Current frame dimensions.
    fn dimensions(&self) -> (usize, usize);

    /// Encode one BGRA frame (`width*height*4` bytes, row-major, no
    /// stride) → (Annex-B bytes, is_keyframe).
    fn encode_bgra(&mut self, bgra: &[u8]) -> Result<(Vec<u8>, bool), Error>;

    /// Request an IDR on the next encode (keyframe-on-join).
    fn force_keyframe(&mut self);

    /// Change target bitrate at runtime (quality ladder). Backends may
    /// clamp; the change takes effect over the next few frames.
    fn set_bitrate(&mut self, bps: u32);
}

/// Construct the best available encoder for this platform. Hardware
/// backends are feature-gated, tried first, and fall back to software
/// at runtime; today software is the only backend (VAAPI parked after
/// the polaris VCE kernel hang — see HANDOFF §8).
pub fn create_encoder(width: usize, height: usize) -> Result<Box<dyn VideoEncoder>, Error> {
    Ok(Box::new(SoftwareEncoder::new(width, height)?))
}

/// The openh264 encoder configuration, shared by construction and
/// bitrate changes (the latter recreates the encoder).
fn encoder_config(bps: u32) -> EncoderConfig {
    EncoderConfig::new()
        .bitrate(openh264::encoder::BitRate::from_bps(bps))
        .max_frame_rate(openh264::encoder::FrameRate::from_hz(VIDEO_FPS))
        .usage_type(openh264::encoder::UsageType::ScreenContentRealTime)
        .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(
            KEYFRAME_INTERVAL_FRAMES,
        ))
        .adaptive_quantization(false) // unsupported for screen content
        .background_detection(false) // unsupported for screen content
        .num_threads(0) // auto
}

/// Software encoder (openh264). The universal fallback — available on
/// every platform, proven end-to-end since 5b.
pub struct SoftwareEncoder {
    encoder: Encoder,
    width: usize,
    height: usize,
    current_bitrate: u32,
}

impl SoftwareEncoder {
    pub fn new(width: usize, height: usize) -> Result<Self, Error> {
        let encoder = Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            encoder_config(VIDEO_BITRATE_BPS),
        )
        .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self {
            encoder,
            width,
            height,
            current_bitrate: VIDEO_BITRATE_BPS,
        })
    }
}

impl VideoEncoder for SoftwareEncoder {
    fn codec(&self) -> CodecId {
        CodecId::H264
    }

    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn encode_bgra(&mut self, bgra: &[u8]) -> Result<(Vec<u8>, bool), Error> {
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

    fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// The openh264 Rust crate does not expose the raw `SetOption`
    /// bitrate path on the safe `Encoder` wrapper (the low-level API
    /// has it, but that exposure was never verified — the registry
    /// grep failed on the crate's directory layout). Instead: recreate
    /// the encoder at the new rate. Consequences, all acceptable for
    /// v1: the next frame is a standalone-decodable IDR (fresh
    /// encoder), rate-control state resets (brief ramp-up), and
    /// SPS/PPS ride in-band ahead of the IDR slice so existing
    /// decoders pick the change up with no extra signaling.
    fn set_bitrate(&mut self, bps: u32) {
        if bps == self.current_bitrate {
            return;
        }
        match Encoder::with_api_config(openh264::OpenH264API::from_source(), encoder_config(bps)) {
            Ok(encoder) => {
                self.encoder = encoder;
                self.current_bitrate = bps;
            }
            Err(e) => eprintln!(
                "video: bitrate change to {bps} bps failed ({e}); keeping {} bps",
                self.current_bitrate
            ),
        }
    }
}

/// Wraps the openh264 decoder. One per receiving sender.
pub struct VideoDecoder {
    decoder: Decoder,
}

impl VideoDecoder {
    pub fn new() -> Result<Self, Error> {
        let decoder =
            Decoder::with_api_config(openh264::OpenH264API::from_source(), DecoderConfig::new())
                .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self { decoder })
    }

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
            let in_bar = (x as i32 - bar_x).abs() < 24 && y.is_multiple_of(4);
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
        let mut enc = SoftwareEncoder::new(w, h).unwrap();
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
        let mut enc = SoftwareEncoder::new(w, h).unwrap();
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
        let mut enc = SoftwareEncoder::new(w, h).unwrap();
        let (frame, _k) = enc.encode_bgra(&test_frame(w, h, 0)).unwrap();
        assert!(
            frame.starts_with(&[0, 0, 0, 1]) || frame.starts_with(&[0, 0, 1]),
            "encoder output must be Annex-B (start code first), got {:?}",
            &frame[..4.min(frame.len())]
        );
    }

    #[test]
    fn bitrate_change_forces_standalone_idr() {
        let (w, h) = (320, 240);
        let mut enc = SoftwareEncoder::new(w, h).unwrap();

        // Prime: keyframe, then a delta within the intra period.
        let _ = enc.encode_bgra(&test_frame(w, h, 0)).unwrap();
        let (_, delta) = enc.encode_bgra(&test_frame(w, h, 1)).unwrap();
        assert!(!delta, "second frame within the intra period is a delta");

        // Runtime bitrate change recreates the encoder: the next frame
        // must be a standalone-decodable IDR with in-band SPS/PPS.
        enc.set_bitrate(1_000_000);
        let (f2, k2) = enc.encode_bgra(&test_frame(w, h, 2)).unwrap();
        assert!(k2, "first frame after a bitrate change must be a keyframe");

        let mut fresh = VideoDecoder::new().unwrap();
        let rgb = fresh
            .decode(&f2)
            .unwrap()
            .expect("post-change IDR must decode standalone");
        assert_eq!(rgb.len(), w * h * 3);
    }
}
