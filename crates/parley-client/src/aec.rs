//! WebRTC AudioProcessing wiring (Phase 4a): AEC + noise suppression +
//! high-pass on the mic path; the played mix as the far-end reference.
//!
//! The Processor accepts ONLY exact 10 ms frames (480 samples @ 48 kHz),
//! non-interleaved — device callbacks deliver arbitrary chunk lengths,
//! so FrameAssembler bridges. The Processor is Send + Sync with &self
//! methods: one Arc<Processor> is shared between the render callback
//! (analyze) and the encoder task (process).
//!
//! The WebRTC APM is behind the `aec` feature, NOT `audio-device`: its
//! build script shells out to `cp`/meson and installs into Unix-shaped
//! paths, so it has never been built on Windows (see
//! [development.md](../../../docs/development.md)). Without the feature
//! this module is a pass-through with the same API — captured audio
//! reaches the encoder untouched, so voice works and echo is simply not
//! cancelled.

use std::collections::VecDeque;
use std::sync::Arc;

#[cfg(feature = "aec")]
pub use webrtc_audio_processing::Processor;
#[cfg(feature = "aec")]
use webrtc_audio_processing::{Config, config};

/// 10 ms @ 48 kHz — the only frame length the processor accepts.
pub const FRAME_SAMPLES: usize = 480;

/// The failure a processor can hit at construction: the APM's own error
/// with the feature on, and the never-type when there is no APM to fail.
#[cfg(feature = "aec")]
pub type Error = webrtc_audio_processing::Error;
#[cfg(not(feature = "aec"))]
pub type Error = std::convert::Infallible;

/// The stand-in for the APM in a build without the `aec` feature: it
/// holds no state, because there is nothing to filter with. Call sites
/// keep their `Option<Arc<Processor>>` shape unchanged.
#[cfg(not(feature = "aec"))]
pub struct Processor(());

/// AEC3 (adaptive delay) + noise suppression + high-pass. AGC off for v1.
#[cfg(feature = "aec")]
pub fn new_processor() -> Result<Arc<Processor>, Error> {
    let processor = Processor::new(48_000)?;
    processor.set_config(Config {
        echo_canceller: Some(config::EchoCanceller::default()),
        high_pass_filter: Some(Default::default()),
        noise_suppression: Some(Default::default()),
        ..Default::default()
    });
    Ok(Arc::new(processor))
}

/// Stub for a build without the APM: cannot fail, filters nothing.
#[cfg(not(feature = "aec"))]
pub fn new_processor() -> Result<Arc<Processor>, Error> {
    Ok(Arc::new(Processor(())))
}

/// Echo-cancellation ONLY (no noise suppression, no high-pass): the
/// share-audio path cancels our own playback out of the captured system
/// mix, and the content there is music and video sound — NS and a HPF
/// would chew on it. Same processor type, smaller toolset.
#[cfg(feature = "aec")]
pub fn new_echo_canceller() -> Result<Arc<Processor>, Error> {
    let processor = Processor::new(48_000)?;
    processor.set_config(Config {
        echo_canceller: Some(config::EchoCanceller::default()),
        ..Default::default()
    });
    Ok(Arc::new(processor))
}

/// Stub for a build without the APM: cannot fail, filters nothing.
#[cfg(not(feature = "aec"))]
pub fn new_echo_canceller() -> Result<Arc<Processor>, Error> {
    Ok(Arc::new(Processor(())))
}

/// Buffers arbitrary-length chunks; pops exact 10 ms frames.
pub struct FrameAssembler {
    buf: VecDeque<f32>,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            buf: VecDeque::new(),
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.buf.extend(samples.iter().copied());
    }

    /// Next complete 10 ms frame, if available.
    pub fn pop_frame(&mut self) -> Option<Vec<f32>> {
        if self.buf.len() < FRAME_SAMPLES {
            return None;
        }
        Some(self.buf.drain(..FRAME_SAMPLES).collect())
    }
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Mic path: process `input` through AEC/NS/HPF, appending to `out`.
/// Adds no latency beyond completing the current 10 ms frame.
#[cfg(feature = "aec")]
pub fn process_capture(
    ap: &Processor,
    asm: &mut FrameAssembler,
    input: &[f32],
    out: &mut Vec<f32>,
) {
    asm.push(input);
    while let Some(mut frame) = asm.pop_frame() {
        let mut chans = [&mut frame[..]];
        if let Err(e) = ap.process_capture_frame(&mut chans) {
            eprintln!("aec: capture frame: {e}");
        }
        out.extend_from_slice(&frame);
    }
}

/// Stub: no APM, so the mic audio passes through unchanged.
#[cfg(not(feature = "aec"))]
pub fn process_capture(
    _ap: &Processor,
    _asm: &mut FrameAssembler,
    input: &[f32],
    out: &mut Vec<f32>,
) {
    out.extend_from_slice(input);
}

/// Render path: feed the played mix to the AEC as the far-end reference.
/// Analysis only — the played audio itself is never modified.
#[cfg(feature = "aec")]
pub fn analyze_render(ap: &Processor, carry: &mut Vec<f32>, mixed: &[f32]) {
    carry.extend_from_slice(mixed);
    while carry.len() >= FRAME_SAMPLES {
        let frame: Vec<f32> = carry.drain(..FRAME_SAMPLES).collect();
        let chans = [frame.as_slice()];
        if let Err(e) = ap.analyze_render_frame(chans) {
            eprintln!("aec: render frame: {e}");
        }
    }
}

/// Stub: nothing to reference, so the played mix is not buffered at all.
#[cfg(not(feature = "aec"))]
pub fn analyze_render(_ap: &Processor, _carry: &mut Vec<f32>, _mixed: &[f32]) {}
