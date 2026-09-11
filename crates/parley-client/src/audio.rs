//! cpal device I/O. Compiled only with the `audio-device` feature (same
//! gate as parley-core's) so CI never needs ALSA.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use tokio::sync::mpsc::UnboundedReceiver;

use parley_core::audio::device::{PREFERRED_RATE, preferred_input_config, preferred_output_config};

use crate::mixer::{self, VoicesMap};

pub struct Audio {
    pub pcm_rx: UnboundedReceiver<Vec<i16>>,
    _in: cpal::Stream,
    _out: cpal::Stream,
}

/// Open both device streams at 48 kHz. Refuses (rather than degrades) if
/// either device won't do 48k — a 44.1k stream feeding the 48k pipeline
/// is the pitch-shift poison mode.
pub fn start(voices: VoicesMap) -> Result<Audio, String> {
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
    let in_stream_cfg: cpal::StreamConfig = in_cfg.into();
    let out_stream_cfg: cpal::StreamConfig = out_cfg.into();

    let (pcm_tx, pcm_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i16>>();

    let in_stream = in_dev
        .build_input_stream(
            &in_stream_cfg,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                // Real-time thread: convert, hand off, never block.
                let mono = downmix(data, in_channels);
                let pcm: Vec<i16> = mono
                    .iter()
                    .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                    .collect();
                let _ = pcm_tx.send(pcm);
            },
            |e| eprintln!("capture stream error: {e}"),
            None,
        )
        .map_err(|e| format!("build capture stream: {e}"))?;

    let out_stream = out_dev
        .build_output_stream(
            &out_stream_cfg,
            {
                let voices = voices.clone();
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / out_channels;
                    let mixed = mixer::mix(&voices, frames);
                    for (frame, &s) in data.chunks_mut(out_channels).zip(mixed.iter()) {
                        for c in frame {
                            *c = s;
                        }
                    }
                }
            },
            |e| eprintln!("render stream error: {e}"),
            None,
        )
        .map_err(|e| format!("build render stream: {e}"))?;

    in_stream
        .play()
        .map_err(|e| format!("start capture: {e}"))?;
    out_stream
        .play()
        .map_err(|e| format!("start render: {e}"))?;

    println!("audio: capture \"{in_name}\" 48 kHz {in_channels}ch f32 → opus 64 kbps");
    println!("audio: render \"{out_name}\" 48 kHz {out_channels}ch (mono mix duplicated)");

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
