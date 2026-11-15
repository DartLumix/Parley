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
use std::time::{Duration, Instant};

use crate::error::Error;

/// Start Media Foundation once for this process. The camera capture path
/// and the hardware encoder/decoder share the platform; a per-camera
/// `MFShutdown` would tear the encoder down under a live share.
#[cfg(all(feature = "video", windows))]
pub fn ensure_media_foundation() -> Result<(), Error> {
    unsafe { crate::video_windows::init_mf() }
}

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
/// openh264 has no safe runtime-bitrate setter in the wrapper we use, so a
/// rate change recreates the encoder (and starts a fresh IDR). The adaptive
/// controller can lower the target every frame during congestion; rebuilding
/// openh264 that often is itself an overload. Coalesce those requests and
/// apply the newest one at most this often. Hardware backends set their rate
/// between frames and never touch this path.
const SOFTWARE_BITRATE_RECONFIG: Duration = Duration::from_millis(500);
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
/// The historical 720p30 baseline. Kept as the anchor for the scaled
/// profile, but no longer the budget a real share is held to.
pub const VIDEO_BITRATE_BPS: u32 = 2_500_000;
/// Ceiling for the scaled profile below. This is deliberately generous:
/// a motion-heavy 1440p60 stream is ~27 Mbps at the profile's own
/// bits-per-pixel, and the whole point of the ladder is that the path —
/// not an arbitrary constant — decides when to spend less. A sender on a
/// slow link still gets there through the loss ladder, and a LAN or a
/// good VPS link can actually carry the picture.
pub const MAX_VIDEO_BITRATE_BPS: u32 = 30_000_000;
/// Floor, so a tiny window on a busy path still gets a usable picture.
pub const MIN_VIDEO_BITRATE_BPS: u32 = 800_000;

/// Bits per pixel per frame. The 720p30 baseline was 0.090; motion-heavy
/// game content needs roughly 0.10–0.15 to stay sharp, and screen text
/// is the easy case at the low end. 0.12 is the compromise the profile
/// uses: 720p30 ≈ 3.3 Mbps, 1080p60 ≈ 15 Mbps, 1440p60 ≈ 27 Mbps. The
/// previous 0.090 profile is what made a fast-moving 1080p60 share look
/// soft just as the content got hard to encode — the encoder ran out of
/// bits, not pixels.
pub const BITS_PER_PIXEL_PER_FRAME: f64 = 0.12;

/// Bitrate target for a resolution and frame rate.
///
/// Scaled with pixels and rate, floored at [`MIN_VIDEO_BITRATE_BPS`] and
/// capped at [`MAX_VIDEO_BITRATE_BPS`]:
/// 720p30 ≈ 3.3 Mbps · 1080p30 ≈ 7.5 Mbps · 1080p60 ≈ 15 Mbps ·
/// 1440p60 ≈ 27 Mbps · 4K60 ≈ 30 Mbps (cap).
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
/// at encoder creation, in fps.
///
/// This is measured through the same strided encode path the share uses,
/// so it is the ceiling the capture loop can actually hold. Pacing above
/// it makes the loop skip frames and makes the latency ladder treat an
/// encoder overrun as network loss; [`target_fps`] therefore caps at this
/// number with a small safety margin.
///
/// The measurement happens once, at creation (see
/// [`create_encoder_auto`]).
pub fn hardware_measured_fps() -> Option<f32> {
    let m = HW_MEASURED_MFPS.load(Ordering::Relaxed);
    (m > 0).then(|| m as f32 / 1000.0)
}

/// Frame rate the share path should pace and timestamp at, given the
/// backend in use: the measured hardware rate (with 5% headroom, and
/// never above the 60 fps profile ceiling) or the 30 fps software rate.
/// The capture's own rate and the share dialog's cap can lower it
/// further. Advertising a rate the encoder cannot sustain turns a
/// smooth stream into periodic skipped frames, so the measurement is a
/// cap rather than a diagnostic here.
pub fn target_fps() -> f32 {
    if !hardware_encoder_active() {
        return VIDEO_FPS_SOFTWARE;
    }
    let measured = hardware_measured_fps().unwrap_or(VIDEO_FPS_HARDWARE);
    (measured * 0.95).clamp(VIDEO_FPS_MIN, VIDEO_FPS_HARDWARE)
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
        // The caller chooses the thread budget; the share path uses the
        // client profile below.
        .num_threads(threads.max(1))
        .max_slice_len(SOFTWARE_SLICE_BYTES)
}

/// Fit a capture into [`SOFTWARE_MAX_HEIGHT`], preserving the aspect
/// ratio and never growing it. Even dimensions only: openh264 rejects an
/// odd width, and a 2:1 downscale of an odd height is not worth a
/// special case.
pub fn fit_software_dims(width: usize, height: usize) -> (usize, usize) {
    let even = |v: usize| (v & !1).max(2);
    if width == 0 || height == 0 {
        return (width, height);
    }
    if height <= SOFTWARE_MAX_HEIGHT {
        return (even(width), even(height));
    }
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
    /// Newest rate requested while a rebuild was throttled.
    pending_bitrate: Option<u32>,
    /// When the encoder was last rebuilt for a rate change.
    last_bitrate_change: Option<Instant>,
    /// Slice-parallelism budget chosen by the caller.
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

    /// Explicit encoder parallelism, for a caller that must bound the
    /// software encoder below the client profile's four threads.
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
            pending_bitrate: None,
            last_bitrate_change: None,
            threads,
            yuv: YUVBuffer::new(width, height),
        })
    }

    /// Apply the newest requested rate if the rebuild throttle allows it.
    /// Returns true when the encoder was replaced.
    fn apply_pending_bitrate(&mut self) -> bool {
        let Some(bps) = self.pending_bitrate else {
            return false;
        };
        if self
            .last_bitrate_change
            .is_some_and(|last| last.elapsed() < SOFTWARE_BITRATE_RECONFIG)
        {
            return false;
        }
        if bps == self.current_bitrate {
            self.pending_bitrate = None;
            return false;
        }
        match Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            encoder_config(bps, VIDEO_FPS_SOFTWARE, self.threads),
        ) {
            Ok(encoder) => {
                self.encoder = encoder;
                self.current_bitrate = bps;
                self.pending_bitrate = None;
                self.last_bitrate_change = Some(Instant::now());
                true
            }
            Err(e) => {
                eprintln!(
                    "video: bitrate change to {bps} bps failed ({e}); keeping {} bps",
                    self.current_bitrate
                );
                // Do not retry a rejected configuration on every frame.
                self.pending_bitrate = None;
                self.last_bitrate_change = Some(Instant::now());
                false
            }
        }
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
        self.apply_pending_bitrate();
        // openh264's `BgraSliceU8::new` asserts on a length mismatch.
        // A caller that rebuilt the encoder mid-frame used to take the
        // whole encoder thread down with it; return an error and let the
        // share drop one frame instead.
        let expected = self.width.saturating_mul(self.height).saturating_mul(4);
        if bgra.len() != expected {
            return Err(Error::Codec(format!(
                "software encoder is {}x{} ({expected} bytes), got {}",
                self.width,
                self.height,
                bgra.len()
            )));
        }
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
        self.apply_pending_bitrate();
        if width == 0 || height == 0 || stride < width.saturating_mul(4) {
            return Err(Error::Codec(format!(
                "strided frame {width}x{height} has invalid stride {stride}"
            )));
        }
        let needed = stride
            .saturating_mul(height.saturating_sub(1))
            .saturating_add(width.saturating_mul(4));
        if bgra.len() < needed {
            return Err(Error::Codec(format!(
                "strided frame is {} bytes, {width}x{height} at stride {stride} needs {needed}",
                bgra.len()
            )));
        }
        if width != self.width || height != self.height {
            // Renegotiated size: fall back to the copy path; the copy
            // path's length guard turns a mismatch into an error.
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
            self.pending_bitrate = None;
            return;
        }
        self.pending_bitrate = Some(bps);
        self.apply_pending_bitrate();
    }
}

#[allow(dead_code)] // target-specific hardware backend helper
/// Convert tightly packed NV12 (BT.601 limited range) to RGB24 or RGBA8.
///
/// The hardware decoders hand back NV12 because that is the surface format
/// every VA-API and Media Foundation H.264 decoder produces. The software
/// path keeps openh264's own converter, which is SIMD and works directly on
/// its YUV buffer; this is the single shared conversion for hardware output.
pub(crate) fn nv12_to_rgb(nv12: &[u8], width: usize, height: usize, rgba: bool) -> Vec<u8> {
    let pixels = width.saturating_mul(height);
    let stride = if rgba { 4 } else { 3 };
    let mut out = vec![0u8; pixels.saturating_mul(stride)];
    if width == 0 || height == 0 || nv12.len() < pixels + pixels / 2 {
        return out;
    }
    let (y_plane, uv_plane) = nv12.split_at(pixels);
    let uv_row = width;
    for y in 0..height {
        let y_row = &y_plane[y * width..(y + 1) * width];
        let uv = &uv_plane[(y / 2) * uv_row..(y / 2 + 1) * uv_row];
        for x in 0..width {
            let c = i32::from(y_row[x]).saturating_sub(16).max(0);
            let u = i32::from(uv[(x / 2) * 2]) - 128;
            let v = i32::from(uv[(x / 2) * 2 + 1]) - 128;
            let r = ((298 * c + 409 * v + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 516 * u + 128) >> 8).clamp(0, 255) as u8;
            let i = (y * width + x) * stride;
            out[i] = r;
            out[i + 1] = g;
            out[i + 2] = b;
            if rgba {
                out[i + 3] = 255;
            }
        }
    }
    out
}

#[allow(dead_code)] // target-specific hardware backend helper
/// BGRA → tightly packed I420 (Y, then U, then V), BT.601 limited range.
/// Android's MediaCodec input `Image` is filled from this representation.
pub(crate) fn bgra_to_i420_into(
    bgra: &[u8],
    width: usize,
    height: usize,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Error::Codec(format!(
            "bgra_to_i420: {width}x{height} — width and height must be non-zero and even"
        )));
    }
    if bgra.len() < width * height * 4 {
        return Err(Error::Codec(format!(
            "bgra_to_i420: {} bytes, {width}x{height} needs {}",
            bgra.len(),
            width * height * 4
        )));
    }
    let y_len = width * height;
    let chroma = (width / 2) * (height / 2);
    out.resize(y_len + chroma * 2, 0);
    let (y_plane, chroma_plane) = out.split_at_mut(y_len);
    let (u_plane, v_plane) = chroma_plane.split_at_mut(chroma);
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 4;
            let b = i32::from(bgra[i]);
            let g = i32::from(bgra[i + 1]);
            let r = i32::from(bgra[i + 2]);
            y_plane[y * width + x] =
                (16 + ((66 * r + 129 * g + 25 * b + 128) >> 8)).clamp(0, 255) as u8;
            if y % 2 == 0 && x % 2 == 0 {
                let ci = (y / 2) * (width / 2) + x / 2;
                u_plane[ci] = (128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8)).clamp(0, 255) as u8;
                v_plane[ci] = (128 + ((112 * r - 94 * g - 18 * b + 128) >> 8)).clamp(0, 255) as u8;
            }
        }
    }
    Ok(())
}

#[allow(dead_code)] // target-specific hardware backend helper
/// Convert tightly packed I420 (BT.601 limited range) to RGB24 or RGBA8.
pub(crate) fn i420_to_rgb(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    width: usize,
    height: usize,
    rgba: bool,
) -> Vec<u8> {
    let pixels = width.saturating_mul(height);
    let stride = if rgba { 4 } else { 3 };
    let mut out = vec![0u8; pixels.saturating_mul(stride)];
    let cw = width.div_ceil(2);
    let ch = height.div_ceil(2);
    let chroma = cw.saturating_mul(ch);
    if width == 0
        || height == 0
        || y_plane.len() < pixels
        || u_plane.len() < chroma
        || v_plane.len() < chroma
    {
        return out;
    }
    for y in 0..height {
        for x in 0..width {
            let c = i32::from(y_plane[y * width + x]).saturating_sub(16).max(0);
            let ci = (y / 2) * cw + x / 2;
            let u = i32::from(u_plane[ci]) - 128;
            let v = i32::from(v_plane[ci]) - 128;
            let r = ((298 * c + 409 * v + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 516 * u + 128) >> 8).clamp(0, 255) as u8;
            let i = (y * width + x) * stride;
            out[i] = r;
            out[i + 1] = g;
            out[i + 2] = b;
            if rgba {
                out[i + 3] = 255;
            }
        }
    }
    out
}

#[allow(dead_code)] // target-specific hardware backend helper
/// Does this Annex-B buffer carry a NAL unit of `nal_type`?
///
/// Scans start codes explicitly instead of searching for a byte value: an
/// SPS/PPS/IDR payload can contain the same byte, and treating that as a
/// parameter set would make the encoder prepend duplicates forever.
pub(crate) fn h264_has_nal(data: &[u8], nal_type: u8) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        let start = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            Some(i + 3)
        } else if i + 4 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            Some(i + 4)
        } else {
            None
        };
        if let Some(header) = start {
            if data.get(header).is_some_and(|b| b & 0x1f == nal_type) {
                return true;
            }
            i = header;
        } else {
            i += 1;
        }
    }
    false
}

#[allow(dead_code)] // target-specific hardware backend helper
/// A hardware encoder may emit either Annex-B (start codes) or AVCC
/// (four-byte length prefixes). The rest of Parley only speaks Annex-B, so
/// normalize here and return whether the access unit is a keyframe.
///
/// Returns false and leaves `out` empty for a malformed buffer; the caller
/// treats that as "no output this frame" rather than sending a corrupt
/// group to every viewer.
pub(crate) fn h264_to_annexb(data: &[u8], out: &mut Vec<u8>) -> bool {
    out.clear();
    if data.is_empty() {
        return false;
    }
    // Try AVCC first. A length prefix such as 0x0000012c itself starts with
    // 00 00 01, so a first-bytes start-code test misclassifies that whole
    // class of AVCC access units. A genuine Annex-B payload (which starts
    // with a start code) does not parse as a complete sequence of
    // length-prefixed NALs, so it falls through cleanly.
    let mut i = 0usize;
    let mut avcc = true;
    while i < data.len() {
        if i + 4 > data.len() {
            avcc = false;
            break;
        }
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        let Some(end) = i.checked_add(len) else {
            avcc = false;
            break;
        };
        if len == 0
            || end > data.len()
            || data[i] & 0x80 != 0
            || !(1..=23).contains(&(data[i] & 0x1f))
        {
            avcc = false;
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..end]);
        i = end;
    }
    if avcc && i == data.len() && !out.is_empty() {
        return h264_has_nal(out, 5);
    }
    out.clear();
    if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        out.extend_from_slice(data);
        return h264_has_nal(out, 5);
    }
    false
}

#[allow(dead_code)] // target-specific hardware backend helper
/// Frame size below which the BGRA→NV12 conversion stays single-threaded:
/// thread hand-off costs more than it saves on small pictures, and a 720p
/// frame already converts in ~4.5 ms on one core.
const PARALLEL_CONVERT_MIN_PIXELS: usize = 1280 * 720;

#[allow(dead_code)] // target-specific hardware backend helper
/// BGRA (0xAABBGGRR little-endian) → NV12 (4:2:0, Y then interleaved UV),
/// BT.601 limited range. Shared by every hardware encoder, all of which take
/// NV12; `width` and `height` must be even.
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize) -> Result<Vec<u8>, Error> {
    let mut nv12 = Vec::new();
    bgra_to_nv12_into(bgra, width, height, &mut nv12)?;
    Ok(nv12)
}

#[allow(dead_code)] // target-specific hardware backend helper
/// [`bgra_to_nv12`] into a caller-owned buffer, so the encoder's hot path
/// reuses one staging allocation instead of allocating a fresh 3 MB Vec at
/// 1080p60. A resolution change resizes it to the exact new NV12 length.
pub(crate) fn bgra_to_nv12_into(
    bgra: &[u8],
    width: usize,
    height: usize,
    nv12: &mut Vec<u8>,
) -> Result<(), Error> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Error::Codec(format!(
            "bgra_to_nv12: {width}x{height} — width and height must be non-zero and even \
             (4:2:0 chroma is sampled 2x2)"
        )));
    }
    let expected = width * height * 4;
    if bgra.len() < expected {
        return Err(Error::Codec(format!(
            "bgra_to_nv12: {} bytes, {width}x{height} needs {expected}",
            bgra.len()
        )));
    }
    let len = width * height + (width / 2) * (height / 2) * 2;
    if nv12.len() != len {
        // Every byte below is written unconditionally, so re-zeroing a
        // correctly sized buffer would be a 3 MB memset per frame for
        // nothing.
        nv12.clear();
        nv12.resize(len, 0);
    }
    // Integer rows are disjoint and chroma is sampled 2x2 inside a band, so
    // horizontal bands are an exact split: no cross-thread reads, no shared
    // state, identical output regardless of thread count.
    let threads = if width * height >= PARALLEL_CONVERT_MIN_PIXELS {
        std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(1)
    } else {
        1
    };
    if threads > 1 {
        // Bands are even-height so every band owns whole chroma rows.
        let rows_per_band = (height.div_ceil(threads) + 1) & !1;
        let (y_plane, uv_plane) = nv12.split_at_mut(width * height);
        std::thread::scope(|scope| {
            let mut y_rest: &mut [u8] = y_plane;
            let mut uv_rest: &mut [u8] = uv_plane;
            let mut y0 = 0usize;
            while y0 < height {
                let y1 = (y0 + rows_per_band).min(height);
                let (y_band, y_tail) = y_rest.split_at_mut((y1 - y0) * width);
                y_rest = y_tail;
                let uv_len = (y1 - y0) / 2 * width;
                let (uv_band, uv_tail) = uv_rest.split_at_mut(uv_len);
                uv_rest = uv_tail;
                scope.spawn(move || convert_band(bgra, width, y0, y1, y_band, uv_band));
                y0 = y1;
            }
        });
        return Ok(());
    }
    let (y_plane, uv_plane) = nv12.split_at_mut(width * height);
    convert_band(bgra, width, 0, height, y_plane, uv_plane);
    Ok(())
}

#[allow(dead_code)] // target-specific hardware backend helper
/// BT.601 limited-range BGRA→NV12 for rows `[y0, y1)` of one picture.
fn convert_band(
    bgra: &[u8],
    width: usize,
    y0: usize,
    y1: usize,
    y_band: &mut [u8],
    uv_band: &mut [u8],
) {
    for y in y0..y1 {
        let row = &bgra[y * width * 4..];
        let out = &mut y_band[(y - y0) * width..];
        for (x, y_out) in out[..width].iter_mut().enumerate() {
            let p = x * 4;
            let b = row[p] as i32;
            let g = row[p + 1] as i32;
            let r = row[p + 2] as i32;
            *y_out = (16 + ((66 * r + 129 * g + 25 * b + 128) >> 8)).clamp(0, 255) as u8;
            if y % 2 == 0 && x % 2 == 0 {
                let u = 128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8);
                let v = 128 + ((112 * r - 94 * g - 18 * b + 128) >> 8);
                let uv = ((y - y0) / 2) * (width / 2) + x / 2;
                uv_band[uv * 2] = u.clamp(0, 255) as u8;
                uv_band[uv * 2 + 1] = v.clamp(0, 255) as u8;
            }
        }
    }
}

/// One decoder backend, chosen once per receiving sender.
enum DecoderBackend {
    Software(Decoder),
    // The unit tests deliberately construct only Software (see
    // VideoDecoder::new), so these variants are live in non-test builds.
    #[cfg(all(feature = "vaapi", target_os = "linux"))]
    #[cfg_attr(test, allow(dead_code))]
    Vaapi(crate::video_vaapi_decode::VaapiDecoder),
    #[cfg(windows)]
    #[cfg_attr(test, allow(dead_code))]
    Windows(crate::video_windows::MfDecoder),
    #[cfg(target_os = "android")]
    #[cfg_attr(test, allow(dead_code))]
    Android(crate::video_android::MediaCodecDecoder),
}

impl DecoderBackend {
    fn is_hardware(&self) -> bool {
        !matches!(self, Self::Software(_))
    }
}

/// Wraps the best available H.264 decoder. One per receiving sender.
///
/// Hardware decode is tried first on builds that carry a platform backend
/// (VA-API, Media Foundation, MediaCodec). Any init failure — missing driver,
/// unsupported profile, no hardware codec — falls back to openh264 without
/// disturbing the stream. A hardware error later in the stream rebuilds the
/// stream's decoder as software on the next frame, so one bad surface cannot
/// leave a viewer permanently black.
pub struct VideoDecoder {
    backend: DecoderBackend,
}

impl VideoDecoder {
    pub fn new() -> Result<Self, Error> {
        // Unit tests exercise the portable software contract. The hardware
        // backends buffer a picture (VA-API releases its tail on flush), so
        // the one-frame-in/one-picture-out assertions below would be
        // testing a different contract. Each hardware backend has its own
        // round-trip test instead.
        #[cfg(all(not(test), feature = "vaapi", target_os = "linux"))]
        match crate::video_vaapi_decode::VaapiDecoder::new() {
            Ok(decoder) => {
                eprintln!("video: using VA-API H.264 decode");
                return Ok(Self {
                    backend: DecoderBackend::Vaapi(decoder),
                });
            }
            Err(e) => eprintln!("video: VA-API decode unavailable ({e}); using software decode"),
        }
        #[cfg(all(not(test), windows))]
        match crate::video_windows::MfDecoder::new() {
            Ok(decoder) => {
                eprintln!("video: using Media Foundation H.264 decode");
                return Ok(Self {
                    backend: DecoderBackend::Windows(decoder),
                });
            }
            Err(e) => {
                eprintln!("video: Media Foundation decode unavailable ({e}); using software decode")
            }
        }
        #[cfg(all(not(test), target_os = "android"))]
        match crate::video_android::MediaCodecDecoder::new() {
            Ok(decoder) => {
                eprintln!("video: using MediaCodec H.264 decode");
                return Ok(Self {
                    backend: DecoderBackend::Android(decoder),
                });
            }
            Err(e) => {
                eprintln!("video: MediaCodec decode unavailable ({e}); using software decode")
            }
        }
        let decoder =
            Decoder::with_api_config(openh264::OpenH264API::from_source(), DecoderConfig::new())
                .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self {
            backend: DecoderBackend::Software(decoder),
        })
    }

    fn decode_impl(
        &mut self,
        frame: &[u8],
        rgba: bool,
    ) -> Result<Option<(Vec<u8>, usize, usize)>, Error> {
        match &mut self.backend {
            DecoderBackend::Software(decoder) => {
                match decoder
                    .decode(frame)
                    .map_err(|e| Error::Codec(e.to_string()))?
                {
                    Some(yuv) => {
                        let (w, h) = yuv.dimensions();
                        if rgba {
                            let mut out = vec![0u8; w * h * 4];
                            yuv.write_rgba8(&mut out);
                            Ok(Some((out, w, h)))
                        } else {
                            let mut out = vec![0u8; w * h * 3];
                            yuv.write_rgb8(&mut out);
                            Ok(Some((out, w, h)))
                        }
                    }
                    None => Ok(None),
                }
            }
            #[cfg(all(feature = "vaapi", target_os = "linux"))]
            DecoderBackend::Vaapi(decoder) => decoder.decode(frame, rgba),
            #[cfg(windows)]
            DecoderBackend::Windows(decoder) => decoder.decode(frame, rgba),
            #[cfg(target_os = "android")]
            DecoderBackend::Android(decoder) => decoder.decode(frame, rgba),
        }
    }

    /// Decode one Annex-B frame, returning RGB8 data plus the decoded
    /// dimensions — the SPS decides, not the caller. The viewer needs
    /// these to size its display for sender-negotiated resolutions
    /// (5d: real capture sends whatever the compositor serves).
    pub fn decode_with_dims(
        &mut self,
        frame: &[u8],
    ) -> Result<Option<(Vec<u8>, usize, usize)>, Error> {
        match self.decode_impl(frame, false) {
            Ok(frame) => Ok(frame),
            Err(e) if self.backend.is_hardware() => {
                eprintln!("video: hardware decode failed ({e}); falling back to software");
                let decoder = Decoder::with_api_config(
                    openh264::OpenH264API::from_source(),
                    DecoderConfig::new(),
                )
                .map_err(|e| Error::Codec(e.to_string()))?;
                self.backend = DecoderBackend::Software(decoder);
                self.decode_impl(frame, false)
            }
            Err(e) => Err(e),
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
        match self.decode_impl(frame, true) {
            Ok(frame) => Ok(frame),
            Err(e) if self.backend.is_hardware() => {
                eprintln!("video: hardware decode failed ({e}); falling back to software");
                let decoder = Decoder::with_api_config(
                    openh264::OpenH264API::from_source(),
                    DecoderConfig::new(),
                )
                .map_err(|e| Error::Codec(e.to_string()))?;
                self.backend = DecoderBackend::Software(decoder);
                self.decode_impl(frame, true)
            }
            Err(e) => Err(e),
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
#[cfg(any(feature = "vaapi", windows, target_os = "android"))]
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
    // Hardware encoders are allowed to buffer the first access units while
    // their pipeline primes. Feed a short sequence and take the first
    // keyframe that actually comes out; requiring output from frame one
    // rejected otherwise-perfect MediaCodec and Media Foundation encoders.
    let mut key_annexb = None;
    for _ in 0..12 {
        let (annexb, keyframe) = enc.encode_bgra(&bgra)?;
        if keyframe && !annexb.is_empty() {
            key_annexb = Some(annexb);
            break;
        }
    }
    let Some(annexb) = key_annexb else {
        return Err(Error::Codec(
            "hardware encoder produced no keyframe in its first 12 frames".into(),
        ));
    };
    // Validate with the portable software decoder, not the receiver's
    // hardware backend. VA-API decoders are allowed to hold a picture in
    // the DPB until the next access unit or an explicit flush (the Vega
    // decode path does exactly that), so a fresh hardware decoder can
    // legitimately return `None` for a perfectly decodable IDR. The
    // software decoder has no such pipeline delay and is the same
    // fallback a viewer uses if hardware decode fails.
    let mut dec =
        Decoder::with_api_config(openh264::OpenH264API::from_source(), DecoderConfig::new())
            .map_err(|e| Error::Codec(format!("fresh software decoder init failed: {e}")))?;
    let mut last_error = None;
    for _ in 0..3 {
        match dec.decode(&annexb) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(e) => last_error = Some(e.to_string()),
        }
    }
    Err(Error::Codec(match last_error {
        Some(e) => format!("keyframe rejected by a fresh decoder: {e}"),
        None => "keyframe decoded to no picture after three software feeds — a mid-stream \
                 receiver could never start"
            .into(),
    }))
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
#[cfg(any(feature = "vaapi", windows, target_os = "android"))]
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
#[cfg(any(feature = "vaapi", windows, target_os = "android"))]
fn accept_hardware_encoder(
    mut encoder: Box<dyn VideoEncoder>,
    backend: &str,
    width: usize,
    height: usize,
) -> Option<Box<dyn VideoEncoder>> {
    match encoder_produces_decodable_keyframe(&mut *encoder) {
        Ok(()) => {
            // The self-test says the stream is usable; this measures the
            // sustained rate through the same path. target_fps() caps the
            // pacing at 95% of it, so a share does not schedule frames the
            // encoder cannot produce.
            let measured = measure_encoder_fps(&mut *encoder);
            // The tests consumed frames and left the encoder mid-GOP: make
            // the first REAL frame an IDR.
            encoder.force_keyframe();
            HW_ACTIVE.store(true, Ordering::Relaxed);
            if let Some(fps) = measured {
                HW_MEASURED_MFPS.store((fps * 1000.0) as u32, Ordering::Relaxed);
            } else {
                HW_MEASURED_MFPS.store(0, Ordering::Relaxed);
            }
            eprintln!(
                "video: hardware ({backend}) encoder selected — {width}x{height}, self-test \
                 {:.1} fps, pacing up to {:.0} fps at {} bps, keyframe self-test passed \
                 (verified decodable by a fresh decoder)",
                measured.unwrap_or(0.0),
                target_fps(),
                bitrate_for(width, height, target_fps())
            );
            Some(encoder)
        }
        Err(e) => {
            eprintln!(
                "video: {backend} encoder self-test FAILED at {width}x{height} ({e}); falling back \
                 to software — a hardware share would have shown every viewer an empty window"
            );
            None
        }
    }
}

pub fn create_encoder_auto(width: usize, height: usize) -> Result<Box<dyn VideoEncoder>, Error> {
    create_encoder_auto_impl(width, height, false)
}

/// Construct an encoder that can take DMA-BUF frames directly.
///
/// A DMA-BUF capture source hands the encoder producer-owned GPU memory;
/// the software backend cannot read that memory. Falling back to it would
/// leave a share that looks active but never sends another frame, so this
/// variant fails instead of silently downgrading.
pub fn create_encoder_auto_dmabuf(
    width: usize,
    height: usize,
) -> Result<Box<dyn VideoEncoder>, Error> {
    create_encoder_auto_impl(width, height, true)
}

fn create_encoder_auto_impl(
    width: usize,
    height: usize,
    require_dmabuf: bool,
) -> Result<Box<dyn VideoEncoder>, Error> {
    let requested = (width, height);
    if hw_encode_enabled() {
        #[cfg(all(feature = "vaapi", target_os = "linux"))]
        {
            match crate::video_vaapi::VaapiEncoder::new_profile(
                width,
                height,
                VIDEO_FPS_HARDWARE,
                bitrate_for(width, height, VIDEO_FPS_HARDWARE),
            ) {
                Ok(e) => {
                    if let Some(e) = accept_hardware_encoder(Box::new(e), "VAAPI", width, height) {
                        return Ok(e);
                    }
                }
                Err(e) => eprintln!("video: VAAPI init failed ({e}); falling back to software"),
            }
        }
        #[cfg(windows)]
        {
            match crate::video_windows::MfEncoder::new(
                width,
                height,
                VIDEO_FPS_HARDWARE,
                bitrate_for(width, height, VIDEO_FPS_HARDWARE),
            ) {
                Ok(e) => {
                    if let Some(e) =
                        accept_hardware_encoder(Box::new(e), "Media Foundation", width, height)
                    {
                        return Ok(e);
                    }
                }
                Err(e) => {
                    eprintln!("video: Media Foundation init failed ({e}); falling back to software")
                }
            }
        }
        #[cfg(target_os = "android")]
        {
            match crate::video_android::MediaCodecEncoder::new(
                width,
                height,
                VIDEO_FPS_HARDWARE,
                bitrate_for(width, height, VIDEO_FPS_HARDWARE),
            ) {
                Ok(e) => {
                    if let Some(e) =
                        accept_hardware_encoder(Box::new(e), "MediaCodec", width, height)
                    {
                        return Ok(e);
                    }
                }
                Err(e) => {
                    eprintln!("video: MediaCodec init failed ({e}); falling back to software")
                }
            }
        }
        #[cfg(not(any(feature = "vaapi", windows, target_os = "android")))]
        {
            eprintln!(
                "video: hw-encode requested but this build has no hardware encoder backend; \
                 using software"
            );
        }
    }
    if require_dmabuf {
        return Err(Error::Codec(
            "DMA-BUF capture requires a working hardware encoder; refusing to fall back \
             to software (which would silently stop sending frames)"
                .into(),
        ));
    }
    HW_ACTIVE.store(false, Ordering::Relaxed);
    HW_MEASURED_MFPS.store(0, Ordering::Relaxed);
    let (width, height) = fit_software_dims(requested.0, requested.1);
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

    #[test]
    fn avcc_first_length_that_looks_like_a_start_code_is_still_avcc() {
        // 0x0000012c is a valid 4-byte AVCC length and also starts with
        // 00 00 01. The old first-bytes heuristic handed this access unit
        // to the transport unchanged, where every viewer read a length as a
        // start code.
        let mut access = vec![0, 0, 1, 0x2c];
        access.push(0x65); // IDR NAL header
        access.extend(std::iter::repeat_n(0x88, 299));
        let mut out = Vec::new();
        assert!(h264_to_annexb(&access, &mut out));
        assert_eq!(out, [&[0, 0, 0, 1][..], &access[4..]].concat());
    }

    #[test]
    fn annexb_passes_through_unchanged() {
        let access = [0, 0, 0, 1, 0x65, 0x88, 0x84, 0, 0, 1, 0x06, 0x05];
        let mut out = Vec::new();
        assert!(h264_to_annexb(&access, &mut out));
        assert_eq!(out, access);
    }

    #[test]
    fn dmabuf_encoder_never_silently_uses_software() {
        // DMA-BUF capture hands producer-owned GPU memory to the encoder;
        // the software backend cannot read it. A fallback would leave a
        // share "active" that never sends another frame.
        super::set_hw_encode(false);
        assert!(super::create_encoder_auto_dmabuf(64, 64).is_err());
    }

    #[test]
    fn odd_i420_chroma_uses_ceil_dimensions() {
        // 3x3 I420 has ceil(3/2)^2 = 4 chroma samples per plane, not 1.
        let y = vec![100u8; 9];
        let u = vec![128u8; 4];
        let v = vec![128u8; 4];
        let rgb = i420_to_rgb(&y, &u, &v, 3, 3, false);
        assert_eq!(rgb.len(), 3 * 3 * 3);
        assert!(rgb.iter().any(|b| *b != 0));
    }

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
        // Under the cap as well as over it: openh264 rejects an odd width,
        // and a window capture is not required to be even.
        assert_eq!(fit_software_dims(1001, 701), (1000, 700));
        assert_eq!(fit_software_dims(1001, 1440), (750, 1080));
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
    fn mismatched_frame_is_an_error_not_a_panic() {
        let mut enc = SoftwareEncoder::new(64, 48).unwrap();
        let expected = 64 * 48 * 4;
        let too_big = vec![0u8; expected + 4];
        let too_small = vec![0u8; expected - 4];
        assert!(enc.encode_bgra(&too_big).is_err());
        assert!(enc.encode_bgra(&too_small).is_err());
        let mut out = Vec::new();
        assert!(
            enc.encode_bgra_strided(&too_small, 64, 48, 64 * 4, &mut out)
                .is_err()
        );
        assert!(
            enc.encode_bgra_strided(&too_small, 64, 48, 10, &mut out)
                .is_err()
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
