//! FPS-first screen-share adaptation.
//!
//! The user's resolution and frame rate are the contract; bitrate is the
//! pressure valve. Only sustained encoder overload or a genuinely backed-up
//! transport may move resolution, and it moves by one small tier at a time.
//! This module is deliberately transport-agnostic: the caller feeds the
//! congestion estimate from `moq_net::bandwidth` (or the equivalent native
//! QUIC estimate) and the current outbound group depth.
//!
//! Strict priority is target FPS, then target resolution, then bitrate.

use std::time::{Duration, Instant};

/// A GOP is exactly one MoQ group and one periodic IDR interval.
pub const GOP_SECONDS: f64 = 2.0;
/// Joining viewers ask for an IDR, but never more often than this.
pub const JOIN_IDR_DEBOUNCE: Duration = Duration::from_secs(1);
/// Sustained headroom needed before stepping resolution back up.
pub const SNAP_BACK_HOLD: Duration = Duration::from_millis(750);

/// Capped-VBR nominal bits per pixel per frame. 1080p60 lands at roughly
/// 6 Mbps and 1440p60 at roughly 10 Mbps; static scenes use far less.
const NOMINAL_BITS_PER_PIXEL: f64 = 0.048;
/// The cap is 5/3 of nominal, matching the 6/10 and 10/16 Mbps examples.
const MAX_TO_NOMINAL: f64 = 5.0 / 3.0;
/// Stage 1 never takes the encoder below 40% of nominal.
pub const BITRATE_FLOOR: f64 = 0.40;
/// Stage 1 shrinks by at most this much per frame, so quality glides
/// instead of stepping audibly.
const BITRATE_DOWN_STEP: f64 = 0.90;
const BITRATE_UP_STEP: f64 = 1.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionTier {
    Full,
    Medium,
    Low,
}

impl ResolutionTier {
    pub fn percent(self) -> u32 {
        match self {
            Self::Full => 100,
            Self::Medium => 85,
            Self::Low => 75,
        }
    }

    fn down(self) -> Self {
        match self {
            Self::Full => Self::Medium,
            Self::Medium | Self::Low => Self::Low,
        }
    }

    fn up(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium | Self::Full => Self::Full,
        }
    }
}

/// One frame's measured pressure. Times are wall-clock around
/// capture + scale + encode, not just the codec call.
#[derive(Debug, Clone, Copy)]
pub struct FrameSample {
    pub encode: Duration,
    /// `moq_net::bandwidth::Consumer` estimate (or native QUIC's). `None`
    /// means unknown, and never triggers a reaction by itself.
    pub available_bps: Option<u64>,
    /// Groups/frames waiting in the transport writer. More than two is
    /// already visible latency at 60 fps.
    pub queued_frames: usize,
    /// No meaningful screen change since the previous frame. Static
    /// desktops never lose resolution.
    pub static_scene: bool,
    /// This picture is an IDR. A lone expensive IDR must not downscale.
    pub keyframe: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptationStage {
    Nominal,
    Bitrate,
    Resolution,
}

/// What the encoder should do for this frame. Dimensions change only when
/// `changed_resolution` is true; the caller may then recreate surfaces,
/// but bitrate changes never require a teardown.
#[derive(Debug, Clone, Copy)]
pub struct EncoderDecision {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub max_bitrate_bps: u64,
    pub force_idr: bool,
    pub changed_resolution: bool,
    pub stage: AdaptationStage,
}

/// Shared cross-platform two-stage controller.
#[derive(Debug, Clone)]
pub struct AdaptiveScreenController {
    target_width: u32,
    target_height: u32,
    target_fps: u32,
    nominal_bps: f64,
    max_bps: f64,
    bitrate_bps: f64,
    tier: ResolutionTier,
    stage: AdaptationStage,
    overload_frames: u8,
    snap_since: Option<Instant>,
    last_observe: Option<Instant>,
    last_idr: Option<Instant>,
}

impl AdaptiveScreenController {
    /// `target_fps` of zero falls back to 60. Dimensions must be non-zero;
    /// callers pass the native capture size when the user has no override.
    pub fn new(target_width: u32, target_height: u32, target_fps: u32) -> Self {
        let target_width = target_width.max(16);
        let target_height = target_height.max(16);
        let target_fps = if target_fps == 0 { 60 } else { target_fps };
        let nominal = nominal_bitrate(target_width, target_height, target_fps);
        Self {
            target_width,
            target_height,
            target_fps,
            nominal_bps: nominal,
            max_bps: nominal * MAX_TO_NOMINAL,
            bitrate_bps: nominal,
            tier: ResolutionTier::Full,
            stage: AdaptationStage::Nominal,
            overload_frames: 0,
            snap_since: None,
            last_observe: None,
            last_idr: None,
        }
    }

    pub fn target_width(&self) -> u32 {
        self.target_width
    }

    pub fn target_height(&self) -> u32 {
        self.target_height
    }

    pub fn target_fps(&self) -> u32 {
        self.target_fps
    }

    pub fn nominal_bitrate(&self) -> u64 {
        self.nominal_bps.round() as u64
    }

    pub fn max_bitrate(&self) -> u64 {
        self.max_bps.round() as u64
    }

    /// One IDR per two seconds: the caller uses this as `gop_size`.
    pub fn gop_frames(&self) -> u32 {
        (self.target_fps as f64 * GOP_SECONDS).round().max(1.0) as u32
    }

    pub fn tier(&self) -> ResolutionTier {
        self.tier
    }

    pub fn stage(&self) -> AdaptationStage {
        self.stage
    }

    pub fn current_dimensions(&self) -> (u32, u32) {
        match self.tier {
            ResolutionTier::Full => (self.target_width, self.target_height),
            _ => scaled_dimensions(self.target_width, self.target_height, self.tier.percent()),
        }
    }

    /// Periodic IDR schedule. The caller owns the frame counter; this just
    /// answers whether the current frame starts a group.
    pub fn periodic_idr(&self, frame_index: u64) -> bool {
        frame_index.is_multiple_of(u64::from(self.gop_frames()))
    }

    /// A viewer joined: request an IDR, debounced so a burst of joins does
    /// not turn into a column of keyframes. The first call always passes.
    pub fn request_join_idr(&mut self, now: Instant) -> bool {
        if self
            .last_idr
            .is_some_and(|last| now.duration_since(last) < JOIN_IDR_DEBOUNCE)
        {
            return false;
        }
        self.last_idr = Some(now);
        true
    }

    /// Observe one completed frame and decide the next frame's settings.
    ///
    /// Passing `now` keeps the snap-back hold deterministic in tests and
    /// lets every platform use its own monotonic clock.
    pub fn observe(&mut self, sample: FrameSample, now: Instant) -> EncoderDecision {
        let budget = 1.0 / f64::from(self.target_fps);
        let encode_s = sample.encode.as_secs_f64();
        let nominal = self.nominal_bps;
        let floor = nominal * BITRATE_FLOOR;
        let available = sample.available_bps.map(|bps| bps as f64);
        let queue_pressure = sample.queued_frames > 2;
        let bandwidth_floor = available.is_some_and(|bps| bps < floor);

        // A lone IDR is allowed to be expensive: it carries SPS/PPS and a
        // full picture. It does not count toward the overload streak, and
        // a static desktop never triggers a resolution drop at all.
        let encode_overload = encode_s > 0.90 * budget;
        if !sample.keyframe && !sample.static_scene && encode_overload {
            self.overload_frames = self.overload_frames.saturating_add(1);
        } else {
            self.overload_frames = 0;
        }

        let stage1 = available.is_some_and(|bps| bps < nominal) && encode_s <= 0.85 * budget;
        let stage2 = !sample.static_scene
            && !sample.keyframe
            && (self.overload_frames >= 3 || bandwidth_floor || queue_pressure);

        let mut force_idr = false;
        let mut changed_resolution = false;

        if stage2 && self.tier != ResolutionTier::Low {
            self.tier = self.tier.down();
            self.stage = AdaptationStage::Resolution;
            self.overload_frames = 0;
            self.snap_since = None;
            // New dimensions mean new SPS/PPS: the first group at this
            // size must start with an IDR.
            force_idr = true;
            changed_resolution = true;
            self.last_idr = Some(now);
        } else if stage1 {
            self.stage = AdaptationStage::Bitrate;
            let target = if let Some(bps) = available {
                (self.bitrate_bps * BITRATE_DOWN_STEP).max(bps * 0.90)
            } else {
                self.bitrate_bps * BITRATE_DOWN_STEP
            };
            // Stage 1 is a descent only. A partially recovered link waits
            // for the nominal-recovery branch below rather than raising
            // the target while still under nominal.
            self.bitrate_bps = target.min(self.bitrate_bps).max(floor).min(self.max_bps);
            self.snap_since = None;
        } else if available.is_some_and(|bps| bps >= nominal) && self.bitrate_bps < nominal {
            // The link recovered; climb back toward nominal gradually.
            self.bitrate_bps = (self.bitrate_bps * BITRATE_UP_STEP).min(nominal);
            self.stage = if self.tier == ResolutionTier::Full {
                AdaptationStage::Nominal
            } else {
                AdaptationStage::Resolution
            };
        }

        // Snap-back: sustained encoder relief plus real transport
        // headroom for 750 ms. The first healthy frame starts the clock;
        // any unhealthy frame resets it.
        let headroom = available.is_some_and(|bps| bps >= nominal) && sample.queued_frames <= 2;
        if self.tier != ResolutionTier::Full && encode_s < 0.65 * budget && headroom {
            let since = *self.snap_since.get_or_insert(now);
            if now.duration_since(since) >= SNAP_BACK_HOLD {
                self.tier = self.tier.up();
                self.stage = if self.tier == ResolutionTier::Full {
                    AdaptationStage::Nominal
                } else {
                    AdaptationStage::Resolution
                };
                self.snap_since = None;
                self.overload_frames = 0;
                force_idr = true;
                changed_resolution = true;
                self.last_idr = Some(now);
            }
        } else {
            self.snap_since = None;
        }

        if force_idr {
            self.last_idr = Some(now);
        }

        let (width, height) = self.current_dimensions();
        let area_ratio = f64::from(width) * f64::from(height)
            / (f64::from(self.target_width) * f64::from(self.target_height));
        let max_now = (self.max_bps * area_ratio).max(floor);
        let bitrate = self.bitrate_bps.clamp(floor, max_now);

        self.last_observe = Some(now);
        EncoderDecision {
            width,
            height,
            fps: self.target_fps,
            bitrate_bps: bitrate.round() as u64,
            max_bitrate_bps: max_now.round() as u64,
            force_idr,
            changed_resolution,
            stage: self.stage,
        }
    }
}

/// Nominal capped-VBR target for a target size and rate, rounded to
/// 100 kbps. The result is a starting point, not a guarantee.
pub fn nominal_bitrate(width: u32, height: u32, fps: u32) -> f64 {
    let pixels = f64::from(width.max(16)) * f64::from(height.max(16));
    let raw = pixels * f64::from(fps.max(1)) * NOMINAL_BITS_PER_PIXEL;
    // 1440p60 lands at 10 Mbps nominal / ~16.7 Mbps cap; larger 4K
    // surfaces stay at that ceiling rather than growing without bound.
    ((raw.max(500_000.0) / 100_000.0).round() * 100_000.0).min(10_000_000.0)
}

/// Scale by a tier percentage and round to a multiple of 16. The result
/// never exceeds the target and never falls below one macroblock.
pub fn scaled_dimensions(width: u32, height: u32, percent: u32) -> (u32, u32) {
    let round16 = |value: u32| {
        let scaled = (u64::from(value) * u64::from(percent) + 50) / 100;
        let rounded = ((scaled + 8) / 16) * 16;
        (rounded.max(16) as u32).min(width.max(16) & !15)
    };
    (round16(width), round16(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(encode_ms: u64, available: Option<u64>, queue: usize) -> FrameSample {
        FrameSample {
            encode: Duration::from_millis(encode_ms),
            available_bps: available,
            queued_frames: queue,
            static_scene: false,
            keyframe: false,
        }
    }

    #[test]
    fn gop_is_two_seconds_and_nominal_matches_the_reference_points() {
        let c = AdaptiveScreenController::new(1920, 1080, 60);
        assert_eq!(c.gop_frames(), 120);
        assert!((5_800_000..=6_200_000).contains(&c.nominal_bitrate()));
        assert!((9_600_000..=10_000_000).contains(&c.max_bitrate()));

        let qhd = AdaptiveScreenController::new(2560, 1440, 60);
        assert!((9_500_000..=10_200_000).contains(&qhd.nominal_bitrate()));
        assert!((16_600_000..=16_700_000).contains(&qhd.max_bitrate()));
    }

    #[test]
    fn stage_one_only_lowers_bitrate_and_never_resolution() {
        let mut c = AdaptiveScreenController::new(1920, 1080, 60);
        let t0 = Instant::now();
        let first = c.observe(sample(5, Some(3_000_000), 0), t0);
        assert_eq!(first.width, 1920);
        assert_eq!(first.height, 1080);
        assert_eq!(first.fps, 60);
        assert_eq!(c.stage(), AdaptationStage::Bitrate);
        assert!(first.bitrate_bps < c.nominal_bitrate());

        let mut decision = first;
        for _ in 0..100 {
            decision = c.observe(sample(5, Some(2_500_000), 0), t0);
        }
        assert_eq!(c.tier(), ResolutionTier::Full);
        assert!(decision.bitrate_bps >= (c.nominal_bitrate() as f64 * 0.40) as u64);
    }

    #[test]
    fn stage_two_needs_three_consecutive_overloaded_frames() {
        let mut c = AdaptiveScreenController::new(1920, 1080, 60);
        let t0 = Instant::now();
        // 17ms is > 0.90 * 16.67ms.
        let d1 = c.observe(sample(17, Some(10_000_000), 0), t0);
        let d2 = c.observe(sample(17, Some(10_000_000), 0), t0);
        assert!(!d1.changed_resolution && !d2.changed_resolution);
        let d3 = c.observe(sample(17, Some(10_000_000), 0), t0);
        assert!(d3.changed_resolution && d3.force_idr);
        assert_eq!((d3.width, d3.height), (1632, 912));
        assert_eq!(d3.fps, 60);
    }

    #[test]
    fn static_screens_and_idr_bursts_never_downscale() {
        let mut c = AdaptiveScreenController::new(1920, 1080, 60);
        let t0 = Instant::now();
        for _ in 0..10 {
            let mut s = sample(40, Some(100_000), 9);
            s.static_scene = true;
            assert!(!c.observe(s, t0).changed_resolution);
        }
        for _ in 0..10 {
            let mut s = sample(40, Some(100_000), 9);
            s.keyframe = true;
            assert!(!c.observe(s, t0).changed_resolution);
        }
        assert_eq!(c.tier(), ResolutionTier::Full);
    }

    #[test]
    fn snap_back_needs_750ms_of_headroom_and_emits_an_idr() {
        let mut c = AdaptiveScreenController::new(1920, 1080, 60);
        let t0 = Instant::now();
        for _ in 0..3 {
            c.observe(sample(17, Some(10_000_000), 0), t0);
        }
        assert_eq!(c.tier(), ResolutionTier::Medium);

        let healthy = sample(5, Some(10_000_000), 0);
        let d = c.observe(healthy, t0 + Duration::from_millis(500));
        assert!(!d.changed_resolution);
        let d = c.observe(healthy, t0 + Duration::from_millis(1_249));
        assert!(!d.changed_resolution);
        let d = c.observe(healthy, t0 + Duration::from_millis(1_250));
        assert!(d.changed_resolution && d.force_idr);
        assert_eq!(c.tier(), ResolutionTier::Full);
        assert_eq!((d.width, d.height), (1920, 1080));
    }

    #[test]
    fn join_idr_is_debounced_for_one_second() {
        let mut c = AdaptiveScreenController::new(1280, 720, 30);
        let t0 = Instant::now();
        assert!(c.request_join_idr(t0));
        assert!(!c.request_join_idr(t0 + Duration::from_millis(999)));
        assert!(c.request_join_idr(t0 + JOIN_IDR_DEBOUNCE));
    }

    #[test]
    fn tiers_round_to_sixteen_pixel_multiples() {
        assert_eq!(scaled_dimensions(1920, 1080, 85), (1632, 912));
        assert_eq!(scaled_dimensions(1920, 1080, 75), (1440, 816));
        assert_eq!(scaled_dimensions(100, 100, 75), (80, 80));
    }
}
