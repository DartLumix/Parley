//! cpal device I/O. Compiled only with the `audio-device` feature (same
//! gate as parley-core's) so CI never needs ALSA.
//!
//! Latency hooks (Phase 3f): `start()` takes optional handles —
//! `inject` logs click injection times (--measure), `report` sends click
//! detections back over the control stream (--report).
//!
//! Buffer sizing: we request an explicit 960-frame (20 ms) period, the
//! default negotiated 5120 frames (107 ms) being the dominant latency
//! term we measured. Devices that refuse fixed sizes fall back to the
//! default buffer — loudly, since that's a known-latency condition.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use parley_core::audio::device::{PREFERRED_RATE, preferred_input_config, preferred_output_config};
use parley_proto::{ClientBody, ClientMessage, SendChat};

use crate::mixer::{self, VoicesMap};

/// Click spec for --measure: 1.4 kHz near-full-scale burst, 5 ms — rare
/// in speech, survives Opus, loudest thing in a quiet room.
const CLICK_FREQ: f32 = 1_400.0;
const CLICK_LEN: usize = 240; // 5 ms @ 48 kHz
const CLICK_AMP: f32 = 0.95;
/// --report: peak threshold above which a buffer counts as "the click".
const CLICK_DETECT_THRESHOLD: f32 = 0.8;
/// One click per 2 s of captured audio.
const CLICK_INTERVAL: u64 = 96_000;
/// Detection cooldown: one report per click, not one per buffer.
const REPORT_COOLDOWN_US: u64 = 400_000;
/// Requested device period: 1024 frames ≈ 21.3 ms — one PipeWire graph
/// quantum. The pipewire-alsa shim rejects non-quantum-aligned sizes
/// (960 failed; 5120 = 5×1024 was the default it negotiated).
const PERIOD_FRAMES: u32 = 1024;

pub struct Audio {
    pub pcm_rx: UnboundedReceiver<Vec<i16>>,
    _in: cpal::Stream,
    _out: cpal::Stream,
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

// ---- capture callback: downmix, inject clicks, convert, hand off ----

struct InputState {
    pcm_tx: UnboundedSender<Vec<i16>>,
    inject: Option<Arc<Mutex<Vec<u64>>>>,
    channels: usize,
    first: bool,
    inject_total: u64,
    inject_next: u64,
}

fn input_cb(state: &mut InputState, data: &[f32]) {
    // Real-time thread: convert, hand off, never block.
    let mut mono = downmix(data, state.channels);
    if state.first {
        state.first = false;
        eprintln!(
            "capture period: {} frames ({:.1} ms)",
            mono.len(),
            mono.len() as f64 / 48.0
        );
    }
    if let Some(injections) = &state.inject {
        state.inject_total += mono.len() as u64;
        if state.inject_total > state.inject_next {
            // Click replaces mic audio for CLICK_LEN samples.
            let before = state.inject_total - mono.len() as u64;
            let offset = (state.inject_next - before) as usize;
            for (i, s) in mono.iter_mut().enumerate().skip(offset).take(CLICK_LEN) {
                let t = (i - offset) as f32 / 48_000.0;
                *s = CLICK_AMP * (2.0 * std::f32::consts::PI * CLICK_FREQ * t).sin();
            }
            injections.lock().unwrap().push(now_micros());
            eprintln!("click injected");
            state.inject_next += CLICK_INTERVAL;
        }
    }
    let pcm: Vec<i16> = mono
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let _ = state.pcm_tx.send(pcm);
}

// ---- render callback: mix, detect clicks, duplicate mono ----

struct OutputState {
    voices: VoicesMap,
    report: Option<UnboundedSender<ClientMessage>>,
    channels: usize,
    first: bool,
    last_report_us: u64,
}

fn output_cb(state: &mut OutputState, data: &mut [f32], info: &cpal::OutputCallbackInfo) {
    let frames = data.len() / state.channels;
    if state.first {
        state.first = false;
        eprintln!(
            "render period: {} frames ({:.1} ms)",
            frames,
            frames as f64 / 48.0
        );
    }
    let mixed = mixer::mix(&state.voices, frames);
    // --report: loudest sample above threshold = the click.
    let now = now_micros();
    if let Some(report_tx) = &state.report
        && let Some((idx, &peak)) = mixed
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        && peak.abs() > CLICK_DETECT_THRESHOLD
        && now.saturating_sub(state.last_report_us) > REPORT_COOLDOWN_US
    {
        state.last_report_us = now;
        // Device lead: how long before playback this callback fires
        // (cpal hands us both instants). The click plays `idx` frames
        // into the buffer, AFTER buffer[0] — add, don't subtract.
        // Device lead: how long before playback this callback fires.
        // `callback.duration_since(playback)` is None when the API
        // reports playback at/before callback time — treat as zero.
        let ts = info.timestamp();
        let lead_us = ts
            .callback
            .duration_since(&ts.playback)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let detected = now as i64 + lead_us + (idx as f64 * 1_000_000.0 / 48_000.0) as i64;
        // Channel 1 = general: chat is a global broadcast and the
        // measuring client parses the text, not the channel.
        let _ = report_tx.send(ClientMessage {
            body: Some(ClientBody::SendChat(SendChat {
                channel_id: 1,
                text: format!("PARLEY_LATENCY {detected}"),
            })),
        });
        eprintln!("click detected (peak {peak:.2}), reported");
    }
    for (frame, &s) in data.chunks_mut(state.channels).zip(mixed.iter()) {
        for c in frame {
            *c = s;
        }
    }
}

/// Open both device streams at 48 kHz. Refuses (rather than degrades) if
/// either device won't do 48k — a 44.1k stream feeding the 48k pipeline
/// is the pitch-shift poison mode. (Buffer-size fallback IS acceptable
/// and announced.)
pub fn start(
    voices: VoicesMap,
    inject: Option<Arc<Mutex<Vec<u64>>>>,
    report: Option<UnboundedSender<ClientMessage>>,
) -> Result<Audio, String> {
    let host = cpal::default_host();
    let in_dev = host
        .default_input_device()
        .ok_or("no default input device")?;
    let out_dev = host
        .default_output_device()
        .ok_or("no default output device")?;
    let in_name = in_dev.name().unwrap_or_else(|_| "?".into());
    let out_name = out_dev.name().unwrap_or_else(|_| "?".into());

    let in_cfg = preferred_input_config(&in_dev).map_err(|e| format!("input config: {e}"))?;
    let out_cfg = preferred_output_config(&out_dev).map_err(|e| format!("output config: {e}"))?;
    for (what, name, rate) in [
        ("capture", in_name.as_str(), in_cfg.sample_rate().0),
        ("render", out_name.as_str(), out_cfg.sample_rate().0),
    ] {
        if rate != PREFERRED_RATE {
            return Err(format!(
                "{what} device \"{name}\" opened at {rate} Hz, need {PREFERRED_RATE} Hz — refusing to run"
            ));
        }
    }

    let in_channels = in_cfg.channels() as usize;
    let out_channels = out_cfg.channels() as usize;

    let (pcm_tx, pcm_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i16>>();

    // Two configs per direction: explicit 20 ms period first, device
    // default as fallback. Each build attempt gets a fresh callback
    // state (the move closures consume their captures).
    let fixed_in = cpal::StreamConfig {
        channels: in_cfg.channels(),
        sample_rate: in_cfg.sample_rate(),
        buffer_size: cpal::BufferSize::Fixed(PERIOD_FRAMES),
    };
    let default_in: cpal::StreamConfig = in_cfg.into();
    let build_input = |cfg: &cpal::StreamConfig| {
        let mut state = InputState {
            pcm_tx: pcm_tx.clone(),
            inject: inject.clone(),
            channels: in_channels,
            first: true,
            inject_total: 0,
            inject_next: CLICK_INTERVAL,
        };
        in_dev.build_input_stream(
            cfg,
            move |d, _| input_cb(&mut state, d),
            |e| eprintln!("capture stream error: {e}"),
            None,
        )
    };
    let in_stream = match build_input(&fixed_in) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "parley: capture device rejected a {PERIOD_FRAMES}-frame buffer; using default (higher latency)"
            );
            build_input(&default_in).map_err(|e| format!("build capture stream: {e}"))?
        }
    };

    let fixed_out = cpal::StreamConfig {
        channels: out_cfg.channels(),
        sample_rate: out_cfg.sample_rate(),
        buffer_size: cpal::BufferSize::Fixed(PERIOD_FRAMES),
    };
    let default_out: cpal::StreamConfig = out_cfg.into();
    let build_output = |cfg: &cpal::StreamConfig| {
        let mut state = OutputState {
            voices: voices.clone(),
            report: report.clone(),
            channels: out_channels,
            first: true,
            last_report_us: 0,
        };
        out_dev.build_output_stream(
            cfg,
            move |d, i| output_cb(&mut state, d, i),
            |e| eprintln!("render stream error: {e}"),
            None,
        )
    };
    let out_stream = match build_output(&fixed_out) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "parley: render device rejected a {PERIOD_FRAMES}-frame buffer; using default (higher latency)"
            );
            build_output(&default_out).map_err(|e| format!("build render stream: {e}"))?
        }
    };

    in_stream
        .play()
        .map_err(|e| format!("start capture: {e}"))?;
    out_stream
        .play()
        .map_err(|e| format!("start render: {e}"))?;

    let measuring = inject.is_some();
    let reporting = report.is_some();
    println!("audio: capture \"{in_name}\" 48 kHz {in_channels}ch f32 → opus 64 kbps");
    println!("audio: render \"{out_name}\" 48 kHz {out_channels}ch (mono mix duplicated)");
    if measuring {
        println!("measure: injecting a click every 2 s");
    }
    if reporting {
        println!("report: click detection armed");
    }

    Ok(Audio {
        pcm_rx,
        _in: in_stream,
        _out: out_stream,
    })
}

fn downmix(data: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        1 => data.to_vec(),
        n => data
            .chunks(n)
            .map(|c| c.iter().sum::<f32>() / n as f32)
            .collect(),
    }
}
