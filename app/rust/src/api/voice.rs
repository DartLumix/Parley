//! Voice: send + receive (7e). Media datagram reader, mixer, output
//! AND input device, AEC, SendPipeline — the whole per-session voice
//! path. Receive-only was 7d; this file now owns both directions.
//!
//! OWNERSHIP — one rule, load-bearing: a VoiceControl belongs to ONE
//! session and lives in LiveSession.voice. Dropping it (leave event,
//! disconnect, registry teardown) flips its stop flag, signals the
//! media reader, and aborts the encoder task; the device thread
//! observes the flag within ~100 ms and its scope exit drops both
//! cpal streams. Voice can no longer outlive the session that owns
//! it — the structural closure of the 7d voice-state-across-reconnects
//! gap (a new session gets a fresh LiveSession with voice: None; the
//! old session's devices die on every death path).
//!
//! Membership semantics (7e, per the approved plan): joining a voice
//! channel IS listening + transmitting (TeamSpeak shape — the SFU
//! contract has no listen toggle). session.rs calls start_voice on
//! the self-join event and drops the control on the self-leave event;
//! there is no separate start/stop button and no device lifetime
//! beyond membership.
//!
//! AUDIO PATHS:
//! - Output:  media datagrams → mixer → cpal output callback →
//!   device. The SAME callback feeds the AEC its far-end reference
//!   (analyze_render — analysis only, the played audio is never
//!   modified).
//! - Input:   cpal capture callback → bounded(16) channel + try_send
//!   (the 6f discipline — a slow consumer drops stale mic chunks,
//!   never blocks the RT thread) → encoder task on the shared runtime
//!   → AEC (speech profile only) → SendPipeline (speech mono 64 kbps /
//!   music stereo 256 kbps) → media datagrams.
//!
//! Profiles: speech = SendPipeline::new(64_000) + AEC on; music =
//! new_music(256_000) with the mono mic upmixed to centered stereo
//! (the terminal client's exact music-channel behavior) + AEC OFF
//! (music channels favor transparency; AEC is a speech tool).
//!
//! BRIDGE RULE: nothing but plain types crosses to Dart. This
//! module's items are pub(crate) — codegen ignores them (the 7d
//! lesson: a pub signature with non-plain types made codegen emit
//! unresolvable opaque wrappers).
//!
//! FEATURE GATE: cpal + webrtc-audio-processing compile ONLY with
//! `audio-device` — default-off keeps workspace-wide CI clippy and
//! the server's musl Docker build clean (§11).
//!
//! THREADING (the 7d rules, unchanged): cpal::Stream is !Send/!Sync —
//! both streams live on the device thread's stack, never in a static.
//! quinn read_datagram() is await-only and FRB worker threads have no
//! tokio context, so async tasks spawn on the shared rt(). The AEC
//! Processor is Send+Sync with &self methods: one Arc shared between
//! the output callback (analyze) and the encoder task (process).
//! start_voice BLOCKS on device readiness — its caller runs it on a
//! blocking-pool thread (session.rs), never under the registry lock.

#[cfg(feature = "audio-device")]
use parley_core::audio::mixer::SharedMixer;

#[cfg(feature = "audio-device")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
#[cfg(feature = "audio-device")]
use parley_core::audio::device::{preferred_input_config, preferred_output_config, PREFERRED_RATE};
#[cfg(feature = "audio-device")]
use parley_proto::media::FLAG_STEREO;
#[cfg(feature = "audio-device")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "audio-device")]
use std::sync::Arc;
#[cfg(feature = "audio-device")]
use std::time::Duration;

/// Device period: 1024 frames ≈ 21.3 ms — one PipeWire graph quantum
/// (same value and rationale as the terminal client's audio.rs).
#[cfg(feature = "audio-device")]
const PERIOD_FRAMES: u32 = 1024;

/// Live voice state for one session. Dropping it stops everything —
/// that IS the shutdown path (see the module docs). All fields are
/// Send+Sync handles; the cpal streams themselves never leave the
/// device thread.
#[cfg(feature = "audio-device")]
pub(crate) struct VoiceControl {
    /// Flipped by Drop (and by reader death); polled by the device
    /// thread (~100 ms slices — the §8 pw-loop discipline; idle cost
    /// nil).
    stop: Arc<AtomicBool>,
    /// Media reader stop signal (tokio mpsc: selectable). Drop sends
    /// on it.
    reader_stop: tokio::sync::mpsc::UnboundedSender<()>,
    /// The encoder task's abort handle: Drop aborts it deterministically
    /// (it would also exit on its own when the capture channel closes).
    encoder_abort: tokio::task::AbortHandle,
    /// Both cpal streams live on this thread's stack; the thread parks
    /// until the stop flag, then scope exit drops the streams. NOT
    /// joined on Drop: join can block, and Drop runs under the
    /// registry lock — the detached thread self-terminates within
    /// ~100 ms (same discipline as the 7d harvest path).
    _device_thread: std::thread::JoinHandle<()>,
}

#[cfg(feature = "audio-device")]
impl VoiceControl {
    /// True when the media reader saw connection death — the devices
    /// are on their way out (the flag they poll is already set). The
    /// caller replaces a stopped control instead of short-circuiting
    /// on it.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

#[cfg(feature = "audio-device")]
impl Drop for VoiceControl {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.reader_stop.send(());
        self.encoder_abort.abort();
    }
}

/// Build the whole voice path for one session: media reader (feed:
/// mixer), output + input device streams, AEC, encoder task. Blocks
/// until the devices report readiness (device build is ~100 ms
/// typical) — the caller MUST run this off the registry lock (see the
/// module docs). Returns the control whose Drop tears everything down.
///
/// A partial failure tears its own parts down: the encoder aborts and
/// reader_stop drops when this fn returns Err, and the device thread
/// (if it started) holds no stream past its scope exit.
#[cfg(feature = "audio-device")]
pub(crate) fn start_voice(
    mixer: SharedMixer,
    conn: quinn::Connection,
    my_id: u32,
    music: bool,
) -> Result<VoiceControl, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (reader_stop_tx, mut reader_stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // --- media datagram reader (async: quinn's read_datagram is
    // await-only). Runs on the session module's runtime. Feed: mixer
    // (audio) — video datagrams are skipped (7f's seam). Reader death
    // (connection gone) flips the SAME stop flag the devices poll —
    // with membership semantics the UI tears down on connection-lost
    // and would never signal the devices itself (the 7d death-signal
    // rule, now with an owner to also Drop it).
    {
        let mixer = mixer.clone();
        let reader_stop_flag = stop.clone();
        let conn = conn.clone();
        crate::api::session::rt().spawn(async move {
            loop {
                tokio::select! {
                    _ = reader_stop_rx.recv() => break,
                    dg = conn.read_datagram() => match dg {
                        Ok(dg) => {
                            let Ok(m) = parley_proto::media::decode(&dg) else {
                                continue;
                            };
                            // Self-echo guard (server never loops audio
                            // back, but be defensive — the terminal
                            // client's filter).
                            if m.sender_id == 0 || m.sender_id == my_id {
                                continue;
                            }
                            // Video rides the same datagrams — the hub
                            // is 7f's seam; skip for now.
                            if m.flags & parley_proto::media::FLAG_VIDEO != 0 {
                                continue;
                            }
                            let stereo = m.flags & FLAG_STEREO != 0;
                            if let Err(e) = mixer
                                .lock()
                                .unwrap()
                                .push_packet(m.sender_id, m.seq, m.payload, stereo)
                            {
                                eprintln!(
                                    "voice: recv pipeline for user {}: {e}",
                                    m.sender_id
                                );
                            }
                        }
                        Err(_) => {
                            reader_stop_flag.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
        });
    }

    // --- AEC (speech profile only): one Processor shared between the
    // output callback (far-end reference) and the encoder task (mic
    // processing). None for music — same rationale as the terminal
    // client's main.rs. Init failure degrades to uncancellable mic
    // rather than failing the join (the terminal client's behavior).
    let aec: Option<Arc<webrtc_audio_processing::Processor>> = if music {
        println!("voice: aec off (music channel)");
        None
    } else {
        match parley_client::aec::new_processor() {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("voice: aec init failed ({e}); continuing without echo cancellation");
                None
            }
        }
    };

    // --- capture channel (bounded 16, the 6f discipline): RT
    // callbacks try_send; a stalled encoder drops stale mic chunks.
    let (pcm_tx, mut pcm_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(16);

    // --- encoder task: AEC → i16 → SendPipeline → datagrams. Exits
    // when the capture sender drops (teardown) or the connection dies.
    let encoder_task = {
        let conn = conn.clone();
        let aec = aec.clone();
        crate::api::session::rt().spawn(async move {
            let mut send = if music {
                match parley_core::audio::pipeline::SendPipeline::new_music(256_000) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("voice: music encoder init failed: {e}");
                        return;
                    }
                }
            } else {
                match parley_core::audio::pipeline::SendPipeline::new(64_000) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("voice: encoder init failed: {e}");
                        return;
                    }
                }
            };
            let mut asm = parley_client::aec::FrameAssembler::new();
            loop {
                let Some(chunk) = pcm_rx.recv().await else {
                    break; // capture sender dropped → teardown
                };
                let processed: Vec<f32> = match aec.as_ref() {
                    Some(ap) => {
                        let mut out = Vec::new();
                        parley_client::aec::process_capture(ap, &mut asm, &chunk, &mut out);
                        out
                    }
                    None => chunk,
                };
                if processed.is_empty() {
                    continue;
                }
                let pcm: Vec<i16> = if music {
                    // Mono mic → centered stereo (the terminal
                    // client's music-channel behavior).
                    processed
                        .iter()
                        .flat_map(|&s| {
                            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                            [v, v]
                        })
                        .collect()
                } else {
                    processed
                        .iter()
                        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                        .collect()
                };
                let mut packets = Vec::new();
                if send.push_pcm(&pcm, &mut packets).is_err() {
                    eprintln!("voice: encode failed");
                    break;
                }
                let flags = if music {
                    parley_proto::media::FLAG_SPEAKING | FLAG_STEREO
                } else {
                    parley_proto::media::FLAG_SPEAKING
                };
                for (seq, ts, pkt) in packets {
                    let dg = parley_proto::media::encode(flags, seq, ts, 0, &pkt);
                    if conn.send_datagram(dg.into()).is_err() {
                        return; // connection gone
                    }
                }
            }
        })
    };

    // --- device thread: OWNS both cpal streams (they must never leave
    // this thread). Builds, reports readiness, parks until stop, then
    // drops them — cpal stops streams on drop.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let stop_for_thread = stop.clone();
    let device_thread = std::thread::spawn(move || {
        match build_streams(&mixer, aec, pcm_tx) {
            Ok(streams) => {
                let AudioStreams { input, output } = streams;
                let played = input.play().and_then(|_| output.play());
                match played {
                    Ok(()) => {
                        let _ = ready_tx.send(Ok(()));
                        while !stop_for_thread.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        // scope exit drops both streams: devices closed
                    }
                    Err(e) => {
                        // Dropping both streams at scope exit; report.
                        let _ = ready_tx.send(Err(format!("start streams: {e}")));
                    }
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    });
    // Build outcome surfaces to the caller; a failure also drops
    // reader_stop_tx and aborts the encoder (this fn returns Err →
    // VoiceControl never constructed). No orphans.
    if let Err(e) = ready_rx
        .recv()
        .map_err(|e| format!("device thread died: {e}"))?
    {
        encoder_task.abort();
        return Err(e);
    }

    Ok(VoiceControl {
        stop,
        reader_stop: reader_stop_tx,
        encoder_abort: encoder_task.abort_handle(),
        _device_thread: device_thread,
    })
}

#[cfg(feature = "audio-device")]
struct AudioStreams {
    input: cpal::Stream,
    output: cpal::Stream,
}

/// Open both device streams at 48 kHz (refuse-not-degrade on rate),
/// quantum-aligned period with device-default fallback — the terminal
/// client's two-attempt discipline, per direction.
#[cfg(feature = "audio-device")]
fn build_streams(
    mixer: &SharedMixer,
    aec: Option<Arc<webrtc_audio_processing::Processor>>,
    pcm_tx: tokio::sync::mpsc::Sender<Vec<f32>>,
) -> Result<AudioStreams, String> {
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

    // --- input stream: downmix → hand off (never block the RT
    // thread). try_send + count: drop-on-full, log every 128th drop.
    let drops = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let fixed_in = cpal::StreamConfig {
        channels: in_cfg.channels(),
        sample_rate: in_cfg.sample_rate(),
        buffer_size: cpal::BufferSize::Fixed(PERIOD_FRAMES),
    };
    let default_in: cpal::StreamConfig = in_cfg.into();
    let build_input = |cfg: &cpal::StreamConfig| {
        let pcm_tx = pcm_tx.clone();
        let drops = drops.clone();
        let mut first = true;
        in_dev.build_input_stream(
            cfg,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if first {
                    first = false;
                    let frames = data.len() / in_channels.max(1);
                    eprintln!(
                        "voice: capture period: {frames} frames ({:.1} ms)",
                        frames as f64 / 48.0
                    );
                }
                // Downmix to mono (sum/channels — the terminal
                // client's downmix).
                let mono: Vec<f32> = if in_channels > 1 {
                    data.chunks(in_channels)
                        .map(|c| c.iter().sum::<f32>() / in_channels as f32)
                        .collect()
                } else {
                    data.to_vec()
                };
                if pcm_tx.try_send(mono).is_err() {
                    let n = drops.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_multiple_of(128) {
                        eprintln!("voice: encoder behind — {n} frames dropped so far");
                    }
                }
            },
            |e| eprintln!("voice: capture stream error: {e}"),
            None,
        )
    };
    let input = match build_input(&fixed_in) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "voice: capture device rejected a {PERIOD_FRAMES}-frame buffer; using default (higher latency)"
            );
            build_input(&default_in).map_err(|e| format!("build capture stream: {e}"))?
        }
    };

    // --- output stream: mix → device channels, AND feed the AEC its
    // far-end reference (mono downmix of the played mix — the terminal
    // client's output_cb, minus click detection).
    let fixed_out = cpal::StreamConfig {
        channels: out_cfg.channels(),
        sample_rate: out_cfg.sample_rate(),
        buffer_size: cpal::BufferSize::Fixed(PERIOD_FRAMES),
    };
    let default_out: cpal::StreamConfig = out_cfg.into();
    let build_output = |cfg: &cpal::StreamConfig| {
        let mixer = mixer.clone();
        let aec = aec.clone();
        let mut first = true;
        let mut render_carry: Vec<f32> = Vec::new();
        out_dev.build_output_stream(
            cfg,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                let frames = data.len() / out_channels.max(1);
                if first {
                    first = false;
                    eprintln!(
                        "voice: render period: {frames} frames ({:.1} ms)",
                        frames as f64 / 48.0
                    );
                }
                // 6d: the mix is interleaved stereo (mono voices
                // duplicated at the mixer); frames = per-channel
                // frames.
                let mixed = mixer.lock().unwrap().mix(frames);
                // Far-end reference for the AEC: what this callback
                // plays, downmixed to mono. Speech clients' voices are
                // mono-duplicated, so this is exactly the terminal
                // client's reference; music runs without AEC.
                if let Some(ap) = &aec {
                    let mono: Vec<f32> = mixed.chunks(2).map(|p| (p[0] + p[1]) * 0.5).collect();
                    parley_client::aec::analyze_render(ap, &mut render_carry, &mono);
                }
                // ch0 = L, everything else = R (the terminal client's
                // mapping; a mono output device gets L only).
                for (frame, pair) in data.chunks_mut(out_channels.max(1)).zip(mixed.chunks(2)) {
                    for (c, s) in frame.iter_mut().enumerate() {
                        *s = if c == 0 { pair[0] } else { pair[1] };
                    }
                }
            },
            |e| eprintln!("voice: render stream error: {e}"),
            None,
        )
    };
    let output = match build_output(&fixed_out) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "voice: render device rejected a {PERIOD_FRAMES}-frame buffer; using default (higher latency)"
            );
            build_output(&default_out).map_err(|e| format!("build render stream: {e}"))?
        }
    };

    println!(
        "voice: capture \"{in_name}\" 48 kHz {in_channels}ch f32 → opus{}",
        if aec.is_some() { " (AEC+NS on)" } else { "" }
    );
    println!("voice: render \"{out_name}\" 48 kHz {out_channels}ch (stereo mix)");

    Ok(AudioStreams { input, output })
}
