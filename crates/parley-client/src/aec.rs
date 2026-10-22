//! WebRTC-style audio processing on the mic path (Phase 4a): AEC + noise
//! suppression + high-pass, with the played mix as the far-end reference.
//!
//! Two backends, one API — the rest of the client never learns which one
//! it is running:
//!
//! * `aec` — the WebRTC Audio Processing Module (C++), the reference
//!   implementation. Unix only: its build script copies with `cp` and
//!   configures with meson into Unix-shaped install paths, so no Windows
//!   build has ever produced it (see docs/development.md).
//! * `aec-rust` — Sonora's pure-Rust port of the same AEC3 pipeline, which
//!   is what the Windows client ships. No C++ toolchain and no meson, and
//!   it compiles on any host — so the synthetic-echo test in
//!   `tests/aec.rs` runs everywhere, not only on Unix.
//!
//! With neither feature the module is a pass-through with the same API:
//! captured audio reaches the encoder untouched, so voice works and echo
//! is simply not cancelled.
//!
//! Both processors accept ONLY exact 10 ms frames (480 samples @ 48 kHz),
//! non-interleaved — device callbacks deliver arbitrary chunk lengths, so
//! FrameAssembler bridges. One Arc<Processor> is shared between the render
//! callback (reference) and the encoder task (mic processing): the APM is
//! `&self` with internal locks, and the Rust port is behind a mutex for
//! the same shape.

use std::collections::VecDeque;
use std::sync::Arc;

/// Exactly one backend may be compiled in — two would define the same
/// names, and a build that silently picked one would hide the mistake.
#[cfg(all(feature = "aec", feature = "aec-rust"))]
compile_error!(
    "pick one AEC backend: `aec` (WebRTC APM, Unix) or `aec-rust` (the Rust port Windows ships)"
);

/// 10 ms @ 48 kHz — the only frame length either processor accepts.
pub const FRAME_SAMPLES: usize = 480;

/// Which toolset to build: speech (AEC + NS + HPF) or the echo canceller
/// alone. The share-audio path cancels our own playback out of captured
/// system audio — music and video sound live there, so NS and a HPF would
/// chew on the content.
#[derive(Clone, Copy)]
pub enum Kind {
    /// AEC3 + noise suppression + high-pass, for the mic path.
    Voice,
    /// Echo cancellation only, for the share-audio capture path.
    EchoOnly,
}

// ---------------------------------------------------------------- backend

#[cfg(feature = "aec")]
mod backend {
    pub use webrtc_audio_processing::Processor;
    use webrtc_audio_processing::{Config, config};

    use super::Kind;

    pub type Error = webrtc_audio_processing::Error;

    /// AEC3 (adaptive delay) + noise suppression + high-pass. AGC off.
    pub fn new(kind: Kind) -> Result<Processor, Error> {
        let processor = Processor::new(48_000)?;
        let config = match kind {
            Kind::Voice => Config {
                echo_canceller: Some(config::EchoCanceller::default()),
                high_pass_filter: Some(Default::default()),
                noise_suppression: Some(Default::default()),
                ..Default::default()
            },
            Kind::EchoOnly => Config {
                echo_canceller: Some(config::EchoCanceller::default()),
                ..Default::default()
            },
        };
        processor.set_config(config);
        Ok(processor)
    }

    /// Exactly FRAME_SAMPLES of mono audio, in place.
    pub fn capture(processor: &Processor, frame: &mut [f32]) {
        let mut chans = [&mut frame[..]];
        if let Err(e) = processor.process_capture_frame(&mut chans) {
            eprintln!("aec: capture frame: {e}");
        }
    }

    /// Feed the far-end reference. Analysis only: the caller's audio is
    /// never modified.
    pub fn render(processor: &Processor, frame: &[f32]) {
        let chans = [frame];
        if let Err(e) = processor.analyze_render_frame(chans) {
            eprintln!("aec: render frame: {e}");
        }
    }
}

#[cfg(feature = "aec-rust")]
mod backend {
    use std::sync::Mutex;

    use sonora::config::{EchoCanceller, HighPassFilter, NoiseSuppression};
    use sonora::{AudioProcessing, Config, StreamConfig};

    use super::{FRAME_SAMPLES, Kind};

    pub type Error = sonora::Error;

    /// Sonora's engine is `Send + Sync` but processes through `&mut self`,
    /// while the client shares one processor between two threads and calls
    /// `&self` methods on it. The mutex restores that shape; the critical
    /// sections are one 10 ms frame of AEC3 (microseconds), and the APM
    /// backend serializes on its own internal locks anyway.
    pub struct Processor {
        ap: Mutex<AudioProcessing>,
    }

    pub fn new(kind: Kind) -> Result<Processor, Error> {
        let config = match kind {
            Kind::Voice => Config {
                echo_canceller: Some(EchoCanceller::default()),
                high_pass_filter: Some(HighPassFilter::default()),
                noise_suppression: Some(NoiseSuppression::default()),
                ..Default::default()
            },
            Kind::EchoOnly => Config {
                echo_canceller: Some(EchoCanceller::default()),
                ..Default::default()
            },
        };
        let ap = AudioProcessing::builder()
            .config(config)
            .capture_config(StreamConfig::new(48_000, 1))
            .render_config(StreamConfig::new(48_000, 1))
            .build();
        Ok(Processor { ap: Mutex::new(ap) })
    }

    /// A poisoned lock must not take the microphone down with it: the audio
    /// keeps flowing unfiltered rather than panicking in a callback.
    fn lock(processor: &Processor) -> std::sync::MutexGuard<'_, AudioProcessing> {
        processor.ap.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Exactly FRAME_SAMPLES of mono audio, in place.
    pub fn capture(processor: &Processor, frame: &mut [f32]) {
        // The engine wants separate input and output slices; processing in
        // place would alias them.
        let mut out = [0.0f32; FRAME_SAMPLES];
        {
            let mut ap = lock(processor);
            let src = [&frame[..]];
            let mut dst = [&mut out[..]];
            if let Err(e) = ap.process_capture_f32(&src, &mut dst) {
                eprintln!("aec: capture frame: {e}");
            }
        }
        frame.copy_from_slice(&out);
    }

    /// Feed the far-end reference. Analysis only: the caller's audio is
    /// never modified, and the processed copy is discarded.
    pub fn render(processor: &Processor, frame: &[f32]) {
        let mut out = [0.0f32; FRAME_SAMPLES];
        let mut ap = lock(processor);
        let src = [frame];
        let mut dst = [&mut out[..]];
        if let Err(e) = ap.process_render_f32(&src, &mut dst) {
            eprintln!("aec: render frame: {e}");
        }
    }
}

/// No processor at all: holds no state, because there is nothing to
/// filter with. Call sites keep their `Option<Arc<Processor>>` shape.
#[cfg(not(any(feature = "aec", feature = "aec-rust")))]
mod backend {
    use super::Kind;

    pub struct Processor(());

    pub type Error = std::convert::Infallible;

    /// Cannot fail — there is no APM to fail.
    pub fn new(_kind: Kind) -> Result<Processor, Error> {
        Ok(Processor(()))
    }

    // No `capture`/`render` here: the facade below handles this build by
    // passing audio straight through, so there is nothing to call.
}

pub use backend::{Error, Processor};

// ----------------------------------------------------------------- facade

/// Speech profile: AEC3 (adaptive delay) + noise suppression + high-pass.
pub fn new_processor() -> Result<Arc<Processor>, Error> {
    Ok(Arc::new(backend::new(Kind::Voice)?))
}

/// Echo cancellation only — the share-audio path's smaller toolset.
pub fn new_echo_canceller() -> Result<Arc<Processor>, Error> {
    Ok(Arc::new(backend::new(Kind::EchoOnly)?))
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
#[cfg(any(feature = "aec", feature = "aec-rust"))]
pub fn process_capture(
    ap: &Processor,
    asm: &mut FrameAssembler,
    input: &[f32],
    out: &mut Vec<f32>,
) {
    asm.push(input);
    while let Some(mut frame) = asm.pop_frame() {
        backend::capture(ap, &mut frame);
        out.extend_from_slice(&frame);
    }
}

/// Stub: no processor, so the mic audio passes through unchanged (and
/// unbuffered — there is no frame boundary to respect).
#[cfg(not(any(feature = "aec", feature = "aec-rust")))]
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
#[cfg(any(feature = "aec", feature = "aec-rust"))]
pub fn analyze_render(ap: &Processor, carry: &mut Vec<f32>, mixed: &[f32]) {
    carry.extend_from_slice(mixed);
    while carry.len() >= FRAME_SAMPLES {
        let frame: Vec<f32> = carry.drain(..FRAME_SAMPLES).collect();
        backend::render(ap, &frame);
    }
}

/// Stub: nothing to reference, so the played mix is not buffered at all.
#[cfg(not(any(feature = "aec", feature = "aec-rust")))]
pub fn analyze_render(_ap: &Processor, _carry: &mut Vec<f32>, _mixed: &[f32]) {}
