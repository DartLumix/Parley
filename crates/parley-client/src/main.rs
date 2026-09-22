//! parley-test-client: connect, auth, join a voice channel, pump audio.
//! Without the `audio-device` feature it's a control-only debug client
//! (chat/presence, no mic or speakers) — that's what CI builds.
//!
//! Latency measurement (Phase 3f): --measure injects a click every 2 s,
//! --report detects it in the render callback and reports back over
//! chat. Stay quiet while measuring; the clicks are loud.
//!
//! Per-user volume (Phase 4b): --volume <user_id>:<gain>, repeatable.
//!
//! AEC/NS (Phase 4a): on by default, --no-aec disables.
//!
//! Loss-adaptive bitrate (Phase 4d): receivers report gap rates to their
//! voice-channel peers; senders switch between Clean (64 kbps, FEC
//! inert) and Robust (24 kbps + aggressive in-band FEC) on an EMA with
//! hysteresis.
//!
//! Share lifecycle (Phase 6a): "share" / "stop" typed on stdin drive
//! start/stop without process exit; --share is just a "share" command
//! at boot. Stopping tears down the portal session + pipewire + the
//! encoder, then signals StopShare; viewers close their windows.
//!
//! Session tokens (Phase 6c): every password login prints the issued
//! token; reconnect with --token <tok> (no --user/--password needed).
//! A silently-dead peer is reaped by the server's QUIC idle timeout.
//!
//! Music channels (Phase 6d): the channel's mode (fetched from the
//! server after auth) selects the profile — speech (mono, FEC, ladder)
//! or music (stereo interleaved, 256 kbps, no FEC, no ladder, no AEC).
//! --music replaces the mic with synthetic 440 Hz L / 880 Hz R test
//! tones so stereo separation is audible and measurable.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parley_core::session;
use parley_proto::{
    AuthDenied, AuthOk, AuthRequest, ChannelMode, ClientBody, ClientMessage, Heartbeat, Hello,
    HelloAck, JoinVoice, ListChannels, PROTOCOL_VERSION, ServerBody, ServerMessage, StopShare,
};
use prost::Message as _;

use parley_client::ShareEvent;
use parley_client::cert;
#[cfg(feature = "screen-capture")]
use parley_client::screen;
#[cfg(feature = "video")]
use parley_client::video;
#[cfg(feature = "audio-device")]
use parley_client::{aec, audio, mixer};
#[cfg(feature = "audio-device")]
use parley_core::audio::pipeline::SendPipeline;
#[cfg(feature = "audio-device")]
use parley_proto::ReceiverReport;
#[cfg(feature = "video")]
use parley_proto::VideoReport;

struct Args {
    server: String,
    user: String,
    password: String,
    token: Option<String>,
    channel_id: u64,
    measure: bool,
    report: bool,
    noaec: bool,
    music: bool,
    volumes: Vec<(u32, f32)>,
    share: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut server = None;
    let mut user = None;
    let mut password = None;
    let mut token = None;
    let mut channel_id = 1u64;
    let (mut measure, mut report, mut noaec, mut music) = (false, false, false, false);
    let mut volumes = Vec::new();
    let mut it = std::env::args().skip(1);
    let mut share = false;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server" => server = it.next(),
            "--user" => user = it.next(),
            "--password" => password = it.next(),
            "--token" => token = it.next(),
            "--channel" => {
                channel_id = it.next().and_then(|v| v.parse().ok()).unwrap_or(channel_id)
            }
            "--measure" => measure = true,
            "--report" => report = true,
            "--no-aec" => noaec = true,
            "--music" => music = true,
            "--volume" => {
                let spec = it.next().ok_or("--volume needs <user_id>:<gain>")?;
                let (id, gain) = spec
                    .split_once(':')
                    .ok_or("--volume format is <user_id>:<gain>, e.g. 2:0.5")?;
                let id: u32 = id.parse().map_err(|_| format!("bad user id: {id}"))?;
                let gain: f32 = gain.parse().map_err(|_| format!("bad gain: {gain}"))?;
                volumes.push((id, gain.clamp(0.0, 4.0)));
            }
            "--share" => share = true,
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let token = token.or_else(|| std::env::var("PARLEY_TOKEN").ok());
    // Token login needs no credentials; password login needs both.
    let (user, password) = if token.is_some() {
        (user.unwrap_or_default(), password.unwrap_or_default())
    } else {
        (
            user.or_else(|| std::env::var("PARLEY_USER").ok())
                .ok_or("missing --user (or PARLEY_USER env)")?,
            password
                .or_else(|| std::env::var("PARLEY_PASSWORD").ok())
                .ok_or("missing --password (or PARLEY_PASSWORD env)")?,
        )
    };
    Ok(Args {
        server: server.unwrap_or_else(|| "127.0.0.1:4443".to_string()),
        user,
        password,
        token,
        channel_id,
        measure,
        report,
        noaec,
        music,
        volumes,
        share,
    })
}

/// Stdin commands (6a): drive the share lifecycle without exiting.
enum Command {
    Share,
    Stop,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read one length-prefixed frame raw, so the auth phase can try decoding
/// as AuthOk OR AuthDenied (auth-phase messages are sent bare, pre-oneof).
async fn read_frame(rx: &mut quinn::RecvStream) -> Result<Vec<u8>, String> {
    let mut len = [0u8; 4];
    rx.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let n = u32::from_be_bytes(len) as usize;
    if n > parley_proto::frame::MAX_SIZE {
        return Err(format!("frame of {n} bytes exceeds MAX_SIZE"));
    }
    let mut buf = vec![0u8; n];
    rx.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    Ok(buf)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let started = std::time::Instant::now();
    let addr: SocketAddr = args.server.parse().map_err(|_| {
        format!(
            "--server must be IP:port for now (got \"{}\") — DNS names come with the real client",
            args.server
        )
    })?;
    let server_name = args
        .server
        .split_once(':')
        .map(|(h, _)| h.to_string())
        .unwrap_or_else(|| args.server.clone());

    // --- connect (TOFU) ---
    let (pin, hosts_path) = cert::load_pin(&args.server)?;
    let (endpoint, seen) = cert::build_endpoint(pin.clone());
    let conn = endpoint.connect(addr, &server_name)?.await?;

    // --- handshake + auth (bare messages) ---
    let (mut tx, mut rx) = conn.open_bi().await?;
    session::write_message(
        &mut tx,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await?;
    let ack: HelloAck = session::read_message(&mut rx).await?;

    session::write_message(
        &mut tx,
        &AuthRequest {
            username: args.user.clone(),
            password: args.password.clone(),
            session_token: args.token.clone().unwrap_or_default(),
        },
    )
    .await?;
    let payload = read_frame(&mut rx).await?;
    let ok = if let Ok(ok) = AuthOk::decode(payload.as_slice()) {
        ok
    } else if let Ok(denied) = AuthDenied::decode(payload.as_slice()) {
        return Err(if args.token.is_some() {
            format!(
                "token auth denied: {} (stale token? a password login issues a fresh one)",
                denied.reason
            )
        } else {
            format!("auth denied: {}", denied.reason)
        }
        .into());
    } else {
        return Err("unexpected message during auth".into());
    };
    if args.token.is_some() {
        println!("relogged in via session token (user id {})", ok.user_id);
    } else {
        println!("session token (relogin with --token): {}", ok.session_token);
    }

    // --- 6d: channel mode discovery (before the writer task takes tx) ---
    let music_mode = {
        session::write_message(
            &mut tx,
            &ClientMessage {
                body: Some(ClientBody::ListChannels(ListChannels {})),
            },
        )
        .await?;
        loop {
            let msg: ServerMessage = session::read_message(&mut rx).await?;
            if let Some(ServerBody::ChannelList(list)) = msg.body {
                let Some(ch) = list.channels.iter().find(|c| c.id == args.channel_id) else {
                    return Err(format!(
                        "channel {} does not exist on this server",
                        args.channel_id
                    )
                    .into());
                };
                break ch.mode() == ChannelMode::Music;
            }
            // skip anything that interleaves (broadcasts from other users)
        }
    };
    println!(
        "channel mode: {}",
        if music_mode {
            "music (256 kbps stereo)"
        } else {
            "speech"
        }
    );

    // --- join voice, wait for our own confirmation ---
    session::write_message(
        &mut tx,
        &ClientMessage {
            body: Some(ClientBody::JoinVoice(JoinVoice {
                channel_id: args.channel_id,
            })),
        },
    )
    .await?;
    loop {
        let msg: ServerMessage = session::read_message(&mut rx).await?;
        if let Some(ServerBody::VoiceState(v)) = &msg.body
            && v.user_id == ok.user_id
            && v.joined
            && v.channel_id == args.channel_id
        {
            break;
        }
    }

    // --- TOFU: pin the server we just fully authenticated against ---
    if pin.is_none() {
        let fp = seen
            .lock()
            .unwrap()
            .clone()
            .ok_or("server presented no certificate")?;
        cert::save_pin(&hosts_path, &args.server, &fp)?;
        println!(
            "TOFU: pinned {fp} for {} in {}",
            args.server,
            hosts_path.display()
        );
        println!("      (sanity-check it against the fingerprint the server prints on startup)");
    }

    println!(
        "connected to {} as \"{}\" (id {}), voice channel {} — {} ms",
        args.server,
        args.user,
        ok.user_id,
        args.channel_id,
        started.elapsed().as_millis()
    );

    // Outbound control channel: heartbeats, click reports and loss
    // reports share one writer task.
    let (_outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

    // 4d: last loss report seen from a peer, permille. Written by the
    // control reader, polled by the bitrate controller in the encode task.
    let loss_state = Arc::new(AtomicU32::new(0));

    // 6e: last video loss report seen from a peer, permille. Written by
    // the control reader, polled by the share senders' ladder.
    let video_loss = Arc::new(AtomicU32::new(0));

    // 5d: keyframe-on-join — filled by the share command task once the
    // capture pipeline is up; the control reader pokes it when a
    // viewer joins our voice channel.
    #[cfg(feature = "screen-capture")]
    let keyframe_poke: Arc<Mutex<Option<screen::ShareHandle>>> = Arc::new(Mutex::new(None));

    // 5c: per-sender video viewers — shared by the datagram reader,
    // control reader (cleanup), and the stats ticker.
    #[cfg(feature = "video")]
    let video_viewers = parley_client::video::new_viewers();

    // Feature-mismatch warnings, split per feature so every Args field
    // stays read in every feature combination (dead_code is per-build:
    // audio-device-only never reads `share`; video-only never reads
    // report/noaec/music/volumes — both fired under -D warnings).
    #[cfg(not(feature = "audio-device"))]
    if args.measure || args.report || args.noaec || args.music || !args.volumes.is_empty() {
        eprintln!(
            "--measure/--report/--volume/--no-aec/--music need the audio-device feature; ignoring"
        );
    }
    #[cfg(not(feature = "video"))]
    if args.share {
        eprintln!("--share needs the video feature; ignoring");
    }

    // --- audio (feature-gated) ---
    #[cfg(feature = "audio-device")]
    let shared_mixer = mixer::new();
    #[cfg(feature = "audio-device")]
    for &(id, gain) in &args.volumes {
        mixer::set_gain(&shared_mixer, id, gain);
        println!("volume: user {id} at {gain:.2}×");
    }
    #[cfg(feature = "audio-device")]
    let aec_proc = if music_mode {
        // Music profile: AEC is a mic-path speech tool — off by design.
        println!("aec: off (music channel)");
        None
    } else if args.noaec {
        println!("aec: disabled (--no-aec)");
        None
    } else {
        match aec::new_processor() {
            Ok(p) => {
                println!(
                    "aec: WebRTC AEC3 + noise suppression + high-pass on (--no-aec to disable)"
                );
                Some(p)
            }
            Err(e) => {
                eprintln!("aec: init failed ({e}); continuing without echo cancellation");
                None
            }
        }
    };
    #[cfg(feature = "audio-device")]
    let injections = args
        .measure
        .then(|| Arc::new(Mutex::new(Vec::<u64>::new())));
    #[cfg(not(feature = "audio-device"))]
    let injections: Option<Arc<Mutex<Vec<u64>>>> = None;
    #[cfg(feature = "audio-device")]
    let runtime = audio::start(
        shared_mixer.clone(),
        injections.clone(),
        args.report.then(|| _outbound_tx.clone()),
        aec_proc.clone(),
    )?;
    #[cfg(feature = "video")]
    {
        let v = video_viewers.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                println!("{}", parley_client::video::stats_line(&v));
            }
        });
    }

    // 6e: video loss reporter — worst per-sender video gap rate every
    // 5 s, routed by the server to channel members (the sharer reads
    // it through video_loss). NOT music-gated: screen share in a music
    // channel still needs its ladder.
    #[cfg(feature = "video")]
    {
        let rep_viewers = video_viewers.clone();
        let rep_outbound = _outbound_tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let Some(worst) = video::video_gap_rates(&rep_viewers) else {
                    continue; // no active viewers: nothing to report
                };
                let _ = rep_outbound.send(ClientMessage {
                    body: Some(ClientBody::VideoReport(VideoReport {
                        gap_rate_permille: worst,
                        reporter_id: 0, // server stamps the authenticated id
                    })),
                });
            }
        });
    }

    // --- writer: heartbeats + anything the audio side posts ---
    let hb_ms = if ack.heartbeat_ms == 0 {
        5000
    } else {
        ack.heartbeat_ms
    };
    tokio::spawn(async move {
        let mut tx = tx;
        let mut tick = tokio::time::interval(Duration::from_millis(hb_ms as u64));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let msg = ClientMessage {
                        body: Some(ClientBody::Heartbeat(Heartbeat {
                            client_time_ms: now_ms(),
                        })),
                    };
                    if session::write_message(&mut tx, &msg).await.is_err() {
                        break;
                    }
                }
                msg = outbound_rx.recv() => {
                    if let Some(msg) = msg {
                        if session::write_message(&mut tx, &msg).await.is_err() {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
    });

    // --- control stream reader: chat / voice states / loss reports ---
    #[cfg(feature = "audio-device")]
    let ctrl_mixer = shared_mixer.clone();
    #[cfg(feature = "video")]
    let ctrl_viewers = video_viewers.clone();
    #[cfg(feature = "screen-capture")]
    let ctrl_poke = keyframe_poke.clone();
    let ctrl_loss_state = loss_state.clone();
    let ctrl_video_loss = video_loss.clone();
    let measure_flag = args.measure;
    tokio::spawn(async move {
        let mut deltas: Vec<i64> = Vec::new();
        loop {
            match session::read_message::<ServerMessage>(&mut rx).await {
                Ok(msg) => match msg.body {
                    Some(ServerBody::ChatMessage(m)) => {
                        if let Some(rest) = m.text.strip_prefix("PARLEY_LATENCY ") {
                            let Ok(detected) = rest.trim().parse::<i64>() else {
                                continue;
                            };
                            let Some(injections) = injections.as_ref() else {
                                continue;
                            };
                            // Pair with the latest injection at or before
                            // the detection; unmatched reports are noise.
                            let t0 = {
                                let list = injections.lock().unwrap();
                                match list.iter().rev().find(|&&t| (t as i64) <= detected) {
                                    Some(&t) => t as i64,
                                    None => continue,
                                }
                            };
                            let delta_us = detected - t0;
                            if delta_us <= 0 || delta_us > 1_000_000 {
                                continue; // spurious pairing
                            }
                            deltas.push(delta_us);
                            println!(
                                "latency: {:.1} ms (digital callback→render)",
                                delta_us as f64 / 1000.0
                            );
                            if deltas.len().is_multiple_of(5) {
                                let mut sorted = deltas.clone();
                                sorted.sort_unstable();
                                println!(
                                    "--- {} samples: min {:.1} / median {:.1} / max {:.1} ms",
                                    sorted.len(),
                                    sorted[0] as f64 / 1000.0,
                                    sorted[sorted.len() / 2] as f64 / 1000.0,
                                    sorted[sorted.len() - 1] as f64 / 1000.0,
                                );
                            }
                        } else {
                            println!("[{}] {}", m.username, m.text);
                        }
                    }
                    Some(ServerBody::VoiceState(v)) => {
                        println!(
                            "user {} {} voice channel {}",
                            v.user_id,
                            if v.joined { "joined" } else { "left" },
                            v.channel_id
                        );
                        // 5d: a viewer joining our channel gets a keyframe
                        // immediately — drop-until-IDR would otherwise cost
                        // them up to one full intra period.
                        #[cfg(feature = "screen-capture")]
                        if v.joined
                            && v.user_id != ok.user_id
                            && let Some(h) = ctrl_poke.lock().unwrap().as_ref()
                        {
                            h.force_keyframe();
                        }
                        if !v.joined {
                            #[cfg(feature = "audio-device")]
                            mixer::remove(&ctrl_mixer, v.user_id);
                            #[cfg(feature = "video")]
                            video::remove_viewer(&ctrl_viewers, v.user_id);
                        }
                    }
                    Some(ServerBody::ShareStopped(s)) => {
                        // 6a: the sharer's echo is ignored by user_id;
                        // viewers tear down (and freeze-proof against
                        // straggler datagrams) on this.
                        if s.user_id != ok.user_id {
                            println!("video: user {} stopped sharing", s.user_id);
                            #[cfg(feature = "video")]
                            video::mark_stopped(&ctrl_viewers, s.user_id);
                        }
                    }
                    Some(ServerBody::ReceiverReport(rep)) => {
                        // 4d: a peer's loss report steers our send profile
                        // (speech channels only — music ignores reports).
                        // Skip our own echo and unstamped reports.
                        if rep.reporter_id != 0 && rep.reporter_id != ok.user_id {
                            ctrl_loss_state.store(rep.gap_rate_permille, Ordering::Relaxed);
                        }
                    }

                    Some(ServerBody::VideoReport(rep)) => {
                        // 6e: a viewer's video loss report steers our
                        // share bitrate. Skip own echo and unstamped.
                        if rep.reporter_id != 0 && rep.reporter_id != ok.user_id {
                            ctrl_video_loss.store(rep.gap_rate_permille, Ordering::Relaxed);
                        }
                    }
                    Some(ServerBody::HeartbeatAck(a)) if measure_flag => {
                        println!(
                            "heartbeat rtt: {} ms",
                            now_ms().saturating_sub(a.client_time_ms)
                        );
                    }
                    _ => {}
                },
                Err(_) => {
                    println!("control stream closed");
                    break;
                }
            }
        }
    });

    // --- media: mic → [AEC/NS] → SendPipeline → datagrams ---
    #[cfg(feature = "audio-device")]
    {
        let mut pcm_rx = runtime.pcm_rx;
        let conn_send = conn.clone();
        let enc_loss_state = loss_state.clone();
        let synth_music = args.music;
        tokio::spawn(async move {
            if music_mode {
                // --- 6d music profile: stereo 256 kbps, no FEC, no
                // ladder (receiver reports are ignored by design). ---
                let mut send = match SendPipeline::new_music(256_000) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("opus music encoder init failed: {e}");
                        return;
                    }
                };
                let flags = parley_proto::media::FLAG_SPEAKING | parley_proto::media::FLAG_STEREO;
                let send_frame =
                    |send: &mut SendPipeline, frame: &[i16], out: &mut Vec<(u16, u32, Vec<u8>)>| {
                        if let Err(e) = send.push_pcm(frame, out) {
                            eprintln!("encode failed: {e}");
                        }
                    };
                if synth_music {
                    // Synthetic source: 440 Hz L / 880 Hz R test tones,
                    // real-time paced at 20 ms — audible/measurable
                    // stereo separation without a stereo mic.
                    use std::f64::consts::PI;
                    let spf = parley_core::audio::SAMPLES_PER_FRAME;
                    let mut t: usize = 0;
                    let mut tick = tokio::time::interval(Duration::from_millis(20));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    println!("music: sending 440 Hz L / 880 Hz R test tones");
                    loop {
                        tick.tick().await;
                        let base = t * spf;
                        let frame: Vec<i16> = (0..spf)
                            .flat_map(|i| {
                                let ti = (base + i) as f64 / 48_000.0;
                                [
                                    (0.3 * (2.0 * PI * 440.0 * ti).sin() * 32767.0) as i16,
                                    (0.3 * (2.0 * PI * 880.0 * ti).sin() * 32767.0) as i16,
                                ]
                            })
                            .collect();
                        t += 1;
                        let mut packets = Vec::new();
                        send_frame(&mut send, &frame, &mut packets);
                        for (seq, ts, pkt) in packets {
                            let dg = parley_proto::media::encode(flags, seq, ts, 0, &pkt);
                            if conn_send.send_datagram(dg.into()).is_err() {
                                return; // connection gone
                            }
                        }
                    }
                } else {
                    // Mic (mono) upmixed to stereo: a voice in a music
                    // channel is a centered mono source.
                    loop {
                        let Some(chunk) = pcm_rx.recv().await else {
                            break;
                        };
                        let stereo: Vec<i16> = chunk
                            .iter()
                            .flat_map(|&s| {
                                let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                                [v, v]
                            })
                            .collect();
                        let mut packets = Vec::new();
                        send_frame(&mut send, &stereo, &mut packets);
                        for (seq, ts, pkt) in packets {
                            let dg = parley_proto::media::encode(flags, seq, ts, 0, &pkt);
                            if conn_send.send_datagram(dg.into()).is_err() {
                                return; // connection gone
                            }
                        }
                    }
                }
            } else {
                // --- speech profile (unchanged since 4d) ---
                let mut send = match SendPipeline::new(64_000) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("opus encoder init failed: {e}");
                        return;
                    }
                };

                // 4d: two-profile bitrate controller.
                // Clean:  64 kbps, loss hint 5  (CELT, max quality, FEC inert).
                // Robust: 24 kbps, loss hint 30 (SILK + LBRR, FEC live).
                // EMA over receiver gap-rate reports (updated every 5 s);
                // down when EMA > 3%, back up after 20 s below 1%. 5 s
                // switch cooldown prevents flapping.
                let mut ema_permille: f32 = 0.0;
                let mut mode_robust = false;
                let mut last_switch = std::time::Instant::now();
                let mut low_since: Option<std::time::Instant> = None;
                let mut ctrl_tick = tokio::time::interval(Duration::from_secs(1));

                fn apply_profile(send: &mut SendPipeline, robust: bool) {
                    let (rate, loss) = if robust { (24_000, 30) } else { (64_000, 5) };
                    send.set_bitrate(rate);
                    send.set_loss_hint(loss);
                }
                apply_profile(&mut send, false);

                // Reassembles arbitrary callback chunks into exact 10 ms
                // frames for the AEC/NS processor.
                let mut asm = aec::FrameAssembler::new();
                loop {
                    tokio::select! {
                        chunk = pcm_rx.recv() => {
                            let Some(chunk) = chunk else { break };
                            let processed: Vec<f32> = match aec_proc.as_ref() {
                                Some(ap) => {
                                    let mut out = Vec::new();
                                    aec::process_capture(ap, &mut asm, &chunk, &mut out);
                                    out
                                }
                                None => chunk,
                            };
                            let pcm: Vec<i16> = processed
                                .iter()
                                .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                                .collect();
                            let mut packets = Vec::new();
                            if !pcm.is_empty()
                                && let Err(e) = send.push_pcm(&pcm, &mut packets)
                            {
                                eprintln!("encode failed: {e}");
                                break;
                            }
                            for (seq, ts, pkt) in packets {
                                let dg = parley_proto::media::encode(
                                    parley_proto::media::FLAG_SPEAKING,
                                    seq,
                                    ts,
                                    0, // sender_id: the server stamps the authenticated id
                                    &pkt,
                                );
                                if conn_send.send_datagram(dg.into()).is_err() {
                                    return; // connection gone
                                }
                            }
                        }
                        _ = ctrl_tick.tick() => {
                            let observed = enc_loss_state.load(Ordering::Relaxed) as f32;
                            ema_permille = 0.7 * ema_permille + 0.3 * observed;
                            let now = std::time::Instant::now();
                            if !mode_robust {
                                if ema_permille > 30.0
                                    && now.duration_since(last_switch).as_secs() >= 5
                                {
                                    mode_robust = true;
                                    apply_profile(&mut send, true);
                                    last_switch = now;
                                    low_since = None;
                                    println!(
                                        "bitrate: → Robust (24 kbps + FEC) — loss {:.1}%",
                                        ema_permille / 10.0
                                    );
                                }
                            } else if ema_permille < 10.0 {
                                let since = *low_since.get_or_insert(now);
                                if now.duration_since(since).as_secs() >= 20
                                    && now.duration_since(last_switch).as_secs() >= 5
                                {
                                    mode_robust = false;
                                    apply_profile(&mut send, false);
                                    last_switch = now;
                                    low_since = None;
                                    println!(
                                        "bitrate: → Clean (64 kbps) — loss {:.1}%",
                                        ema_permille / 10.0
                                    );
                                }
                            } else {
                                low_since = None;
                            }
                        }
                    }
                }
            }
        });

        // --- 4d: loss reporter — one aggregate (worst-peer) report per
        // 5 s window, routed by the server back to voice-channel peers.
        // Speech channels only: music has no ladder to steer. ---
        if !music_mode {
            let rep_mixer = shared_mixer.clone();
            let rep_outbound = _outbound_tx.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tick.tick().await;
                    let rates = mixer::gap_rates(&rep_mixer);
                    if rates.is_empty() {
                        continue; // no active voices: nothing to report
                    }
                    let worst = rates.iter().map(|&(_, r)| r).max().unwrap_or(0);
                    let _ = rep_outbound.send(ClientMessage {
                        body: Some(ClientBody::ReceiverReport(ReceiverReport {
                            gap_rate_permille: worst,
                            reporter_id: 0, // server stamps the authenticated id
                        })),
                    });
                }
            });
        }
    }

    #[cfg(not(feature = "audio-device"))]
    println!("audio-device feature off: no mic or speakers");

    // --- media datagram reader ---
    // Hoisted out of the audio-device gate: a video-only build receives
    // video. Control-only builds (no features) still never spawn it.
    #[cfg(any(feature = "audio-device", feature = "video"))]
    {
        #[cfg(feature = "audio-device")]
        let dg_mixer = shared_mixer.clone();
        let my_id = ok.user_id;
        let conn_recv = conn.clone();
        tokio::spawn(async move {
            loop {
                match conn_recv.read_datagram().await {
                    Ok(dg) => {
                        let Ok(m) = parley_proto::media::decode(&dg) else {
                            continue;
                        };
                        #[cfg(feature = "video")]
                        if m.flags & parley_proto::media::FLAG_VIDEO != 0 {
                            // server never loops a sender's own video back
                            if m.sender_id != 0 && m.sender_id != my_id {
                                video::handle_video_datagram(
                                    &video_viewers,
                                    m.sender_id,
                                    m.payload,
                                );
                            }
                            continue;
                        }
                        #[cfg(feature = "audio-device")]
                        {
                            // server never loops a sender's own audio back
                            if m.sender_id == 0 || m.sender_id == my_id {
                                continue;
                            }
                            // 6d: FLAG_STEREO per packet — the mixer keys
                            // the decoder off it.
                            let stereo = m.flags & parley_proto::media::FLAG_STEREO != 0;
                            if let Err(e) =
                                mixer::push_packet(&dg_mixer, m.sender_id, m.seq, m.payload, stereo)
                            {
                                eprintln!("recv pipeline for user {}: {e}", m.sender_id);
                            }
                        }
                    }
                    Err(_) => {
                        println!("media channel closed");
                        break;
                    }
                }
            }
        });
    }

    // --- share lifecycle (Phase 6a) ---
    // One command task owns the share state machine. Sources: stdin
    // commands (a plain OS thread — tokio io-std isn't in the feature
    // set) and ShareEvents from the capture supervisor / synthetic
    // sender. On Ended the task sends StopShare — only after the
    // supervisor confirmed full teardown, so no video datagram can
    // follow the stop notice.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let (_events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<ShareEvent>();

    // stdin reader thread. On EOF (Ctrl-D) the sender drops and the
    // command task stops reading commands — but keeps share state
    // alive until the connection ends (a share must not die with the
    // terminal's stdin).
    {
        let cmd_tx = cmd_tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                let cmd = match line.trim() {
                    "share" => Command::Share,
                    "stop" => Command::Stop,
                    "" => continue,
                    other => {
                        println!("unknown command: {other} (share | stop)");
                        continue;
                    }
                };
                if cmd_tx.send(cmd).is_err() {
                    break;
                }
            }
        });
    }

    // Share state, per feature (cfg the variables, not the reads, to
    // keep every feature combination warning-free).
    #[cfg(feature = "screen-capture")]
    let share_ctrl: Arc<Mutex<Option<screen::ShareController>>> = Arc::new(Mutex::new(None));
    #[cfg(feature = "screen-capture")]
    let share_poke = keyframe_poke.clone();
    #[cfg(any(feature = "video", feature = "screen-capture"))]
    let share_conn = conn.clone();
    #[cfg(any(feature = "video", feature = "screen-capture"))]
    let share_events = _events_tx.clone();
    #[cfg(any(feature = "video", feature = "screen-capture"))]
    let share_vloss = video_loss.clone();
    #[cfg(feature = "screen-capture")]
    let mut share_starting = false;
    #[cfg(all(feature = "video", not(feature = "screen-capture")))]
    let mut synth_share: Option<tokio::task::AbortHandle> = None;
    let share_outbound = _outbound_tx.clone();

    let mut cmd_open = true;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                c = cmd_rx.recv(), if cmd_open => {
                    match c {
                        Some(Command::Share) => {
                            #[cfg(feature = "screen-capture")]
                            {
                                if share_starting || share_ctrl.lock().unwrap().is_some() {
                                    println!("share: already active or starting");
                                } else {
                                    share_starting = true;
                                    println!("share: requesting screen capture — pick a monitor or window in the portal dialog");
                                    // Starter task: keeps the command loop
                                    // free while the portal dialog is open.
                                    let conn = share_conn.clone();
                                    let poke = share_poke.clone();
                                    let ctrl_slot = share_ctrl.clone();
                                    let events = share_events.clone();
                                    let vloss = share_vloss.clone();
                                    tokio::spawn(async move {
                                        match screen::start(conn, vloss, events.clone()).await {
                                            Ok((handle, ctrl)) => {
                                                *poke.lock().unwrap() = Some(handle);
                                                *ctrl_slot.lock().unwrap() = Some(ctrl);
                                                let _ = events.send(ShareEvent::Started);
                                            }
                                            Err(e) => {
                                                let _ = events.send(ShareEvent::StartFailed(e));
                                            }
                                        }
                                    });
                                }
                            }
                            #[cfg(all(feature = "video", not(feature = "screen-capture")))]
                            {
                                if synth_share.is_some() {
                                    println!("share: already active");
                                } else {
                                    match video::ShareSender::new() {
                                        Ok(sender) => {
                                            let task = tokio::spawn(video::share_task(
                                                share_conn.clone(),
                                                sender,
                                                share_vloss.clone(),
                                            ));
                                            synth_share = Some(task.abort_handle());
                                            let _ = share_events.send(ShareEvent::Started);
                                        }
                                        Err(e) => {
                                            let _ = share_events.send(ShareEvent::StartFailed(e));
                                        }
                                    }
                                }
                            }
                            #[cfg(not(any(feature = "video", feature = "screen-capture")))]
                            eprintln!("share: not built with video or screen-capture");
                        }
                        Some(Command::Stop) => {
                            #[cfg(feature = "screen-capture")]
                            {
                                if let Some(c) = share_ctrl.lock().unwrap().take() {
                                    c.request_stop();
                                    // ShareEvent::Ended follows after full teardown
                                } else if share_starting {
                                    println!("share: still starting — finish or cancel the portal dialog first");
                                } else {
                                    println!("share: not active");
                                }
                            }
                            #[cfg(all(feature = "video", not(feature = "screen-capture")))]
                            {
                                if let Some(abort) = synth_share.take() {
                                    abort.abort();
                                    let _ = share_events.send(ShareEvent::Ended { external: false });
                                } else {
                                    println!("share: not active");
                                }
                            }
                            #[cfg(not(any(feature = "video", feature = "screen-capture")))]
                            println!("share: not active");
                        }
                        None => cmd_open = false,
                    }
                }
                e = events_rx.recv() => {
                    match e {
                        Some(ev) => match ev {
                            ShareEvent::Started => {
                                // Readiness is logged by the subsystems
                                // (pw "stream connected" / share_task pattern
                                // line); only clear the starting flag here.
                                #[cfg(feature = "screen-capture")]
                                {
                                    share_starting = false;
                                }
                            }
                            ShareEvent::StartFailed(e) => {
                                #[cfg(feature = "screen-capture")]
                                {
                                    share_starting = false;
                                }
                                eprintln!("share: {e}");
                            }
                            ShareEvent::Ended { external } => {
                                #[cfg(feature = "screen-capture")]
                                {
                                    *share_ctrl.lock().unwrap() = None;
                                    *share_poke.lock().unwrap() = None;
                                    share_starting = false;
                                }
                                #[cfg(all(feature = "video", not(feature = "screen-capture")))]
                                {
                                    synth_share = None;
                                }
                                // Only now — after the supervisor joined the
                                // threads — can no more video datagrams exist.
                                let _ = share_outbound.send(ClientMessage {
                                    body: Some(ClientBody::StopShare(StopShare {})),
                                });
                                println!(
                                    "share: stopped{}",
                                    if external { " (closed from the desktop)" } else { "" }
                                );
                            }
                        },
                        None => {
                            // _events_tx is held by main for the whole
                            // session; None means the client is shutting down.
                            std::future::pending::<()>().await;
                        }
                    }
                }
            }
        }
    });

    // --share at boot is just a "share" command.
    if args.share {
        let _ = cmd_tx.send(Command::Share);
    }

    println!("talking — Ctrl-C to quit");
    #[cfg(any(feature = "video", feature = "screen-capture"))]
    println!("commands: \"share\" to start sharing, \"stop\" to stop it");
    let why = conn.closed().await;
    println!("connection closed: {why}");
    Ok(())
}
