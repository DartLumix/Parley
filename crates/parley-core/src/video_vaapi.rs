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

use crate::error::Error;
use crate::video::{CodecId, KEYFRAME_INTERVAL_FRAMES, VIDEO_BITRATE_BPS, VIDEO_FPS, VideoEncoder};

/// BGRA (0xAABBGGRR little-endian) → NV12 (4:2:0, Y then interleaved
/// UV), BT.601 limited range — the conversion the other agent (rightly)
/// insisted on. `width` must be even (VCE 4:2:0); callers negotiate
/// even sizes from PipeWire in practice.
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize) -> Result<Vec<u8>, Error> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) {
        return Err(Error::Codec(format!(
            "bgra_to_nv12: {width}x{height} — width must be non-zero and even"
        )));
    }
    let expected = width * height * 4;
    if bgra.len() < expected {
        return Err(Error::Codec(format!(
            "bgra_to_nv12: {} bytes, {width}x{height} needs {expected}",
            bgra.len()
        )));
    }
    let mut nv12 = vec![0u8; width * height + (width / 2) * (height / 2) * 2];
    let (y_plane, uv_plane) = nv12.split_at_mut(width * height);
    for y in 0..height {
        for x in 0..width {
            let p = (y * width + x) * 4;
            let b = bgra[p] as i32;
            let g = bgra[p + 1] as i32;
            let r = bgra[p + 2] as i32;
            // BT.601 limited: Y = 16 + (66R + 129G + 25B) >> 8, etc.
            let yy = 16 + ((66 * r + 129 * g + 25 * b + 128) >> 8);
            y_plane[y * width + x] = yy.clamp(0, 255) as u8;
            if y % 2 == 0 && x % 2 == 0 {
                let u = 128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8);
                let v = 128 + ((112 * r - 94 * g - 18 * b + 128) >> 8);
                let uv = (y / 2) * (width / 2) + x / 2;
                uv_plane[uv * 2] = u.clamp(0, 255) as u8;
                uv_plane[uv * 2 + 1] = v.clamp(0, 255) as u8;
            }
        }
    }
    Ok(nv12)
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
    pub fn new(width: usize, height: usize) -> Result<Self, Error> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) {
            return Err(Error::Codec(format!(
                "vaapi encoder: {width}x{height} rejected (width must be even, non-zero)"
            )));
        }
        let cfg = VaConfig::new(
            width as u32,
            height as u32,
            VIDEO_FPS as u32,
            VIDEO_BITRATE_BPS,
            KEYFRAME_INTERVAL_FRAMES,
        );
        let enc = VaEncoder::new(cfg).map_err(|e| Error::Codec(format!("vaapi init: {e:#}")))?;
        Ok(Self {
            inner: Some(enc),
            created_thread: std::thread::current().id(),
            width,
            height,
            current_bitrate: VIDEO_BITRATE_BPS,
            nv12: Vec::new(),
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
        self.nv12 = bgra_to_nv12(bgra, self.width, self.height)?;
        let idr = self.pending_idr;
        self.pending_idr = false;
        let nv12 = std::mem::take(&mut self.nv12);
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

    /// 7g strided input: repack via the shared fallback (the VAAPI
    /// upload wants tight NV12 anyway; the zero-copy DMA-BUF path is a
    /// later optimization).
    fn encode_bgra_strided(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        out: &mut Vec<u8>,
    ) -> Result<bool, Error> {
        crate::video::strided_fallback(self, bgra, width, height, stride, out)
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
