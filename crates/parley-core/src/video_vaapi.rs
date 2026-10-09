//! 7g: opt-in VAAPI H.264 hardware encoder (`VideoEncoder` backend).
//!
//! §8 CONTAINMENT: on the desktop's polaris10 (RX 590) the VCE encode
//! ring has wedged the GPU twice — the rustdesk incident, and our own
//! first-party contained probe (standalone binary, H.264 profile, NV12
//! input, MB-aligned 720p — the "wrong profile / RGB fed to VCE"
//! hypotheses are both falsified; see HANDOFF §8). This module exists
//! ONLY behind the `vaapi` cargo feature AND the runtime `--hw-encode`
//! flag, and selection happens once at encoder creation with fallback
//! to software on any init error. A wedge mid-stream is a kernel event
//! no userspace fallback can survive — that risk is what the flag
//! accepts, knowingly, on non-polaris hardware.
//!
//! Input contract: same BGRA bytes the software path takes (tight, no
//! stride). Conversion BGRA→NV12 (BT.601 limited, matching what a
//! default pipeline assumes for unlabelled SD/HD) runs on the CPU here
//! — the upload is the only DMA to the encoder surface. A zero-copy
//! DMA-BUF path from PipeWire is the future step once this is proven.

use moq_vaapi::encode::{Config as VaConfig, Encoder as VaEncoder};

/// Frame size below which the conversion stays single-threaded: thread
/// hand-off costs more than it saves on small pictures, and a 720p frame
/// already converts in ~4.5 ms on one core.
const PARALLEL_CONVERT_MIN_PIXELS: usize = 1280 * 720;

use crate::error::Error;
use crate::video::{
    CodecId, VIDEO_FPS_SOFTWARE, VideoEncoder, bitrate_for, keyframe_interval_frames,
};

/// BGRA (0xAABBGGRR little-endian) → NV12 (4:2:0, Y then interleaved
/// UV), BT.601 limited range — the conversion the other agent (rightly)
/// insisted on. `width` must be even (VCE 4:2:0); callers negotiate
/// even sizes from PipeWire in practice.
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize) -> Result<Vec<u8>, Error> {
    let mut nv12 = Vec::new();
    bgra_to_nv12_into(bgra, width, height, &mut nv12)?;
    Ok(nv12)
}

/// 7h: [`bgra_to_nv12`] into a CALLER-OWNED buffer — the encoder's hot
/// path reuses one staging buffer for the life of the encoder instead
/// of allocating and zero-filling a fresh 3 MB Vec (1080p) on every
/// frame at 60 fps. The buffer is resized to the exact NV12 length, so
/// a resolution change needs no special case.
pub fn bgra_to_nv12_into(
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
        // Only when the geometry changed: every byte below is written
        // unconditionally, so re-zeroing a correctly sized buffer would
        // be a 3 MB memset per frame for nothing.
        nv12.clear();
        nv12.resize(len, 0);
    }
    // 7h HD: 1080p60 needs this conversion under ~8 ms and 1440p60 under
    // ~5 — a single scalar pass costs 10 and 18 (measured), which is
    // most of the frame budget on its own. Integer rows are disjoint and
    // chroma is sampled 2x2 inside a band, so horizontal bands are an
    // exact split: no cross-thread reads, no shared state, identical
    // output regardless of thread count.
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
                // The two halves were split off the same buffer, so each
                // band can take a disjoint slice of each; `split_at_mut`
                // yields independent borrows, which is what lets the
                // closures below run concurrently.
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

/// BT.601 limited-range BGRA→NV12 for rows `[y0, y1)` of one picture:
/// `y_band` holds exactly those luma rows, `uv_band` the matching
/// interleaved chroma rows (`height / 2` rows of `width` bytes). Chroma
/// is point-sampled from the top-left pixel of each 2x2 block — the
/// same rule the single-threaded version used, so the two produce
/// identical bytes.
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
            // BT.601 limited: Y = 16 + (66R + 129G + 25B) >> 8, etc.
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

/// VAAPI H.264 encoder. Constructed only via
/// [`crate::video::create_encoder_auto`] under the `vaapi` feature with
/// hardware preference on; init failure NEVER reaches callers (they
/// get the software encoder instead).
///
/// Send story: moq_vaapi's Encoder holds `Rc`s (libva contexts are
/// single-thread-bound in the crate's model) so it is !Send. Parley's
/// `Box<dyn VideoEncoder>` requires Send. The wrapper below asserts —
/// in debug builds, on every encode — that it stays on the thread that
/// created it. The screen.rs encoder thread creates AND uses the
/// encoder on one thread (5e architecture), so the invariant holds by
/// construction; a future caller that moved the encoder across threads
/// would panic in debug rather than corrupt GPU state.
pub struct VaapiEncoder {
    inner: Option<VaEncoder>,
    created_thread: std::thread::ThreadId,
    width: usize,
    height: usize,
    current_bitrate: u32,
    /// Reused NV12 staging buffer (one alloc, reused per frame).
    nv12: Vec<u8>,
    /// 7h: reused BGRA repack buffer for strided input (PipeWire hands
    /// us row-padded rows; the VA-API upload wants tight NV12, so the
    /// rows are packed first — into this buffer, not a fresh Vec per
    /// frame).
    packed: Vec<u8>,
    /// force_keyframe latched here; the next encode passes it to
    /// moq-vaapi (its `keyframe` argument drives the IDR).
    pending_idr: bool,
}

// SAFETY: VaEncoder is !Send because libva context Rcs are not
// cross-thread in moq-vaapi's model. Parley's usage contract (encoder
// created and used on the same encoder thread — 5e) makes moving it
// impossible; debug-build thread assertions in `enc()` enforce this.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    /// Software-equivalent profile (30 fps, 720p30-anchored bitrate) —
    /// the shape the keyframe-shape test and any non-HD caller gets.
    pub fn new(width: usize, height: usize) -> Result<Self, Error> {
        Self::new_profile(
            width,
            height,
            VIDEO_FPS_SOFTWARE,
            bitrate_for(width, height, VIDEO_FPS_SOFTWARE),
        )
    }

    /// 7h HD: the explicit profile. [`crate::video::create_encoder_auto`]
    /// asks for the hardware profile (60 fps, resolution-scaled bitrate),
    /// which is what makes 1080p60/1440p60 reachable: the frame rate is a
    /// VA-API config parameter here, not just our pacing, and it drives
    /// both the driver's rate-control window and the SPS's level/VUI.
    pub fn new_profile(width: usize, height: usize, fps: f32, bitrate: u32) -> Result<Self, Error> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Error::Codec(format!(
                "vaapi encoder: {width}x{height} rejected (width and height must be even and \
                 non-zero — VCE codes 4:2:0 macroblocks); falling back to software"
            )));
        }
        if !(1.0..=240.0).contains(&fps) {
            return Err(Error::Codec(format!(
                "vaapi encoder: frame rate {fps} out of range"
            )));
        }
        let cfg = VaConfig::new(
            width as u32,
            height as u32,
            fps as u32,
            bitrate,
            keyframe_interval_frames(fps),
        );
        let enc = VaEncoder::new(cfg).map_err(|e| Error::Codec(format!("vaapi init: {e:#}")))?;
        Ok(Self {
            inner: Some(enc),
            created_thread: std::thread::current().id(),
            width,
            height,
            current_bitrate: bitrate,
            nv12: Vec::new(),
            packed: Vec::new(),
            pending_idr: false,
        })
    }

    /// Borrow the encoder after checking the thread invariant.
    fn enc(&mut self) -> &mut VaEncoder {
        debug_assert_eq!(
            std::thread::current().id(),
            self.created_thread,
            "VaapiEncoder used from a different thread than it was created on — \
             moq-vaapi/libva contexts are thread-bound; fix the caller"
        );
        self.inner
            .as_mut()
            .expect("encoder present for its lifetime")
    }
}

impl VideoEncoder for VaapiEncoder {
    fn codec(&self) -> CodecId {
        CodecId::H264
    }

    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn encode_bgra(&mut self, bgra: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        // The Vec is MOVED out (not copied) because `encode_nv12`
        // borrows the buffer while `self.enc()` borrows self; its
        // allocation is reused every frame — the conversion writes into
        // it in place, so the hot path allocates nothing.
        let mut nv12 = std::mem::take(&mut self.nv12);
        if let Err(e) = bgra_to_nv12_into(bgra, self.width, self.height, &mut nv12) {
            self.nv12 = nv12;
            return Err(e);
        }
        let idr = self.pending_idr;
        self.pending_idr = false;
        let result = self
            .enc()
            .encode_nv12(&nv12, idr)
            .map_err(|e| Error::Codec(format!("vaapi encode: {e:#}")));
        self.nv12 = nv12;
        let annexb = result?;
        // moq-vaapi doesn't report the frame type back; the ladder's
        // IDR bookkeeping only needs "was this one forced". The first
        // frame of a fresh encoder is an IDR by construction; report
        // keyframe only for forced IDRs so the wire flag matches what
        // the software path emits (periodic GOP IDRs are NOT flagged —
        // the assembler only needs the forced/first-frame signal).
        Ok((annexb, idr))
    }

    /// 7g strided input: repack into the REUSED buffer, then convert
    /// (the VA-API upload wants tight NV12 anyway; the zero-copy DMA-BUF
    /// path is a later optimization).
    ///
    /// 7h HD: `strided_fallback` allocated a fresh `width*height*4` Vec
    /// per frame — 8 MB at 1080p, on the share thread, every frame —
    /// which at 60 fps is half a GB a second of allocator traffic for a
    /// buffer whose lifetime is one function call. A tight frame (the
    /// strided-buffer case a caller reaches when it already packed) skips
    /// the repack entirely.
    fn encode_bgra_strided(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        out: &mut Vec<u8>,
    ) -> Result<bool, Error> {
        if width != self.width || height != self.height {
            // Renegotiated size: the plain path re-derives dims from
            // self.width/height and never mismatches.
            return crate::video::strided_fallback(self, bgra, width, height, stride, out);
        }
        let (bytes, keyframe) = if stride == width * 4 {
            self.encode_bgra(bgra)?
        } else {
            let mut packed = std::mem::take(&mut self.packed);
            let packed_ok = {
                packed.clear();
                packed.reserve(width * height * 4);
                let mut ok = true;
                for y in 0..height {
                    match bgra.get(y * stride..y * stride + width * 4) {
                        Some(row) => packed.extend_from_slice(row),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                ok
            };
            if !packed_ok {
                self.packed = packed;
                return Err(Error::Codec("strided frame truncated".into()));
            }
            let res = self.encode_bgra(&packed);
            self.packed = packed;
            res?
        };
        out.clear();
        out.extend_from_slice(&bytes);
        Ok(keyframe)
    }

    fn force_keyframe(&mut self) {
        // Latched; the next encode passes it as moq-vaapi's keyframe
        // argument (a forced IDR; the first frame is one by default).
        self.pending_idr = true;
    }

    fn set_bitrate(&mut self, bps: u32) {
        if bps == self.current_bitrate {
            return;
        }
        // moq-vaapi rate-control changes take effect between frames —
        // no encoder recreation, no IDR forced (its RC window is 1.5 s).
        match self.enc().set_bitrate(bps) {
            Ok(()) => self.current_bitrate = bps,
            Err(e) => eprintln!(
                "video: vaapi bitrate change to {bps} failed ({e:#}); keeping {}",
                self.current_bitrate
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_sizes_and_structure() {
        let (w, h) = (64, 48);
        let bgra = vec![128u8; w * h * 4];
        let nv = bgra_to_nv12(&bgra, w, h).unwrap();
        assert_eq!(nv.len(), w * h + (w / 2) * (h / 2) * 2);
        // All-gray 128: BT.601 limited Y = 16 + (220·128+128)>>8 = 126,
        // U/V neutral 128.
        assert_eq!(nv[0], 126, "Y for gray-128");
        assert_eq!(nv[w * h], 128, "U neutral");
        assert_eq!(nv[w * h + 1], 128, "V neutral");
    }

    #[test]
    fn rejects_odd_width_and_short_input() {
        let bgra = vec![0u8; 64 * 48 * 4];
        assert!(bgra_to_nv12(&bgra, 63, 48).is_err());
        assert!(bgra_to_nv12(&bgra[..100], 64, 48).is_err());
    }

    #[test]
    fn red_pixel_maps_to_cr_dominant() {
        // Pure red: Y≈81, U≈90, V≈240 (BT.601 limited).
        let (w, h) = (2, 2);
        let mut bgra = vec![0u8; 16];
        for px in bgra.chunks_exact_mut(4) {
            px[2] = 255; // R
            px[3] = 255;
        }
        let nv = bgra_to_nv12(&bgra, w, h).unwrap();
        assert!((nv[0] as i32 - 81).abs() <= 3, "Y = {}", nv[0]);
        let u = nv[4] as i32;
        let v = nv[5] as i32;
        assert!((u - 90).abs() <= 4, "U = {u}");
        assert!((v - 240).abs() <= 4, "V = {v}");
    }
}
