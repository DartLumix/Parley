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

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parley_core::session;
use parley_proto::{
    AuthDenied, AuthOk, AuthRequest, ClientBody, ClientMessage, Heartbeat, Hello, HelloAck,
    JoinVoice, PROTOCOL_VERSION, ServerBody, ServerMessage,
};
use prost::Message as _;

use parley_client::cert;
#[cfg(feature = "audio-device")]
use parley_client::{aec, audio, mixer};
#[cfg(feature = "audio-device")]
use parley_core::audio::pipeline::SendPipeline;
#[cfg(feature = "audio-device")]
use parley_proto::ReceiverReport;

struct Args {
    server: String,
    user: String,
    password: String,
    channel_id: u64,
    measure: bool,
    report: bool,
    noaec: bool,
    volumes: Vec<(u32, f32)>,
}

fn parse_args() -> Result<Args, String> {
    let mut server = None;
    let mut user = None;
    let mut password = None;
    let mut channel_id = 1u64;
    let (mut measure, mut report, mut noaec) = (false, false, false);
    let mut volumes = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server" => server = it.next(),
            "--user" => user = it.next(),
            "--password" => password = it.next(),
            "--channel" => {
                channel_id = it.next().and_then(|v| v.parse().ok()).unwrap_or(channel_id)
            }
            "--measure" => measure = true,
            "--report" => report = true,
            "--no-aec" => noaec = true,
            "--volume" => {
                let spec = it.next().ok_or("--volume needs <user_id>:<gain>")?;
                let (id, gain) = spec
                    .split_once(':')
                    .ok_or("--volume format is <user_id>:<gain>, e.g. 2:0.5")?;
                let id: u32 = id.parse().map_err(|_| format!("bad user id: {id}"))?;
                let gain: f32 = gain.parse().map_err(|_| format!("bad gain: {gain}"))?;
                volumes.push((id, gain.clamp(0.0, 4.0)));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let user = user
        .or_else(|| std::env::var("PARLEY_USER").ok())
        .ok_or("missing --user (or PARLEY_USER env)")?;
    let password = password
        .or_else(|| std::env::var("PARLEY_PASSWORD").ok())
        .ok_or("missing --password (or PARLEY_PASSWORD env)")?;
    Ok(Args {
        server: server.unwrap_or_else(|| "127.0.0.1:4443".to_string()),
        user,
        password,
        channel_id,
        measure,
        report,
        noaec,
        volumes,
    })
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
            session_token: String::new(),
        },
    )
    .await?;
    let payload = read_frame(&mut rx).await?;
    let ok = if let Ok(ok) = AuthOk::decode(payload.as_slice()) {
        ok
    } else if let Ok(denied) = AuthDenied::decode(payload.as_slice()) {
        return Err(format!("auth denied: {}", denied.reason).into());
    } else {
        return Err("unexpected message during auth".into());
    };

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
        "connected to {} as \"{}\" (id {}), voice channel {}",
        args.server, args.user, ok.user_id, args.channel_id
    );

    // Outbound control channel: heartbeats, click reports and loss
    // reports share one writer task.
    let (_outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

    // 4d: last loss report seen from a peer, permille. Written by the
    // control reader, polled by the bitrate controller in the encode task.
    let loss_state = Arc::new(AtomicU32::new(0));

    #[cfg(not(feature = "audio-device"))]
    if args.measure || args.report || args.noaec || !args.volumes.is_empty() {
        eprintln!("--measure/--report/--volume/--no-aec need the audio-device feature; ignoring");
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
    let aec_proc = if args.noaec {
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
    let ctrl_loss_state = loss_state.clone();
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
                        #[cfg(feature = "audio-device")]
                        if !v.joined {
                            mixer::remove(&ctrl_mixer, v.user_id);
                        }
                    }
                    Some(ServerBody::ReceiverReport(rep)) => {
                        // 4d: a peer's loss report steers our send profile.
                        // Skip our own echo (broadcast includes us) and
                        // unstamped reports.
                        if rep.reporter_id != 0 && rep.reporter_id != ok.user_id {
                            ctrl_loss_state.store(rep.gap_rate_permille, Ordering::Relaxed);
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
        tokio::spawn(async move {
            let mut send = match SendPipeline::new(64_000) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("opus encoder init failed: {e}");
                    return;
                }
            };

            // --- 4d: two-profile bitrate controller ---
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
        });

        let dg_mixer = shared_mixer.clone();
        let conn_recv = conn.clone();
        let my_id = ok.user_id;
        tokio::spawn(async move {
            loop {
                match conn_recv.read_datagram().await {
                    Ok(dg) => {
                        let Ok(m) = parley_proto::media::decode(&dg) else {
                            continue;
                        };
                        // server never loops a sender's own audio back
                        if m.sender_id == 0 || m.sender_id == my_id {
                            continue;
                        }
                        if let Err(e) = mixer::push_packet(&dg_mixer, m.sender_id, m.seq, m.payload)
                        {
                            eprintln!("recv pipeline for user {}: {e}", m.sender_id);
                        }
                    }
                    Err(_) => {
                        println!("media channel closed");
                        break;
                    }
                }
            }
        });

        // --- 4d: loss reporter — one aggregate (worst-peer) report per
        // 5 s window, routed by the server back to voice-channel peers ---
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

    #[cfg(not(feature = "audio-device"))]
    println!("audio-device feature off: control-only mode (no mic/speakers)");

    println!("talking — Ctrl-C to quit");
    let why = conn.closed().await;
    println!("connection closed: {why}");
    Ok(())
}
