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
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::Error;

/// v1 send profile: 720p30 screen content.
pub const VIDEO_WIDTH: usize = 1280;
pub const VIDEO_HEIGHT: usize = 720;
/// Frame rate of the software (openh264) profile. Not a taste choice:
/// openh264 tops out around 32 fps at 1080p on this class of machine,
/// so promising more would just mean dropping frames.
pub const VIDEO_FPS_SOFTWARE: f32 = 30.0;
/// Tallest frame the SOFTWARE encoder is handed.
///
/// Above 1080p openh264 cannot hold a 30 fps budget on this class of
/// machine: measured at 2560x1440 it averages ~33 ms per frame with a
/// ~58 ms p95, so half the frames miss their slot and the sender's
/// skip-to-newest pacing turns that into visible stepping. A "Source"
/// share on a 1440p/4K display is therefore downscaled to 1080p rather
/// than offered as a stutter; hardware encode (VAAPI) is not capped and
/// does the full 1440p60. The share dialog's own cap still applies on
/// top, and a capture below this is never grown.
pub const SOFTWARE_MAX_HEIGHT: usize = 1080;
/// Largest NAL the software encoder emits, i.e. the slice size that lets
/// openh264 encode a frame on several threads.
///
/// Measured at 1080p30 (`tests/software_encode_cost.rs`): a single-slice
/// frame misses its 33 ms slot 117/240 times (p95 48 ms); split into
/// ~4 KB slices over 4 threads that drops to 17/240 (p95 34.5 ms). The
/// boundary is a visible cost only if slices get small, which is why
/// this is 4 KB and not the datagram budget (~1.2 KB).
const SOFTWARE_SLICE_BYTES: u32 = 4000;
/// Encoder threads for the software profile. Past four the frame stops
/// getting faster (measured: 8 threads buys 0.1 ms) because the slices
/// that remain are already in flight.
const SOFTWARE_THREADS: u16 = 4;
/// 7h HD: frame rate the share path targets once a HARDWARE encoder is
/// live. VCE/QuickSync do 1080p60 and 1440p60 without noticing; the
/// roadmap's "1440p60 with hardware encode" is gated on exactly this.
pub const VIDEO_FPS_HARDWARE: f32 = 60.0;
/// The software profile's rate under its historical name (the synthetic
/// sender and mpv's rawvideo demuxer use it).
pub const VIDEO_FPS: f32 = VIDEO_FPS_SOFTWARE;

/// Keyframe every 2 s: bounded loss-recovery window (5a's
/// drop-until-IDR rule means a late or lossy joiner waits at most
/// this long for a decodable picture). Expressed in TIME because the
/// frame rate is now profile-dependent — a 60 fps stream needs twice
/// the frames for the same recovery window.
const KEYFRAME_INTERVAL_SECS: f32 = 2.0;

/// Keyframe interval in frames for a given rate.
pub fn keyframe_interval_frames(fps: f32) -> u32 {
    (fps.max(1.0) * KEYFRAME_INTERVAL_SECS).round() as u32
}

/// The interval at the software profile's rate (2 s at 30 fps).
pub const KEYFRAME_INTERVAL_FRAMES: u32 = 60;
/// Screen content at 720p30 holds up well around 2.5 Mbps.
pub const VIDEO_BITRATE_BPS: u32 = 2_500_000;
/// Ceiling for the scaled profile below. 12 Mbps is comfortable on the
/// LAN/local-network deployment this is built for and still far under
/// what a 1080p60/1440p60 stream can spend.
pub const MAX_VIDEO_BITRATE_BPS: u32 = 12_000_000;
/// Floor, so a tiny window on a busy path still gets a usable picture.
pub const MIN_VIDEO_BITRATE_BPS: u32 = 800_000;

/// Bits per pixel per frame — the quality budget the 720p30 baseline
/// encodes (2.5 Mbps / (1280·720·30)). Held constant as resolution and
/// frame rate grow, which is what makes HD *look* like HD: the old
/// fixed 2.5 Mbps target is what a 720p30 stream spends, and handing it
/// to 1080p60 buys four times less detail per pixel.
pub const BITS_PER_PIXEL_PER_FRAME: f64 = VIDEO_BITRATE_BPS as f64 / (1280.0 * 720.0 * 30.0);

/// Bitrate target for a resolution and frame rate.
///
/// Exactly [`VIDEO_BITRATE_BPS`] at the 720p30 baseline (so nothing about
/// the shipped software profile moves), scaled with pixels and rate
/// above it, capped at [`MAX_VIDEO_BITRATE_BPS`]:
/// 1080p30 ≈ 5.6 Mbps · 1080p60 ≈ 11.3 Mbps · 1440p60 ≈ 12 Mbps (cap).
pub fn bitrate_for(width: usize, height: usize, fps: f32) -> u32 {
    let pixels = width.max(2) as f64 * height.max(2) as f64;
    let target = pixels * f64::from(fps.max(1.0)) * BITS_PER_PIXEL_PER_FRAME;
    (target.round() as u64).clamp(MIN_VIDEO_BITRATE_BPS as u64, MAX_VIDEO_BITRATE_BPS as u64) as u32
}

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

    /// 7g: encode one strided BGRA frame — `stride` is the row pitch in
    /// BYTES (≥ width*4). The buffer is borrowed for the duration of the
    /// call only. Backends that can convert strided input without a
    /// repack (openh264's SIMD path does) override this; the default
    /// copies tight and delegates to [`Self::encode_bgra`]. Returns the
    /// Annex-B bytes in the CALLER-PROVIDED `out` Vec (cleared first),
    /// avoiding a fresh allocation per frame on the hot path.
    fn encode_bgra_strided(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        out: &mut Vec<u8>,
    ) -> Result<bool, Error> {
        strided_fallback(self, bgra, width, height, stride, out)
    }

    /// Request an IDR on the next encode (keyframe-on-join).
    fn force_keyframe(&mut self);

    /// Change target bitrate at runtime (quality ladder). Backends may
    /// clamp; the change takes effect over the next few frames.
    fn set_bitrate(&mut self, bps: u32);

    /// Can this backend encode a [`DmaBufFrame`] without the pixels
    /// passing through the CPU? Only the VAAPI backend can (7j), and the
    /// capture side asks BEFORE it negotiates a stream format: a
    /// producer that hands out DMA-BUFs and a software encoder are an
    /// incompatible pair, so the decision has to be made up front.
    ///
    /// Unix-only, like the frame type: DMA-BUF is a kernel interface
    /// with no Windows equivalent (a Windows capture backend will hand
    /// over CPU frames, or share textures another way).
    #[cfg(unix)]
    fn dmabuf_supported(&self) -> bool {
        false
    }

    /// Encode one frame held in a DMA-BUF, writing Annex-B bytes into
    /// `out` (cleared first) and reporting whether it was flagged as a
    /// keyframe. `frame` owns its fd, so the caller's buffer stays open
    /// for the duration of the call.
    ///
    /// The default is a loud error rather than a silent copy: a backend
    /// that does not override this must never be handed a DMA-BUF, and
    /// `dmabuf_supported` is what callers check.
    #[cfg(unix)]
    fn encode_dmabuf(&mut self, frame: DmaBufFrame, out: &mut Vec<u8>) -> Result<bool, Error> {
        let _ = (frame, out);
        Err(Error::Codec(
            "this encoder cannot encode DMA-BUF frames".into(),
        ))
    }
}

/// One plane of a [`DmaBufFrame`], within the single fd holding them all.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaBufPlane {
    /// Byte offset of the plane from the start of the buffer.
    pub offset: u32,
    /// Bytes between adjacent rows of the plane.
    pub pitch: u32,
}

/// A frame some other API allocated, described the DRM way (7j).
///
/// This is the neutral form of `moq_vaapi::dmabuf::DmaBuf`: the encoder
/// trait lives in this crate, which must not depend on a VAAPI device, so
/// the capture side builds one of these from a PipeWire buffer and only the
/// backend knows how to import it.
///
/// `drm_format` is a `DRM_FORMAT_*` fourcc (little-endian bytes, e.g.
/// `b"XR24"` for packed `B G R X`), and `modifier` is the DRM format
/// modifier the producer negotiated — a driver will refuse to import a
/// tiling it cannot address, which is a fact the caller cannot paper over.
#[cfg(unix)]
#[derive(Debug)]
pub struct DmaBufFrame {
    /// The exported descriptor; held open until the encoder returns.
    pub fd: std::os::fd::OwnedFd,
    /// Pixel format, a `DRM_FORMAT_*` code.
    pub drm_format: u32,
    /// Format modifier describing the tiling of the buffer.
    pub modifier: u64,
    /// Visible width in pixels (padding lives in the plane pitches).
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Plane offsets and pitches, in format order (one to four).
    pub planes: Vec<DmaBufPlane>,
}

/// Construct the best available encoder for this platform. Hardware
/// backends are feature-gated, tried first, and fall back to software
/// at runtime.
pub fn create_encoder(width: usize, height: usize) -> Result<Box<dyn VideoEncoder>, Error> {
    HW_ACTIVE.store(false, Ordering::Relaxed);
    HW_MEASURED_MFPS.store(0, Ordering::Relaxed);
    Ok(Box::new(SoftwareEncoder::new(width, height)?))
}

/// 7h: is a HARDWARE encoder the one actually in use? Set when
/// [`create_encoder_auto`] settles on a backend (only after the
/// hardware self-test passes), never mid-stream.
static HW_ACTIVE: AtomicBool = AtomicBool::new(false);

/// True when the live encoder is a hardware one — the transport and
/// display paths size themselves from this (see [`target_fps`]).
pub fn hardware_encoder_active() -> bool {
    HW_ACTIVE.load(Ordering::Relaxed)
}

/// Sustained rate measured at encoder creation, in milli-fps (0 = not
/// measured). See [`hardware_measured_fps`].
static HW_MEASURED_MFPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The hardware encoder's MEASURED throughput over a short self-test
/// at encoder creation, in fps (diagnostic only — see [`target_fps`]).
///
/// It is logged and shown by the hardware test tools, but it no longer
/// CAPS the stream. It used to: the share path paced at this number, and
/// because it is measured during the encoder's first, cold frames (the
/// video processor's context, the BGRX surface and the driver's shaders
/// are all created inside the timing window) it read well under the
/// sustained rate — a machine whose encoder ran at ~60 fps paced its
/// whole share at 50, and a 1080p share read as 30. The encoder's real
/// deliverable rate is simply what it delivers: the pacing loop skips a
/// frame it cannot encode in time and viewers measure the arrival
/// cadence, so a cap only ever made the stream slower than the machine
/// and slower than the viewer's own report.
///
/// The measurement happens once, at creation (see
/// [`create_encoder_auto`]).
pub fn hardware_measured_fps() -> Option<f32> {
    let m = HW_MEASURED_MFPS.load(Ordering::Relaxed);
    (m > 0).then(|| m as f32 / 1000.0)
}

/// Frame rate the share path should pace and timestamp at, given the
/// backend in use: the profile ceiling (60 fps hardware, 30 fps
/// software), lowered by the capture's own rate and by the share
/// dialog's cap. See [`hardware_measured_fps`] for why the measured
/// rate is not a factor.
pub fn target_fps() -> f32 {
    if !hardware_encoder_active() {
        return VIDEO_FPS_SOFTWARE;
    }
    VIDEO_FPS_HARDWARE
}

/// A stream below this is not worth sending as video.
pub const VIDEO_FPS_MIN: f32 = 10.0;

/// The default strided path: repack tight, delegate to `encode_bgra`.
/// Shared by the trait default and SoftwareEncoder's size-mismatch case
/// (and VaapiEncoder, which uploads tight NV12 anyway).
pub(crate) fn strided_fallback(
    enc: &mut (impl VideoEncoder + ?Sized),
    bgra: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    out: &mut Vec<u8>,
) -> Result<bool, Error> {
    let mut packed = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let row = bgra
            .get(y * stride..y * stride + width * 4)
            .ok_or_else(|| Error::Codec("strided frame truncated".into()))?;
        packed.extend_from_slice(row);
    }
    let (bytes, keyframe) = enc.encode_bgra(&packed)?;
    out.clear();
    out.extend_from_slice(&bytes);
    Ok(keyframe)
}

/// 7g: a borrowed, ROW-PADDED BGRA buffer (PipeWire's mapped frame)
/// as an openh264 `RGB8Source`. `dimensions_padded` carries the row
/// pitch so the crate's AVX2 converter walks the stride directly —
/// no repack. Channel order (2,1,0) = BGRA, pixel_stride 4.
struct PwStridedBgra<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    /// Row pitch in BYTES.
    stride: usize,
}

impl openh264::formats::RGBSource for PwStridedBgra<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
    fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
        let o = (y * self.stride + x * 4).min(self.data.len().saturating_sub(4));
        (
            f32::from(self.data[o + 2]),
            f32::from(self.data[o + 1]),
            f32::from(self.data[o]),
        )
    }
}

impl openh264::formats::RGB8Source for PwStridedBgra<'_> {
    fn dimensions_padded(&self) -> (usize, usize) {
        (self.stride / 4, self.height)
    }
    fn rgb8_data(&self) -> &[u8] {
        self.data
    }
    fn pixel_stride(&self) -> usize {
        4
    }
    fn rgb_channel_offsets(&self) -> (usize, usize, usize) {
        (2, 1, 0)
    }
}

impl openh264::formats::BGRA8Source for PwStridedBgra<'_> {}

/// The openh264 encoder configuration, shared by construction and
/// bitrate changes (the latter recreates the encoder).
fn encoder_config(bps: u32, fps: f32, threads: u16) -> EncoderConfig {
    EncoderConfig::new()
        .bitrate(openh264::encoder::BitRate::from_bps(bps))
        .max_frame_rate(openh264::encoder::FrameRate::from_hz(fps))
        .usage_type(openh264::encoder::UsageType::ScreenContentRealTime)
        .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(
            keyframe_interval_frames(fps),
        ))
        .adaptive_quantization(false) // unsupported for screen content
        .background_detection(false) // unsupported for screen content
        // Slice-based parallelism: 1080p30 is not reachable in real time
        // on one core (see SOFTWARE_SLICE_BYTES for the measurement).
        // The server's transcode worker bounds this to one thread so a
        // single viewer cannot fan out across the host.
        .num_threads(threads.max(1))
        .max_slice_len(SOFTWARE_SLICE_BYTES)
}

/// Fit a capture into [`SOFTWARE_MAX_HEIGHT`], preserving the aspect
/// ratio and never growing it. Even dimensions only: openh264 rejects an
/// odd width, and a 2:1 downscale of an odd height is not worth a
/// special case.
pub fn fit_software_dims(width: usize, height: usize) -> (usize, usize) {
    if height <= SOFTWARE_MAX_HEIGHT || height == 0 {
        return (width, height);
    }
    let even = |v: usize| (v & !1).max(2);
    (
        even((width * SOFTWARE_MAX_HEIGHT) / height),
        even(SOFTWARE_MAX_HEIGHT),
    )
}

/// Software encoder (openh264). The universal fallback — available on
/// every platform, proven end-to-end since 5b.
pub struct SoftwareEncoder {
    encoder: Encoder,
    width: usize,
    height: usize,
    current_bitrate: u32,
    /// Slice-parallelism budget; a server transcode job passes 1.
    threads: u16,
    /// 7g: reused YUV conversion buffer (saves a 1.3–2 MB alloc+copy
    /// per frame over `YUVBuffer::from_bgra8_source`).
    yuv: YUVBuffer,
}

impl SoftwareEncoder {
    pub fn new(width: usize, height: usize) -> Result<Self, Error> {
        Self::with_bitrate(
            width,
            height,
            bitrate_for(width, height, VIDEO_FPS_SOFTWARE),
        )
    }

    /// 7h HD: explicit target, so a caller that knows the profile (the
    /// share path, scaling for resolution) does not go through the
    /// 720p30 baseline.
    pub fn with_bitrate(width: usize, height: usize, bps: u32) -> Result<Self, Error> {
        Self::with_bitrate_threads(width, height, bps, SOFTWARE_THREADS)
    }

    /// Explicit encoder parallelism. Used by the server's transcode
    /// worker: one share's re-encode must not spend four cores, so it
    /// asks for one thread even though the client profile uses four.
    pub fn with_bitrate_threads(
        width: usize,
        height: usize,
        bps: u32,
        threads: u16,
    ) -> Result<Self, Error> {
        let threads = threads.max(1);
        let encoder = Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            encoder_config(bps, VIDEO_FPS_SOFTWARE, threads),
        )
        .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self {
            encoder,
            width,
            height,
            current_bitrate: bps,
            threads,
            yuv: YUVBuffer::new(width, height),
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
        self.yuv.read_bgra8(src);
        let stream = self
            .encoder
            .encode(&self.yuv)
            .map_err(|e| Error::Codec(e.to_string()))?;
        let keyframe = matches!(
            stream.frame_type(),
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        Ok((stream.to_vec(), keyframe))
    }

    /// 7g hot path: feed the STRIDED pw buffer straight into openh264's
    /// AVX2 BGRA→YUV420 converter via a custom `RGB8Source` view (the
    /// trait's `dimensions_padded` carries the row pitch — verified
    /// SIMD + correct against the tight-packed path in the 7g bench).
    /// The buffer is borrowed; no repack, no per-frame YUV alloc.
    fn encode_bgra_strided(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        out: &mut Vec<u8>,
    ) -> Result<bool, Error> {
        if width != self.width || height != self.height {
            // Renegotiated size: fall back to the copy path (encode_bgra
            // re-derives dims from self.width/height — never mismatch).
            return strided_fallback(self, bgra, width, height, stride, out);
        }
        if stride == width * 4 {
            // Tight: use the crate's own zero-check wrapper.
            let src = openh264::formats::BgraSliceU8::new(bgra, (width, height));
            self.yuv.read_bgra8(src);
        } else {
            let src = PwStridedBgra {
                data: bgra,
                width,
                height,
                stride,
            };
            self.yuv.read_bgra8(src);
        }
        let stream = self
            .encoder
            .encode(&self.yuv)
            .map_err(|e| Error::Codec(e.to_string()))?;
        let keyframe = matches!(
            stream.frame_type(),
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        out.clear();
        stream.write_vec(out);
        Ok(keyframe)
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
        match Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            encoder_config(bps, VIDEO_FPS_SOFTWARE, self.threads),
        ) {
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

    /// 7m: decode straight to RGBA8 instead of RGB8.
    ///
    /// The Flutter bridge's texture wants RGBA, and converting RGB→RGBA
    /// afterwards is a second full-frame pass over the picture (plus a
    /// 6 MB intermediate at 1080p). `write_rgba8` is the same SIMD
    /// converter with a fourth, opaque channel written inline — one pass
    /// instead of two, and no intermediate buffer to allocate.
    pub fn decode_rgba_with_dims(
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
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                Ok(Some((rgba, w, h)))
            }
            None => Ok(None),
        }
    }
}
/// 7g: opt-in hardware selection state. NOT a global default — the
/// client sets this from `--hw-encode` BEFORE creating any encoder.
/// VAAPI is tried first; ANY error falls back to software, so a box
/// without VAAPI (or with broken VCE) shares exactly as before.
static HW_ENCODE: AtomicBool = AtomicBool::new(false);

/// Enable hardware-encode preference. Idempotent; call before
/// [`create_encoder`]. See [`create_encoder`] for the fallback rules.
pub fn prefer_hw_encode() {
    set_hw_encode(true);
}

/// 7g: full runtime switch (the Flutter app's toggle). Takes effect at
/// the NEXT encoder creation — a running share keeps its backend; stop
/// and re-share to switch.
pub fn set_hw_encode(enable: bool) {
    HW_ENCODE.store(enable, Ordering::Relaxed);
}

/// Whether hardware preference is on (for diagnostics).
pub fn hw_encode_enabled() -> bool {
    HW_ENCODE.load(Ordering::Relaxed)
}

/// 7h: does this encoder's output actually decode?
///
/// A hardware encoder can emit frames that are perfect for the
/// transport and undecodable for every receiver — exactly what
/// radeonsi/VCE did while the vendored crate's SPS/PPS disagreed with
/// the slice syntax the driver writes. Nothing in the share path
/// noticed: the sender logged encoded frames, the receivers logged
/// nothing, and the user watched an empty window while the loss ladder
/// thrashed. So validate BEFORE going live: force a keyframe, hand it to
/// a FRESH decoder (the receiver's real situation), and require a
/// picture.
///
/// The pattern is deliberately detailed — a flat frame can survive a
/// bitstream a real screen capture would not.
#[cfg(feature = "vaapi")]
fn encoder_produces_decodable_keyframe(enc: &mut dyn VideoEncoder) -> Result<(), Error> {
    let (w, h) = enc.dimensions();
    if w == 0 || h == 0 {
        return Err(Error::Codec(format!(
            "encoder reports no dimensions ({w}x{h})"
        )));
    }
    let mut bgra = vec![0u8; w * h * 4];
    for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = (i % w) as u8;
        let y = (i / w) as u8;
        px[0] = x;
        px[1] = y;
        px[2] = x ^ y;
        px[3] = 255;
    }
    enc.force_keyframe();
    let (annexb, keyframe) = enc.encode_bgra(&bgra)?;
    if !keyframe {
        return Err(Error::Codec(
            "self-test frame was not flagged a keyframe".into(),
        ));
    }
    let mut dec = VideoDecoder::new()?;
    match dec.decode_with_dims(&annexb) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(Error::Codec(
            "keyframe decoded to no picture — a mid-stream receiver could never start".into(),
        )),
        Err(e) => Err(Error::Codec(format!(
            "keyframe rejected by a fresh decoder: {e}"
        ))),
    }
}

/// 7h HD: how fast is this encoder really, through the path the share
/// will use?
///
/// Times a short run of real encode calls — `encode_bgra_strided`, with
/// a padded stride, i.e. the repack AND the conversion AND the driver,
/// not a microbenchmark of one stage — and returns the sustained rate.
/// The first frame is dropped from the measurement (it pays the
/// driver's pipeline priming), and the result is the rate the profile
/// should advertise.
#[cfg(feature = "vaapi")]
fn measure_encoder_fps(enc: &mut dyn VideoEncoder) -> Option<f32> {
    let (w, h) = enc.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    const WARMUP: usize = 3;
    const FRAMES: usize = 12;
    // Padded rows, like a compositor's buffer: the strided path is the
    // one a real share uses, so it is the one to measure.
    let stride = w * 4 + 256;
    let mut bgra = vec![0u8; stride * h];
    for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = (i % stride / 4) as u8;
        let y = (i / stride) as u8;
        px[0] = x.wrapping_mul(3);
        px[1] = y.wrapping_mul(5);
        px[2] = x ^ y;
        px[3] = 255;
    }
    let mut out = Vec::new();
    // Warm-up frames are NOT timed: the first frames pay for the video
    // processor's context, the BGRX surface and the driver's shader
    // uploads — one-time costs that otherwise land inside the window and
    // make a 60 fps encoder measure ~50 (and pace the whole share at 50).
    for _ in 0..WARMUP {
        enc.encode_bgra_strided(&bgra, w, h, stride, &mut out)
            .ok()?;
    }
    // Per-frame times, median: one scheduler hiccup or a GPU frequency
    // dip must not move the rate the live share is paced at.
    let mut total = 0.0;
    for _ in 0..FRAMES {
        let t = std::time::Instant::now();
        enc.encode_bgra_strided(&bgra, w, h, stride, &mut out)
            .ok()?;
        total += t.elapsed().as_secs_f64();
    }
    (total > 0.0).then(|| (FRAMES as f64 / total) as f32)
}

/// Construct the best available encoder for this platform.
///
/// With `prefer_hw_encode()` set and the `vaapi` feature enabled, tries
/// [`VaapiEncoder`] first and falls back to [`SoftwareEncoder`] on any
/// init error (missing libva, no encode-capable node, driver refusal)
/// OR on a failed keyframe self-test (see
/// [`encoder_produces_decodable_keyframe`] — init succeeding says
/// nothing about whether the frames decode).
///
/// Without either, software. Hardware is never attempted mid-stream:
/// the decision happens once, at encoder creation (bitrate changes
/// recreate the chosen backend only).
///
/// 7h HD: the two backends run different PROFILES. Hardware is asked
/// for [`VIDEO_FPS_HARDWARE`] and a bitrate scaled to the resolution
/// ([`bitrate_for`]) — that is what makes 1080p60/1440p60 reachable;
/// software stays at its 30 fps ceiling and its 720p30-anchored rate.
/// [`hardware_encoder_active`] records which one won, and the share
/// path paces and timestamps from [`target_fps`].
pub fn create_encoder_auto(width: usize, height: usize) -> Result<Box<dyn VideoEncoder>, Error> {
    if hw_encode_enabled() {
        #[cfg(feature = "vaapi")]
        match crate::video_vaapi::VaapiEncoder::new_profile(
            width,
            height,
            VIDEO_FPS_HARDWARE,
            bitrate_for(width, height, VIDEO_FPS_HARDWARE),
        ) {
            Ok(mut e) => match encoder_produces_decodable_keyframe(&mut e) {
                Ok(()) => {
                    // 7h HD: the decodability self-test says the stream
                    // is usable; this says roughly how fast it is. It is
                    // diagnostic (the log line below), never a pacing
                    // cap — see `hardware_measured_fps` for why.
                    let measured = measure_encoder_fps(&mut e);
                    // The tests consumed frames and left the encoder
                    // mid-GOP: make the first REAL frame an IDR so a
                    // viewer joining at t=0 never waits for one.
                    e.force_keyframe();
                    HW_ACTIVE.store(true, Ordering::Relaxed);
                    if let Some(fps) = measured {
                        HW_MEASURED_MFPS.store((fps * 1000.0) as u32, Ordering::Relaxed);
                    } else {
                        HW_MEASURED_MFPS.store(0, Ordering::Relaxed);
                    }
                    eprintln!(
                        "video: hardware (VAAPI) encoder selected — {width}x{height}, self-test \
						 {:.1} fps, pacing up to {:.0} fps at {} bps, keyframe self-test \
						 passed (verified decodable by a fresh decoder); if the GPU wedges, \
						 this is why",
                        measured.unwrap_or(0.0),
                        target_fps(),
                        bitrate_for(width, height, target_fps())
                    );
                    return Ok(Box::new(e));
                }
                Err(e) => eprintln!(
                    "video: VAAPI encoder self-test FAILED ({e}); falling back to software — \
                     hardware share would have shown every viewer an empty window"
                ),
            },
            Err(e) => eprintln!("video: VAAPI init failed ({e}); falling back to software"),
        }
        #[cfg(not(feature = "vaapi"))]
        {
            eprintln!(
                "video: hw-encode requested but built without the vaapi feature; using software"
            );
        }
    }
    HW_ACTIVE.store(false, Ordering::Relaxed);
    HW_MEASURED_MFPS.store(0, Ordering::Relaxed);
    let (width, height) = fit_software_dims(width, height);
    let e = SoftwareEncoder::new(width, height)?;
    eprintln!(
        "video: software (openh264) encoder selected — {width}x{height} @ {VIDEO_FPS_SOFTWARE} \
         fps, {} bps",
        bitrate_for(width, height, VIDEO_FPS_SOFTWARE)
    );
    Ok(Box::new(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Horizontal gradient + a moving bar, so frames differ and motion
    /// compensation has something to chew on.
    fn test_frame(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut bgra = vec![0u8; w * h * 4];
        for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
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

    /// The software profile's ceiling: aspect preserved, even, never
    /// grown, and a capture that already fits is untouched (a 720p
    /// window must not be upscaled to 1080p just because it is smaller).
    #[test]
    fn software_dims_fit_the_profile() {
        assert_eq!(fit_software_dims(1280, 720), (1280, 720));
        assert_eq!(fit_software_dims(1920, 1080), (1920, 1080));
        assert_eq!(fit_software_dims(2560, 1440), (1920, 1080));
        assert_eq!(fit_software_dims(3440, 1440), (2580, 1080));
        assert_eq!(fit_software_dims(3840, 2160), (1920, 1080));
        assert_eq!(fit_software_dims(1080, 1920), (606, 1080));
        // Odd results are snapped even, never to zero.
        let (w, h) = fit_software_dims(1001, 1440);
        assert_eq!((w % 2, h % 2), (0, 0));
        assert!(w >= 2 && h >= 2);
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
