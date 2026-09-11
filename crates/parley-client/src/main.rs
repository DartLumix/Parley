//! parley-test-client: connect, auth, join a voice channel, pump audio.
//! Without the `audio-device` feature it's a control-only debug client
//! (chat/presence, no mic or speakers) — that's what CI builds.

use std::net::SocketAddr;
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
}

fn parse_args() -> Result<Args, String> {
    let mut server = None;
    let mut user = None;
    let mut password = None;
    let mut channel_id = 1u64;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server" => server = it.next(),
            "--user" => user = it.next(),
            "--password" => password = it.next(),
            "--channel" => {
                channel_id = it.next().and_then(|v| v.parse().ok()).unwrap_or(channel_id)
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

    // --- audio (feature-gated) ---
    #[cfg(feature = "audio-device")]
    let voices = mixer::new_voices();
    #[cfg(feature = "audio-device")]
    let runtime = audio::start(voices.clone())?;

    // --- heartbeat writer owns the control stream from here on ---
    let hb_ms = if ack.heartbeat_ms == 0 {
        5000
    } else {
        ack.heartbeat_ms
    };
    tokio::spawn(async move {
        let mut tx = tx;
        let mut tick = tokio::time::interval(Duration::from_millis(hb_ms as u64));
        loop {
            tick.tick().await;
            let msg = ClientMessage {
                body: Some(ClientBody::Heartbeat(Heartbeat {
                    client_time_ms: now_ms(),
                })),
            };
            if session::write_message(&mut tx, &msg).await.is_err() {
                break;
            }
        }
    });

    // --- control stream reader: chat / voice states / presence ---
    #[cfg(feature = "audio-device")]
    let ctrl_voices = voices.clone();
    tokio::spawn(async move {
        loop {
            match session::read_message::<ServerMessage>(&mut rx).await {
                Ok(msg) => match msg.body {
                    Some(ServerBody::ChatMessage(m)) => println!("[{}] {}", m.username, m.text),
                    Some(ServerBody::VoiceState(v)) => {
                        println!(
                            "user {} {} voice channel {}",
                            v.user_id,
                            if v.joined { "joined" } else { "left" },
                            v.channel_id
                        );
                        #[cfg(feature = "audio-device")]
                        if !v.joined {
                            mixer::remove(&ctrl_voices, v.user_id);
                        }
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
                    // sender_id 0: the server stamps the authenticated id
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

        let dg_voices = voices.clone();
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
                        let mut voices = dg_voices.lock().unwrap();
                        let v = voices.entry(m.sender_id).or_insert_with(|| {
                            mixer::Voice::new(m.sender_id).expect("opus decoder")
                        });
                        v.push_packet(m.seq, m.payload);
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
