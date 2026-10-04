//! Step 3: voice receive. Media datagram reader + mixer + cpal output,
//! receive-only (step 4 adds the mic and AEC).
//!
//! Architecture (README's locked decision: "UI only; all
//! performance-critical code stays in Rust"): the audio callback is
//! the most performance-critical code in the client, so the device
//! lives HERE — same cpal discipline as the terminal client (48k
//! native, quantum-aligned 1024-frame periods, refuse-not-degrade on
//! rate). Mixer/jitter/PLC are parley-core's (ungated since the 6.9
//! piece-2 promotion — exactly the bridge surface it was built to be).
//!
//! BRIDGE RULE (the step-3 lesson): nothing but plain types crosses
//! to Dart. This module's entry points are pub(crate) — SharedMixer
//! and quinn::Connection in a pub signature made codegen wrap them as
//! opaque objects and emit unresolvable references. session.rs's
//! voice_start(session_id) is the only bridge surface.
//!
//! Ownership: join/leave own VOICE MEMBERSHIP server-side;
//! voice_start/voice_stop own the LOCAL DEVICE. The mixer is the
//! session's, shared both ways.
//!
//! cpal::Stream is !Send/!Sync (cpal 0.15 platform wrapper) — it
//! lives on the device thread's stack, never in the static. The
//! static holds only Send+Sync control. The reader task's stop signal
//! is a tokio channel (selectable — a flag checked between datagrams
//! would sleep forever on idle, and a lingering reader after a stop
//! would split datagrams with its restart successor).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use parley_core::audio::device::{preferred_output_config, PREFERRED_RATE};
use parley_core::audio::mixer::SharedMixer;
use parley_proto::media::FLAG_STEREO;

/// Device period: 1024 frames ≈ 21.3 ms — one PipeWire graph quantum
/// (same value and rationale as the terminal client's audio.rs).
const PERIOD_FRAMES: u32 = 1024;

struct VoiceControl {
    /// Device thread polls this (~100 ms slices — the §8 pw-loop
    /// discipline; idle cost nil).
    stop: Arc<AtomicBool>,
    /// Reader-task stop signal (tokio mpsc: selectable).
    reader_stop: tokio::sync::mpsc::UnboundedSender<()>,
    _device_thread: std::thread::JoinHandle<()>,
}

static VOICE: Mutex<Option<VoiceControl>> = Mutex::new(None);

/// Start listening (receive-only). Called by session::voice_start.
pub(crate) fn voice_start(
    mixer: SharedMixer,
    conn: quinn::Connection,
    my_id: u32,
) -> Result<(), String> {
    let mut guard = VOICE.lock().unwrap();
    // Harvest a stale listening session (connection died, UI tore down
    // before the button could stop it): the reader's death signal set
    // the stop flag, but nobody took VOICE. Drop it before starting
    // fresh — else this call would no-op against a corpse.
    if let Some(ctrl) = guard.as_ref() {
        if ctrl.stop.load(Ordering::Relaxed) {
            *guard = None;
            // The device thread saw the flag and is exiting on its own;
            // the JoinHandle is detached. reader_stop's send below is
            // skipped (channel already closed) — harmless.
        }
    }
    if guard.is_some() {
        return Ok(()); // already listening
    }

    let stop = Arc::new(AtomicBool::new(false));
    let (reader_stop_tx, mut reader_stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // --- media datagram reader (async: quinn's read_datagram is
    // await-only). Runs on the session module's runtime — this fn runs
    // on an FRB worker thread with no tokio context of its own.
    {
        let mixer = mixer.clone();
        let reader_stop_flag = stop.clone();
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
                            // is step 5's seam; skip for now.
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
                            // Connection death reaches the reader FIRST
                            // (datagram stream errors). The device
                            // thread and its cpal stream have no other
                            // path to shutdown — the UI tears down the
                            // Stop button on connection-lost, so nobody
                            // would ever signal them. The reader IS the
                            // death signal: flip the same stop flag the
                            // button uses.
                            reader_stop_flag.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
        });
    }

    // --- device thread: OWNS the cpal stream (it must never leave
    // this thread). Builds, reports readiness, parks until stop,
    // then drops the stream — cpal stops it on drop.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let stop_for_thread = stop.clone();
    let device_thread = std::thread::spawn(move || {
        match build_output_stream(&mixer) {
            Ok(stream) => {
                if let Err(e) = stream.play() {
                    let _ = ready_tx.send(Err(format!("start output: {e}")));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                while !stop_for_thread.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                // scope exit drops the stream: device closed
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    });
    // Build outcome surfaces to the caller; a failure also drops
    // reader_stop_tx (this fn returns) → reader's recv() resolves
    // None → the select arm fires → reader exits. No orphans.
    ready_rx
        .recv()
        .map_err(|e| format!("device thread died: {e}"))??;

    *guard = Some(VoiceControl {
        stop,
        reader_stop: reader_stop_tx,
        _device_thread: device_thread,
    });
    Ok(())
}

/// Open the default output at 48 kHz (refuse-not-degrade), quantum-
/// aligned period with device-default fallback — the terminal
/// client's two-attempt discipline.
fn build_output_stream(mixer: &SharedMixer) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no default output device")?;
    let name = device.name().unwrap_or_else(|_| "?".into());
    let cfg = preferred_output_config(&device).map_err(|e| format!("output config: {e}"))?;
    if cfg.sample_rate().0 != PREFERRED_RATE {
        return Err(format!(
            "output device \"{name}\" at {} Hz, need 48 kHz — refusing to run",
            cfg.sample_rate().0
        ));
    }
    let channels = cfg.channels() as usize;
    let fixed = cpal::StreamConfig {
        channels: cfg.channels(),
        sample_rate: cfg.sample_rate(),
        buffer_size: cpal::BufferSize::Fixed(PERIOD_FRAMES),
    };
    let default_cfg: cpal::StreamConfig = cfg.into();
    let build = |config: &cpal::StreamConfig| {
        let mixer = mixer.clone();
        device.build_output_stream(
            config,
            // The render callback: pull the mix, map interleaved stereo
            // to device channels (ch0=L, rest=R — the terminal
            // client's mapping). No AEC (receive-only: no far-end
            // reference problem exists yet), no click detection.
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                let frames = data.len() / channels.max(1);
                let mixed = mixer.lock().unwrap().mix(frames);
                for (frame, pair) in data.chunks_mut(channels.max(1)).zip(mixed.chunks(2)) {
                    for (c, s) in frame.iter_mut().enumerate() {
                        *s = if c == 0 { pair[0] } else { pair[1] };
                    }
                }
            },
            |e| eprintln!("voice: render stream error: {e}"),
            None,
        )
    };
    match build(&fixed) {
        Ok(s) => {
            println!("voice: listening on \"{name}\" 48 kHz {channels}ch");
            Ok(s)
        }
        Err(_) => {
            eprintln!(
                "voice: device rejected a {PERIOD_FRAMES}-frame buffer; using default (higher latency)"
            );
            build(&default_cfg).map_err(|e| format!("build output stream: {e}"))
        }
    }
}

/// Stop listening: signal both halves. Called manually (button) AND
/// implicitly (reader death → stop flag). Idempotent: whichever fires
/// first wins; the other is a no-op on an already-taken VOICE.
pub(crate) fn voice_stop() {
    if let Some(ctrl) = VOICE.lock().unwrap().take() {
        ctrl.stop.store(true, Ordering::Relaxed);
        let _ = ctrl.reader_stop.send(());
    }
}
