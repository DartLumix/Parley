//! parley-test-client: connect, auth, join a voice channel, pump audio.
//! Without the `audio-device` feature it's a control-only debug client
//! (chat/presence, no mic or speakers) — that's what CI builds.
//!
//! Latency measurement (Phase 3f), two roles on one machine:
//!   A: --measure   injects a click every 2 s, pairs returning reports,
//!                  prints callback→render latency + stats
//!   B: --report    detects clicks in the render callback and reports
//!                  them back over chat (control-stream reuse — no
//!                  protocol changes). Both processes share the machine
//!                  clock, so the deltas are exact.
//! Stay quiet while measuring; the clicks are loud.
//!
//! Per-user volume (Phase 4b): --volume <user_id>:<gain>, repeatable.
//! 0.0 mutes that user locally; other peers are unaffected.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parley_core::session;
use parley_proto::{
    AuthDenied, AuthOk, AuthRequest, ClientBody, ClientMessage, Heartbeat, Hello, HelloAck,
    JoinVoice, PROTOCOL_VERSION, ServerBody, ServerMessage,
};
use prost::Message as _;

#[cfg(feature = "audio-device")]
mod audio;
mod cert;
#[cfg(feature = "audio-device")]
mod mixer;

struct Args {
    server: String,
    user: String,
    password: String,
    channel_id: u64,
    measure: bool,
    report: bool,
    volumes: Vec<(u32, f32)>,
}

fn parse_args() -> Result<Args, String> {
    let mut server = None;
    let mut user = None;
    let mut password = None;
    let mut channel_id = 1u64;
    let (mut measure, mut report) = (false, false);
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

    // Outbound control channel: heartbeats AND click reports share one
    // writer task (audio callbacks can't await, but unbounded send is
    // synchronous and non-blocking).
    let (_outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

    #[cfg(not(feature = "audio-device"))]
    if args.measure || args.report || !args.volumes.is_empty() {
        eprintln!("--measure/--report/--volume need the audio-device feature; ignoring");
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

    // --- control stream reader: chat / voice states / latency reports ---
    #[cfg(feature = "audio-device")]
    let ctrl_mixer = shared_mixer.clone();
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

    // --- media: mic → SendPipeline → datagrams; datagrams → pipelines ---
    #[cfg(feature = "audio-device")]
    {
        let mut pcm_rx = runtime.pcm_rx;
        let conn_send = conn.clone();
        tokio::spawn(async move {
            let mut send = match parley_core::audio::pipeline::SendPipeline::new(64_000) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("opus encoder init failed: {e}");
                    return;
                }
            };
            while let Some(chunk) = pcm_rx.recv().await {
                let mut packets = Vec::new();
                if let Err(e) = send.push_pcm(&chunk, &mut packets) {
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
    }

    #[cfg(not(feature = "audio-device"))]
    println!("audio-device feature off: control-only mode (no mic/speakers)");

    println!("talking — Ctrl-C to quit");
    let why = conn.closed().await;
    println!("connection closed: {why}");
    Ok(())
}
