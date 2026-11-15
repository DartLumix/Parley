//! 7g: opt-in VAAPI H.264 hardware encoder (`VideoEncoder` backend).
//!
//! §8 CONTAINMENT: on the desktop's polaris10 (RX 590) the VCE encode
//! ring has wedged the GPU twice — the rustdesk incident, and our own
//! first-party contained probe (standalone binary, H.264 profile, NV12
//! input, MB-aligned 720p — the "wrong profile / RGB fed to VCE"
//! hypotheses are both falsified; see docs/hd-encode-runs.md). This module exists
//! ONLY behind the `vaapi` cargo feature AND the runtime `--hw-encode`
//! flag, and selection happens once at encoder creation with fallback
//! to software on any init error. A wedge mid-stream is a kernel event
//! no userspace fallback can survive — that risk is what the flag
//! accepts, knowingly, on non-polaris hardware.
//!
//! Input contract: same BGRA bytes the software path takes, packed rows
//! with a caller-supplied stride. There is no CPU color conversion on this
//! path: the BGRA rows are uploaded into a packed-RGB surface with the
//! producer's own pitch and the VA-API video processor converts them to
//! NV12 in the encoder's color space (7i). The previous code converted on
//! the CPU, which cost 12 ms of a 1080p frame's budget — more than the
//! encode itself.
//!
//! A zero-copy DMA-BUF path (PipeWire handing us the compositor's buffer
//! instead of a memfd) is the next step; it needs the capture side to hold
//! the producer's buffer until the GPU has read it, which is a thread-model
//! change, not an encoder change.

use moq_vaapi::encode::{Config as VaConfig, Encoder as VaEncoder};

use crate::error::Error;
use crate::video::{
    CodecId, DmaBufFrame, VIDEO_FPS_SOFTWARE, VideoEncoder, bitrate_for, keyframe_interval_frames,
};

// Kept at this path for the encode-cost integration test.
pub use crate::video::bgra_to_nv12;

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
    /// force_keyframe latched here; the next encode passes it to
    /// moq-vaapi (its `keyframe` argument drives the IDR).
    pending_idr: bool,
    /// GOP position tracked here because the crate does not report the
    /// encoded slice type. Without this, periodic IDRs were sent with
    /// `keyframe = false`, so a receiver that lost a delta never knew a
    /// fresh reference point had arrived.
    gop: u32,
    frames_since_keyframe: u32,
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
            pending_idr: false,
            gop: keyframe_interval_frames(fps),
            frames_since_keyframe: 0,
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

    /// Decide whether this picture is an IDR. The first picture is one,
    /// a forced request is one, and the wall-clock GOP boundary is one.
    /// Passing a periodic IDR as `keyframe` to the crate keeps its own
    /// counter aligned with this one, so it never emits an unreported
    /// IDR between our two boundaries.
    fn take_idr(&mut self) -> bool {
        let idr = self.pending_idr
            || self.frames_since_keyframe == 0
            || self.frames_since_keyframe >= self.gop;
        self.pending_idr = false;
        // The picture just emitted IS the keyframe. Counting it as frame 0
        // of the new group would make `frames_since_keyframe == 0` true on
        // the very next call too, turning every frame into an IDR.
        self.frames_since_keyframe = if idr {
            1
        } else {
            self.frames_since_keyframe.saturating_add(1)
        };
        idr
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
        let expected = self.width.saturating_mul(self.height).saturating_mul(4);
        if bgra.len() != expected {
            return Err(Error::Codec(format!(
                "vaapi encoder is {}x{} ({expected} bytes), got {}",
                self.width,
                self.height,
                bgra.len()
            )));
        }
        // 7i: no CPU conversion and no repack — the crate uploads packed
        // BGRA rows straight into a BGRX surface (honoring the pitch) and
        // the video processor produces the NV12 the encoder consumes.
        let idr = self.take_idr();
        let (width, height) = (self.width, self.height);
        let annexb = self
            .enc()
            .encode_bgra(bgra, width as u32, height as u32, width * 4, idr)
            .map_err(|e| Error::Codec(format!("vaapi encode: {e:#}")))?;
        // `take_idr` models the crate's own GOP boundary as well, so a
        // periodic IDR is flagged on the wire and a receiver can resume
        // from it rather than waiting for the next forced one.
        Ok((annexb, idr))
    }

    /// 7g strided input: rows are uploaded with their own pitch — the
    /// producer's stride is exactly what the driver's image plane takes,
    /// so there is no repack step at all (7i).
    ///
    /// 7h HD: this used to allocate a fresh `width*height*4` Vec per frame
    /// to repack rows (8 MB at 1080p, half a GB a second at 60 fps); the
    /// pitch-aware upload removed both the allocation and the copy.
    fn encode_bgra_strided(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        out: &mut Vec<u8>,
    ) -> Result<bool, Error> {
        if width == 0 || height == 0 || stride < width.saturating_mul(4) {
            return Err(Error::Codec(format!(
                "vaapi strided frame {width}x{height} has invalid stride {stride}"
            )));
        }
        let needed = stride
            .saturating_mul(height.saturating_sub(1))
            .saturating_add(width.saturating_mul(4));
        if bgra.len() < needed {
            return Err(Error::Codec(format!(
                "vaapi strided frame is {} bytes, {width}x{height} at stride {stride} needs \
                 {needed}",
                bgra.len()
            )));
        }
        if width != self.width || height != self.height {
            // Renegotiated size: the plain path re-derives dims from
            // self.width/height and never mismatches.
            return crate::video::strided_fallback(self, bgra, width, height, stride, out);
        }
        let idr = self.take_idr();
        let bytes = self
            .enc()
            .encode_bgra(bgra, width as u32, height as u32, stride, idr)
            .map_err(|e| Error::Codec(format!("vaapi encode: {e:#}")))?;
        out.clear();
        out.extend_from_slice(&bytes);
        Ok(idr)
    }

    /// 7j: the zero-copy handoff. The capture side negotiated DMA-BUFs
    /// precisely because this returns true; a false here means the
    /// negotiation should never have offered them.
    fn dmabuf_supported(&self) -> bool {
        true
    }

    /// 7j: import and encode the producer's buffer in place.
    ///
    /// The crate decides between in-place (NV12 at the encoder's size)
    /// and a video-processor pass (anything else mapped, including the
    /// packed BGRX a screencast produces), so a renegotiated size still
    /// encodes — scaled — instead of failing.
    fn encode_dmabuf(&mut self, frame: DmaBufFrame, out: &mut Vec<u8>) -> Result<bool, Error> {
        let idr = self.take_idr();
        let buffer = moq_vaapi::dmabuf::DmaBuf {
            drm_format: frame.drm_format,
            modifier: frame.modifier,
            width: frame.width,
            height: frame.height,
            planes: frame
                .planes
                .iter()
                .map(|p| moq_vaapi::dmabuf::Plane {
                    offset: p.offset,
                    pitch: p.pitch,
                })
                .collect(),
            fd: frame.fd,
            // PipeWire hands out packed RGB here, whose space is the
            // stream's by definition; a YUV producer that knows its space
            // would need a field on DmaBufFrame to say so.
            color: None,
        };
        let bytes = self
            .enc()
            .encode_dmabuf(buffer, idr)
            .map_err(|e| Error::Codec(format!("vaapi encode (dmabuf): {e:#}")))?;
        out.clear();
        out.extend_from_slice(&bytes);
        Ok(idr)
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
    use crate::video::bgra_to_nv12;

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
        for px in bgra.as_chunks_mut::<4>().0 {
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
