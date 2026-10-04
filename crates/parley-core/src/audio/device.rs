#![cfg(feature = "audio-device")]

//! Platform audio device I/O (cpal: PipeWire/ALSA on Linux, WASAPI,
//! CoreAudio, etc.). We don't build streams here — we only discover
//! devices and sane configs. Stream construction with our exact
//! callback model lives in the client pipeline (Phase 3e).

use cpal::traits::{DeviceTrait, HostTrait};

use crate::error::Error;

/// Human-readable list of (input devices, output devices) on the
/// default host.
pub fn list_devices() -> Result<(Vec<String>, Vec<String>), Error> {
    let host = cpal::default_host();

    let inputs = host
        .input_devices()
        .map_err(|e| Error::AudioDevice(e.to_string()))?
        .filter_map(|d| d.name().ok())
        .collect();

    let outputs = host
        .output_devices()
        .map_err(|e| Error::AudioDevice(e.to_string()))?
        .filter_map(|d| d.name().ok())
        .collect();

    Ok((inputs, outputs))
}

/// The default input device and a supported config, resampled
/// conceptually to 48 kHz mono by the pipeline (cpal gives us whatever
/// the device wants; conversion happens in the callback later).
pub fn default_input_config() -> Result<(String, cpal::SupportedStreamConfig), Error> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| Error::AudioDevice("no default input device".into()))?;
    let name = device
        .name()
        .map_err(|e| Error::AudioDevice(e.to_string()))?;
    let config = device
        .default_input_config()
        .map_err(|e| Error::AudioDevice(e.to_string()))?;
    Ok((name, config))
}

/// The default output device and a supported config.
pub fn default_output_config() -> Result<(String, cpal::SupportedStreamConfig), Error> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| Error::AudioDevice("no default output device".into()))?;
    let name = device
        .name()
        .map_err(|e| Error::AudioDevice(e.to_string()))?;
    let config = device
        .default_output_config()
        .map_err(|e| Error::AudioDevice(e.to_string()))?;
    Ok((name, config))
}

/// All device streams open here: Opus is 48 kHz and this machine's PipeWire
/// graph is 48k-only, so nothing resamples anywhere.
pub const PREFERRED_RATE: u32 = 48_000;

fn preferred_from(
    default: cpal::SupportedStreamConfig,
    ranges: &[cpal::SupportedStreamConfigRange],
    direction: &str,
) -> cpal::SupportedStreamConfig {
    let capable = ranges.iter().any(|r| {
        r.channels() == default.channels()
            && r.min_sample_rate().0 <= PREFERRED_RATE
            && r.max_sample_rate().0 >= PREFERRED_RATE
    });
    if capable {
        // Only the rate changes; format/channels/buffer stay the default's.
        // Never inherit a range's sample_format — enumeration order can
        // surface odd ones (I8 here).
        return cpal::SupportedStreamConfig::new(
            default.channels(),
            cpal::SampleRate(PREFERRED_RATE),
            *default.buffer_size(),
            default.sample_format(),
        );
    }
    eprintln!(
        "parley: no {} Hz {} config (default {} Hz, {} ch); reported ranges (ch, min Hz, max Hz, fmt): {:?}",
        PREFERRED_RATE,
        direction,
        default.sample_rate().0,
        default.channels(),
        ranges
            .iter()
            .map(|r| (
                r.channels(),
                r.min_sample_rate().0,
                r.max_sample_rate().0,
                r.sample_format()
            ))
            .collect::<Vec<_>>(),
    );
    default
}

/// Default output config with the rate bumped to PREFERRED_RATE when a
/// reported range covers it; untouched default otherwise.
pub fn preferred_output_config(
    device: &cpal::Device,
) -> Result<cpal::SupportedStreamConfig, cpal::DefaultStreamConfigError> {
    let default = device.default_output_config()?;
    let ranges = match device.supported_output_configs() {
        Ok(ranges) => ranges.collect(),
        Err(e) => {
            eprintln!("parley: output config enumeration failed: {e}");
            Vec::new()
        }
    };
    Ok(preferred_from(default, &ranges, "output"))
}

/// Same for capture.
pub fn preferred_input_config(
    device: &cpal::Device,
) -> Result<cpal::SupportedStreamConfig, cpal::DefaultStreamConfigError> {
    let default = device.default_input_config()?;
    let ranges = match device.supported_input_configs() {
        Ok(ranges) => ranges.collect(),
        Err(e) => {
            eprintln!("parley: input config enumeration failed: {e}");
            Vec::new()
        }
    };
    Ok(preferred_from(default, &ranges, "input"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests touch real hardware — only meaningful on a desktop
    /// session, and they break headless CI. Gated behind PARLEY_AUDIO=1.
    fn hardware_enabled() -> bool {
        std::env::var("PARLEY_AUDIO")
            .map(|v| v == "1")
            .unwrap_or(false)
    }

    #[test]
    fn lists_devices_when_hardware_enabled() {
        if !hardware_enabled() {
            eprintln!("skipping (set PARLEY_AUDIO=1 to enable)");
            return;
        }
        let (inputs, outputs) = list_devices().unwrap();
        eprintln!("inputs:  {inputs:?}");
        eprintln!("outputs: {outputs:?}");
        assert!(
            !outputs.is_empty(),
            "a machine running this test has audio output"
        );
    }

    #[test]
    fn default_configs_when_hardware_enabled() {
        if !hardware_enabled() {
            eprintln!("skipping (set PARLEY_AUDIO=1 to enable)");
            return;
        }
        if let Ok((name, config)) = default_input_config() {
            eprintln!("input:  {name} {:?}", config);
        } else {
            eprintln!("no input device (headless server?) — fine");
        }
        let (name, config) = default_output_config().unwrap();
        eprintln!("output: {name} {:?}", config);
    }
}
