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
//! LOSS LADDER (7e2 — the 4d controller): speech channels run a 1 Hz
//! EMA (0.7/0.3) over the latest peer loss report; above 30‰ the
//! encoder drops to Robust (24 kbps + loss hint 30 — SILK + LBRR, FEC
//! live), back to Clean (64 kbps, hint 5) after 20 s below 10‰, with
//! a 5 s switch cooldown. A separate 5 s reporter sends the worst
//! receive gap-rate of our mix back through the session writer (the
//! server routes + stamps it, never back to us). Music does neither —
//! reports ignored by design. The ladder math is a pure struct
//! (Ladder) unit-tested below; the state machine mirrors
//! crates/parley-client/src/main.rs.
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
use parley_core::audio::pipeline::SendPipeline;
#[cfg(feature = "audio-device")]
use parley_proto::media::FLAG_STEREO;
#[cfg(feature = "audio-device")]
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
#[cfg(feature = "audio-device")]
use std::sync::Arc;
#[cfg(feature = "audio-device")]
use std::time::{Duration, Instant};
#[cfg(feature = "audio-device")]
use tokio::sync::mpsc::UnboundedSender;

#[cfg(feature = "audio-device")]
use crate::api::session::Command;

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
    /// The 5 s loss reporter task's abort handle (7e2; None for music
    /// channels — they send no reports). Drop aborts it.
    reporter_abort: Option<tokio::task::AbortHandle>,
    /// Both cpal streams live on this thread's stack; the thread parks
    /// until the stop flag, then scope exit drops the streams. NOT
    /// joined on Drop: join can block, and Drop runs under the
    /// registry lock — the detached thread self-terminates within
    /// ~100 ms (same discipline as the 7d harvest path).
    _device_thread: std::thread::JoinHandle<()>,
    /// 7g: the capture side's mic tee holds this too — dropping the
    /// control doesn't strand the tee (its mux-routed channel closes
    /// when the share ends anyway).
    #[cfg(feature = "screen-capture")]
    _mic_mux_tx: Option<tokio::sync::mpsc::Sender<Vec<f32>>>,
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
        if let Some(reporter) = &self.reporter_abort {
            reporter.abort();
        }
    }
}

/// 4d's two-profile bitrate controller state (pure — no device or
/// codec deps, unit-tested below). Stepped at 1 Hz by the encoder
/// task with the latest peer loss report.
///
/// Profiles (apply_profile): Clean = 64 kbps + loss hint 5 (CELT, max
/// quality, FEC inert); Robust = 24 kbps + hint 30 (SILK + LBRR, FEC
/// live). EMA over receiver gap-rate reports (0.7/0.3 per step);
/// down when the EMA exceeds 30‰, back up after 20 s below 10‰; a
/// 5 s switch cooldown prevents flapping.
#[cfg(feature = "audio-device")]
struct Ladder {
    ema_permille: f32,
    robust: bool,
    last_switch: Instant,
    low_since: Option<Instant>,
}

#[cfg(feature = "audio-device")]
impl Ladder {
    /// Fresh ladder, Clean, clock zeroed at `now` (injectable for the
    /// tests' deterministic timelines).
    fn new(now: Instant) -> Self {
        Self {
            ema_permille: 0.0,
            robust: false,
            last_switch: now,
            low_since: None,
        }
    }

    /// Current EMA in permille (for the switch log line).
    fn ema_permille(&self) -> f32 {
        self.ema_permille
    }

    /// Step the EMA with one observation; Some(new_robust) on a
    /// profile switch, None otherwise.
    fn step(&mut self, observed_permille: u32, now: Instant) -> Option<bool> {
        self.ema_permille = 0.7 * self.ema_permille + 0.3 * observed_permille as f32;
        if !self.robust {
            if self.ema_permille > 30.0 && now.duration_since(self.last_switch).as_secs() >= 5 {
                self.robust = true;
                self.last_switch = now;
                self.low_since = None;
                Some(true)
            } else {
                None
            }
        } else if self.ema_permille < 10.0 {
            let since = *self.low_since.get_or_insert(now);
            if now.duration_since(since).as_secs() >= 20
                && now.duration_since(self.last_switch).as_secs() >= 5
            {
                self.robust = false;
                self.last_switch = now;
                self.low_since = None;
                Some(false)
            } else {
                None
            }
        } else {
            self.low_since = None;
            None
        }
    }
}

/// 4d's profile application: lock-free bitrate + FEC-hint flip on the
/// live SendPipeline (the encoder task owns it mutably).
#[cfg(feature = "audio-device")]
fn apply_profile(send: &mut SendPipeline, robust: bool) {
    let (rate, loss) = if robust { (24_000, 30) } else { (64_000, 5) };
    send.set_bitrate(rate);
    send.set_loss_hint(loss);
}

/// Build the whole voice path for one session: media reader (feed:
/// mixer), output + input device streams, AEC, encoder task, and — in
/// speech channels — the 5 s loss reporter and the send-side bitrate
/// ladder (7e2). Blocks until the devices report readiness (device
/// build is ~100 ms typical) — the caller MUST run this off the
/// registry lock (see the module docs). Returns the control whose
/// Drop tears everything down.
///
/// `loss_report` is the session's slot for the latest peer loss
/// report (to_event writes it; the ladder's 1 Hz tick reads it);
/// `cmd_tx` feeds the reporter's ReceiverReport through the session
/// writer.
///
/// A partial failure tears its own parts down: the encoder and
/// reporter abort and reader_stop drops when this fn returns Err, and
/// the device thread (if it started) holds no stream past its scope
/// exit.
#[cfg(feature = "audio-device")]
#[cfg_attr(feature = "screen-capture", allow(clippy::too_many_arguments))]
pub(crate) fn start_voice(
    mixer: SharedMixer,
    conn: quinn::Connection,
    my_id: u32,
    music: bool,
    loss_report: Arc<AtomicU32>,
    cmd_tx: UnboundedSender<Command>,
    // 7f: the session's video feed — video datagrams land on this
    // same media path, and the session's video task consumes the
    // channel (3b: decode never on this reader). None in featureless
    // builds (video datagrams are dropped).
    #[cfg(feature = "audio-device")] video_tx: Option<
        // 7h: (sender_id, declared codec id, payload) — the codec rides
        // the media flag bits; see the send site below.
        tokio::sync::mpsc::UnboundedSender<(u32, u8, Vec<u8>)>,
    >,
    // Mic mute: the capture callback reads this per device period and
    // drops the chunk entirely. Session-owned, so the state survives a
    // leave/join and dies with the session.
    muted: Arc<AtomicBool>,
    // 7g: the mic tee routes captured chunks into this mux channel
    // while the share-audio mux owns the route flag; the mux drains it
    // and mixes the mic with the share's system audio. None when the
    // share feature is off (no tee → mic rides the normal encoder).
    #[cfg(feature = "screen-capture")] mic_mux_tx: Option<tokio::sync::mpsc::Sender<Vec<f32>>>,
) -> Result<VoiceControl, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (reader_stop_tx, mut reader_stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // --- media datagram reader (async: quinn's read_datagram is
    // await-only). Runs on the session module's runtime. Feed: mixer
    // (audio) + video task (FLAG_VIDEO payloads — 7f). Reader death
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
                            // 7f: video rides the same datagrams —
                            // hand the payload to the session's video
                            // task (the hub assembles + decodes there,
                            // never on this reader). No video feature:
                            // dropped.
                            if m.flags & parley_proto::media::FLAG_VIDEO != 0 {
                                if let Some(tx) = &video_tx {
                                    // 7h: carry the sender's declared
                                    // codec so the hub can refuse a
                                    // stream this build cannot decode
                                    // instead of feeding foreign bytes
                                    // to an H.264 decoder that rejects
                                    // every frame and says nothing.
                                    let _ = tx.send((
                                        m.sender_id,
                                        parley_proto::media::video_codec(m.flags),
                                        m.payload.to_vec(),
                                    ));
                                }
                                continue;
                            }
                            let stereo = m.flags & FLAG_STEREO != 0;
                            let share = m.flags & parley_proto::media::FLAG_SHARE_AUDIO != 0;
                            // 7k: a share's system audio stays silent until
                            // the viewer asks for it. Everyone in a channel
                            // should not hear someone's music just because
                            // they started sharing — the sender's VOICE
                            // rides its own stream and is never gated here.
                            if share && !crate::api::session::share_audio_audible(m.sender_id)
                            {
                                continue;
                            }
                            if let Err(e) = mixer.lock().unwrap().push_packet(
                                m.sender_id,
                                m.seq,
                                m.payload,
                                stereo,
                                share,
                            ) {
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
    let (pcm_tx, pcm_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(16);

    // 7g mic tee: while the share-audio mux owns the route flag, chunks
    // go to the mux (it mixes mic + system audio); otherwise to the
    // normal encoder below. Same tee shape as the terminal client's —
    // try_send on both sides: stale chunks drop, never blocks the RT
    // callback. Installed only when the share mux channel exists.
    #[cfg(feature = "screen-capture")]
    let mic_mux_tx_keep = mic_mux_tx.clone();
    #[cfg(feature = "screen-capture")]
    let mut pcm_rx = if let Some(mux_tx) = mic_mux_tx {
        let route = crate::api::session::share_mic_route().clone();
        let (tee_enc_tx, tee_enc_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(16);
        // 7k: the AEC runs HERE, before the split, so the mux's mic leg
        // is echo-cancelled too. Routing the raw mic into the share
        // (the 7g shape) meant a sharer's share carried whatever their
        // speakers played — the acoustic half of the feedback loop.
        // Processing before the split keeps each chunk processed exactly
        // once (`aec_in_tee` below tells the encoder not to repeat it).
        let tee_aec = aec.clone();
        crate::api::session::rt().spawn(async move {
            let mut pcm_rx = pcm_rx;
            let mut asm = parley_client::aec::FrameAssembler::new();
            loop {
                let Some(chunk) = pcm_rx.recv().await else {
                    break;
                };
                let chunk: Vec<f32> = match tee_aec.as_ref() {
                    Some(ap) => {
                        let mut out = Vec::new();
                        parley_client::aec::process_capture(ap, &mut asm, &chunk, &mut out);
                        out
                    }
                    None => chunk,
                };
                if chunk.is_empty() {
                    continue;
                }
                if route.load(Ordering::Relaxed) {
                    let _ = mux_tx.try_send(chunk);
                    continue;
                }
                if tee_enc_tx.try_send(chunk).is_err() {
                    break;
                }
            }
        });
        tee_enc_rx
    } else {
        pcm_rx
    };
    #[cfg(feature = "screen-capture")]
    let aec_in_tee = mic_mux_tx_keep.is_some();
    #[cfg(not(feature = "screen-capture"))]
    let aec_in_tee = false;
    #[cfg(not(feature = "screen-capture"))]
    let mut pcm_rx = pcm_rx;

    // --- encoder task: AEC → i16 → SendPipeline → datagrams. Exits
    // when the capture sender drops (teardown) or the connection dies.
    let encoder_task = {
        let conn = conn.clone();
        // 7k: with the tee installed (screen-capture builds) the mic is
        // already echo-cancelled upstream — processing it again here
        // would feed the canceller its own output.
        let aec = if aec_in_tee { None } else { aec.clone() };
        crate::api::session::rt().spawn(async move {
            let mut send = if music {
                match SendPipeline::new_music(256_000) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("voice: music encoder init failed: {e}");
                        return;
                    }
                }
            } else {
                match SendPipeline::new(64_000) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("voice: encoder init failed: {e}");
                        return;
                    }
                }
            };
            let mut asm = parley_client::aec::FrameAssembler::new();
            // 7e2 (4d): the ladder lives IN the encoder task because
            // this task owns `send` mutably — no locks on the encode
            // path. Music: the tick arm never runs its body (no
            // profile flips; reports are never sent either).
            let mut ladder = Ladder::new(Instant::now());
            let mut ladder_tick = tokio::time::interval(Duration::from_secs(1));
            if !music {
                apply_profile(&mut send, false); // explicit Clean at startup (4d)
            }
            loop {
                tokio::select! {
                    chunk = pcm_rx.recv() => {
                        let Some(chunk) = chunk else {
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
                    _ = ladder_tick.tick() => {
                        if music {
                            continue; // 4d: music ignores reports by design
                        }
                        let observed = loss_report.load(Ordering::Relaxed);
                        if let Some(robust) = ladder.step(observed, Instant::now()) {
                            apply_profile(&mut send, robust);
                            println!(
                                "voice: bitrate → {} — loss {:.1}%",
                                if robust {
                                    "Robust (24 kbps + FEC)"
                                } else {
                                    "Clean (64 kbps)"
                                },
                                ladder.ema_permille() / 10.0
                            );
                        }
                    }
                }
            }
        })
    };

    // --- 4d loss reporter (speech only): one aggregate worst-peer
    // report per 5 s window; the server routes it to the voice
    // channel's other members, stamped with our authenticated id and
    // never back to us. Music channels send nothing — there is no
    // ladder to steer (4d's rule). Exits when the session writer
    // dies; VoiceControl::Drop aborts it.
    let reporter_abort = if music {
        None
    } else {
        let rep_mixer = mixer.clone();
        let rep_cmd = cmd_tx.clone();
        Some(
            crate::api::session::rt()
                .spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_secs(5));
                    loop {
                        tick.tick().await;
                        let worst = {
                            let rates = rep_mixer.lock().unwrap().gap_rates();
                            if rates.is_empty() {
                                continue; // no active voices: nothing to report
                            }
                            rates.iter().map(|&(_, r)| r).max().unwrap_or(0)
                        };
                        if rep_cmd
                            .send(Command::ReceiverReport {
                                gap_rate_permille: worst,
                            })
                            .is_err()
                        {
                            break; // writer gone: session over
                        }
                    }
                })
                .abort_handle(),
        )
    };

    // --- device thread: OWNS both cpal streams (they must never leave
    // this thread). Builds, reports readiness, parks until stop, then
    // drops them — cpal stops streams on drop.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let stop_for_thread = stop.clone();
    let device_thread = std::thread::spawn(move || {
        match build_streams(&mixer, aec, pcm_tx, muted) {
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
    // reader_stop_tx and aborts the encoder + reporter (this fn
    // returns Err → VoiceControl never constructed). No orphans.
    if let Err(e) = ready_rx
        .recv()
        .map_err(|e| format!("device thread died: {e}"))?
    {
        encoder_task.abort();
        if let Some(reporter) = &reporter_abort {
            reporter.abort();
        }
        return Err(e);
    }

    Ok(VoiceControl {
        stop,
        reader_stop: reader_stop_tx,
        encoder_abort: encoder_task.abort_handle(),
        reporter_abort,
        _device_thread: device_thread,
        #[cfg(feature = "screen-capture")]
        _mic_mux_tx: mic_mux_tx_keep,
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
    muted: Arc<AtomicBool>,
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
        let muted = muted.clone();
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
                // Muted: drop the chunk before the encoder. Sending
                // nothing (rather than encoded silence) keeps the
                // muted mic off the wire entirely.
                if muted.load(Ordering::Relaxed) {
                    return;
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
                // Far-end reference: what this callback plays, downmixed
                // to mono. Two consumers — the mic's AEC (speech only)
                // and the 7k share-audio filter, which subtracts our own
                // playback from the captured system mix so a screen
                // share can never carry the voice channel (or another
                // share) back out as a feedback loop. Speech clients'
                // voices are mono-duplicated, so this is exactly the
                // terminal client's reference.
                if aec.is_some() || crate::api::share_echo::active() {
                    let mono: Vec<f32> = mixed.chunks(2).map(|p| (p[0] + p[1]) * 0.5).collect();
                    if let Some(ap) = &aec {
                        parley_client::aec::analyze_render(ap, &mut render_carry, &mono);
                    }
                    crate::api::share_echo::push_render(&mono);
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

/// 7e2: the ladder's pure state machine — the two thresholds, both
/// cooldowns, the EMA smoothing, and the low-window reset, on
/// deterministic timelines (no devices, no codec, no async).
#[cfg(all(test, feature = "audio-device"))]
mod ladder_tests {
    use super::Ladder;
    use std::time::{Duration, Instant};

    fn t0() -> Instant {
        Instant::now()
    }

    /// A single 10% blip must not flip the profile: the EMA lands at
    /// ~30‰ and decays back below the threshold before the cooldown
    /// even elapses.
    #[test]
    fn single_loss_blip_does_not_switch() {
        let t = t0();
        let mut l = Ladder::new(t);
        assert_eq!(l.step(100, t), None);
        assert_eq!(l.step(0, t + Duration::from_secs(5)), None);
    }

    /// Sustained loss above 30‰ switches to Robust — but only once
    /// the 5 s switch cooldown has elapsed.
    #[test]
    fn clean_to_robust_needs_threshold_and_cooldown() {
        let t = t0();
        let mut l = Ladder::new(t);
        assert_eq!(l.step(100, t + Duration::from_secs(1)), None); // ema ~30, cooldown
        assert_eq!(l.step(100, t + Duration::from_secs(4)), None); // ema ~51, cooldown
        assert_eq!(l.step(100, t + Duration::from_secs(5)), Some(true)); // ema ~65.7
    }

    /// The threshold is strict: 29.7‰ is not enough even with the
    /// cooldown long elapsed.
    #[test]
    fn threshold_is_strictly_above_30() {
        let t = t0();
        let mut l = Ladder::new(t);
        assert_eq!(l.step(99, t + Duration::from_secs(10)), None); // ema 29.7
        assert_eq!(l.step(99, t + Duration::from_secs(11)), Some(true)); // ema ~50.5
    }

    /// Recovery needs 20 s CONTINUOUSLY below 10‰ (plus the 5 s
    /// cooldown, long since satisfied here).
    #[test]
    fn robust_to_clean_needs_20s_below_10() {
        let t = t0();
        let mut l = Ladder::new(t);
        assert_eq!(l.step(100, t), None);
        assert_eq!(l.step(100, t + Duration::from_secs(5)), Some(true)); // ema ~51.0
                                                                         // Clean reports decay the EMA below 10‰ on the 5th step —
                                                                         // the low window just started, no switch yet.
        for i in 1..=6 {
            assert_eq!(l.step(0, t + Duration::from_secs(5 + i)), None);
        }
        assert_eq!(l.step(0, t + Duration::from_secs(29)), None); // 19 s below
        assert_eq!(l.step(0, t + Duration::from_secs(30)), Some(false)); // 20 s below
    }

    /// A loss burst mid-recovery restarts the 20 s clock: 20 s past
    /// the ORIGINAL low point must not switch.
    #[test]
    fn rising_ema_resets_the_low_window() {
        let t = t0();
        let mut l = Ladder::new(t);
        assert_eq!(l.step(100, t), None);
        assert_eq!(l.step(100, t + Duration::from_secs(5)), Some(true)); // ema ~65.7
        for i in 1..=6 {
            l.step(0, t + Duration::from_secs(5 + i)); // ema ~6.0 at t+11 s; low_since = t+10
        }
        assert_eq!(l.step(30, t + Duration::from_secs(12)), None); // ema ~13.2 → reset
        assert_eq!(l.step(0, t + Duration::from_secs(13)), None); // ema ~9.2 → new low_since
        assert_eq!(l.step(0, t + Duration::from_secs(14)), None); // ema ~6.5 — window continues
        assert_eq!(l.step(0, t + Duration::from_secs(31)), None); // 18 s on the NEW clock
                                                                  // (21 s past the original — proves the reset.)
        assert_eq!(l.step(0, t + Duration::from_secs(34)), Some(false)); // 21 s on it
    }

    /// A clean channel never leaves Clean, no matter how long.
    #[test]
    fn zero_loss_never_leaves_clean() {
        let t = t0();
        let mut l = Ladder::new(t);
        for i in 1..=10 {
            assert_eq!(l.step(0, t + Duration::from_secs(i)), None);
        }
        assert!(l.ema_permille().abs() < 0.001);
    }
}
