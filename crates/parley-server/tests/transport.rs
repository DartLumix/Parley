//! A real QUIC client talks to a real QUIC server over loopback UDP.
//! This is the Phase 1a milestone: transport + TLS + a stream exchange.

use std::sync::Arc;

use parley_server::{Server, tls};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

use parley_server::Config;
use std::path::PathBuf;

use parley_core::session;
use parley_proto::{Hello, HelloAck, PROTOCOL_VERSION};

use parley_proto::{AuthDenied, AuthOk, AuthRequest};

use parley_proto::{
    ClientBody, ClientMessage, Heartbeat, PresenceRequest, ReceiverReport, ServerBody,
    ServerMessage, StopShare, VideoReport,
};

use parley_proto::{FetchHistory, ListChannels, SendChat};

use parley_proto::{JoinVoice, LeaveVoice, VoiceStatesRequest};
use std::time::Duration;
use tokio::time::timeout;

use parley_proto::frame;

use parley_core::audio::jitter::PopOutcome;

async fn send_client_msg(tx: &mut quinn::SendStream, body: parley_proto::ClientBody) {
    session::write_message(tx, &parley_proto::ClientMessage { body: Some(body) })
        .await
        .unwrap();
}

/// Read the next server message, skipping ambient presence pushes
/// (the server broadcasts PresenceList on every session start/end —
/// 7.x presence-push). Pushes are lifecycle noise like HeartbeatAcks;
/// the one test that inspects presence reads raw.
async fn read_server_msg(rx: &mut quinn::RecvStream) -> parley_proto::ServerMessage {
    loop {
        let msg: ServerMessage = session::read_message(rx).await.unwrap();
        match msg.body {
            Some(ServerBody::PresenceList(_)) => continue,
            _ => return msg,
        }
    }
}

/// Assert that nothing but ambient presence pushes arrives within
/// `dur`. Used where tests previously wrapped a bare read in
/// `timeout(...)` + `assert!(is_err())` — the presence-push broadcast
/// made that pattern false-positive on session churn mid-test.
async fn expect_silence(rx: &mut quinn::RecvStream, dur: Duration) {
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, session::read_message::<ServerMessage>(rx)).await {
            Err(_) => return, // deadline hit: silence confirmed
            Ok(Err(e)) => panic!("stream error during silence check: {e}"),
            Ok(Ok(msg)) => match msg.body {
                Some(ServerBody::PresenceList(_)) => continue, // ambient
                other => panic!("expected silence, got {other:?}"),
            },
        }
    }
}

async fn expect_voice_state(
    rx: &mut quinn::RecvStream,
    user_id: u32,
    channel_id: u64,
    joined: bool,
) {
    let msg = read_server_msg(rx).await;
    match msg.body {
        Some(ServerBody::VoiceState(v)) => {
            assert_eq!(v.user_id, user_id);
            assert_eq!(v.channel_id, channel_id);
            assert_eq!(v.joined, joined);
        }
        other => panic!("expected voice state, got {other:?}"),
    }
}

fn sine_frames(n_frames: usize) -> Vec<Vec<i16>> {
    (0..n_frames)
        .map(|f| {
            (0..parley_core::audio::SAMPLES_PER_FRAME)
                .map(|i| {
                    let idx = f * parley_core::audio::SAMPLES_PER_FRAME + i;
                    let t = idx as f64 / parley_core::audio::SAMPLE_RATE as f64;
                    (0.3 * f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * i16::MAX as f64)
                        as i16
                })
                .collect()
        })
        .collect()
}

fn best_correlation(a: &[i16], b: &[i16]) -> f64 {
    let n = a.len().min(b.len());
    let mut best = f64::NEG_INFINITY;
    for lag in 0..600 {
        let (mut dot, mut sa, mut sb) = (0.0, 0.0, 0.0);
        for i in 0..(n - lag) {
            let x = a[i] as f64;
            let y = b[i + lag] as f64;
            dot += x * y;
            sa += x * x;
            sb += y * y;
        }
        let denom = (sa * sb).sqrt();
        if denom > 0.0 {
            best = best.max(dot / denom);
        }
    }
    best
}

#[tokio::test]
async fn chat_broadcast_and_history() {
    init_logs();

    let (config, server) = setup_server_with_user("chat");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);

    // Two sessions: sender and listener
    let (conn_a, mut tx_a, mut rx_a) =
        connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    let (conn_b, _tx_b, mut rx_b) =
        connect_and_auth(&endpoint, addr, "seconduser", "secondpass").await;

    // Both get the broadcast; skip any non-chat messages (none expected yet)
    send_client_msg(
        &mut tx_a,
        parley_proto::ClientBody::SendChat(SendChat {
            channel_id: 1,
            text: "hello world".to_string(),
        }),
    )
    .await;

    // alice reads broadcast
    let msg = read_server_msg(&mut rx_a).await;
    match msg.body {
        Some(parley_proto::ServerBody::ChatMessage(m)) => {
            assert_eq!(m.text, "hello world");
            assert_eq!(m.channel_id, 1);
            assert!(m.message_id > 0);
        }
        _ => panic!("expected chat message"),
    }
    // bob reads broadcast
    let msg = read_server_msg(&mut rx_b).await;
    match msg.body {
        Some(parley_proto::ServerBody::ChatMessage(m)) => {
            assert_eq!(m.text, "hello world");
        }
        _ => panic!("expected chat message"),
    }

    // History: sender fetches
    send_client_msg(
        &mut tx_a,
        parley_proto::ClientBody::FetchHistory(FetchHistory {
            channel_id: 1,
            limit: 50,
            before_message_id: 0,
        }),
    )
    .await;
    let msg = read_server_msg(&mut rx_a).await;
    match msg.body {
        Some(parley_proto::ServerBody::HistoryResult(h)) => {
            assert_eq!(h.messages.len(), 1);
            assert_eq!(h.messages[0].text, "hello world");
            assert_eq!(h.messages[0].username, "testuser");
        }
        _ => panic!("expected history result"),
    }

    conn_a.close(0u32.into(), b"done");
    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn channel_list_contains_general() {
    let (config, server) = setup_server_with_user("channels");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let (conn, mut tx, mut rx) = connect_and_auth(&endpoint, addr, "testuser", "testpass").await;

    send_client_msg(
        &mut tx,
        parley_proto::ClientBody::ListChannels(ListChannels {}),
    )
    .await;
    let msg = read_server_msg(&mut rx).await;
    match msg.body {
        Some(parley_proto::ServerBody::ChannelList(list)) => {
            assert!(
                list.channels
                    .iter()
                    .any(|c| c.name == "general" && c.id == 1)
            );
        }
        _ => panic!("expected channel list"),
    }

    conn.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

fn test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("parley-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn setup_server_with_user(tag: &str) -> (Config, Server) {
    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        data_dir: test_dir(tag),
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let db = parley_server::db::Db::open(&config.data_dir.join("parley.db")).unwrap();
    db.create_user("testuser", "testpass", false).unwrap();
    db.create_user("seconduser", "secondpass", false).unwrap();
    db.create_user("thirduser", "thirdpass", false).unwrap();
    drop(db);

    let server = Server::bind(&config).unwrap();
    (config, server)
}

/// Add a channel while the server is running (WAL allows the brief
/// concurrent writer). Returns its id.
fn add_channel(config: &Config, name: &str, music: bool) -> u64 {
    let db = parley_server::db::Db::open(&config.data_dir.join("parley.db")).unwrap();
    let id = db.create_channel(name, music).unwrap();
    drop(db);
    id
}

async fn connect_and_auth_full(
    endpoint: &quinn::Endpoint,
    addr: std::net::SocketAddr,
    username: &str,
    password: &str,
) -> (
    quinn::Connection,
    quinn::SendStream,
    quinn::RecvStream,
    AuthOk,
) {
    let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut tx, mut rx) = conn.open_bi().await.unwrap();

    session::write_message(
        &mut tx,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await
    .unwrap();
    let _ack: HelloAck = session::read_message(&mut rx).await.unwrap();

    session::write_message(
        &mut tx,
        &AuthRequest {
            username: username.to_string(),
            password: password.to_string(),
            session_token: String::new(),
        },
    )
    .await
    .unwrap();

    let ok: AuthOk = session::read_message(&mut rx).await.unwrap();
    assert!(
        !ok.session_token.is_empty(),
        "auth must return a session token"
    );

    (conn, tx, rx, ok)
}

async fn connect_and_auth(
    endpoint: &quinn::Endpoint,
    addr: std::net::SocketAddr,
    username: &str,
    password: &str,
) -> (quinn::Connection, quinn::SendStream, quinn::RecvStream) {
    let (conn, tx, rx, _ok) = connect_and_auth_full(endpoint, addr, username, password).await;
    (conn, tx, rx)
}

#[tokio::test]
async fn heartbeat_roundtrip() {
    init_logs();

    let (config, server) = setup_server_with_user("heartbeat");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let (conn, mut tx, mut rx) = connect_and_auth(&endpoint, addr, "testuser", "testpass").await;

    let sent_at = 12345u64;
    session::write_message(
        &mut tx,
        &ClientMessage {
            body: Some(ClientBody::Heartbeat(Heartbeat {
                client_time_ms: sent_at,
            })),
        },
    )
    .await
    .unwrap();

    let response: ServerMessage = read_server_msg(&mut rx).await;
    match response.body {
        Some(ServerBody::HeartbeatAck(ack)) => {
            assert_eq!(ack.client_time_ms, sent_at);
            assert!(ack.server_time_ms > 0);
        }
        _ => panic!("expected heartbeat ack"),
    }

    conn.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn presence_shows_connected_users() {
    init_logs();

    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        data_dir: test_dir("presence"),
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let db = parley_server::db::Db::open(&config.data_dir.join("parley.db")).unwrap();
    db.create_user("alice", "pass_a", false).unwrap();
    db.create_user("bob", "pass_b", false).unwrap();
    drop(db);

    let server = Server::bind(&config).unwrap();
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);

    let (conn_a, mut tx_a, mut rx_a) = connect_and_auth(&endpoint, addr, "alice", "pass_a").await;
    let (conn_b, _tx_b, _rx_b) = connect_and_auth(&endpoint, addr, "bob", "pass_b").await;

    // Alice requests presence. The pull response and the join-pushes
    // are indistinguishable by type, so read raw PresenceLists until
    // one carries the full online set — pushes converge to it and the
    // pull confirms it.
    session::write_message(
        &mut tx_a,
        &ClientMessage {
            body: Some(ClientBody::PresenceRequest(PresenceRequest {})),
        },
    )
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "presence list with both users never arrived"
        );
        let response: ServerMessage =
            tokio::time::timeout(remaining, session::read_message(&mut rx_a))
                .await
                .expect("presence message should arrive")
                .unwrap();
        match response.body {
            Some(ServerBody::PresenceList(list)) => {
                let names: Vec<&str> = list.users.iter().map(|u| u.username.as_str()).collect();
                if names.contains(&"alice") && names.contains(&"bob") {
                    assert_eq!(list.users.len(), 2, "both alice and bob should be online");
                    break;
                }
                // an earlier push (pre-bob): keep reading
            }
            other => panic!("expected presence list, got {other:?}"),
        }
    }

    conn_a.close(0u32.into(), b"done");
    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn auth_success() {
    init_logs();

    let (config, server) = setup_server_with_user("auth-ok");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();

    let (mut tx, mut rx) = conn.open_bi().await.unwrap();

    // Handshake
    session::write_message(
        &mut tx,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await
    .unwrap();

    let ack: HelloAck = session::read_message(&mut rx).await.unwrap();
    assert_eq!(ack.protocol_version, PROTOCOL_VERSION);

    // Auth
    session::write_message(
        &mut tx,
        &AuthRequest {
            username: "testuser".to_string(),
            password: "testpass".to_string(),
            session_token: String::new(),
        },
    )
    .await
    .unwrap();

    let ok: AuthOk = session::read_message(&mut rx).await.unwrap();
    assert!(!ok.is_admin);
    assert!(ok.user_id > 0);
    assert!(!ok.session_token.is_empty());

    conn.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn auth_failure_wrong_password() {
    let (config, server) = setup_server_with_user("auth-denied");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();

    let (mut tx, mut rx) = conn.open_bi().await.unwrap();

    // Handshake
    session::write_message(
        &mut tx,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await
    .unwrap();
    let _ack: HelloAck = session::read_message(&mut rx).await.unwrap();

    // Auth with wrong password
    session::write_message(
        &mut tx,
        &AuthRequest {
            username: "testuser".to_string(),
            password: "wrongpass".to_string(),
            session_token: String::new(),
        },
    )
    .await
    .unwrap();

    let denied: AuthDenied = session::read_message(&mut rx).await.unwrap();
    assert!(!denied.reason.is_empty());

    conn.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn fingerprint_survives_restart() {
    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        data_dir: test_dir("restart"),
    };

    let fp1 = Server::bind(&config).unwrap().fingerprint().to_string();
    let fp2 = Server::bind(&config).unwrap().fingerprint().to_string();

    assert_eq!(fp1, fp2, "fingerprint must be stable across restarts");

    let _ = std::fs::remove_dir_all(&config.data_dir);
}

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .try_init();
}

/// A client endpoint that verifies the server by pinned fingerprint.
fn client_endpoint(pinned_fingerprint: String) -> quinn::Endpoint {
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedFingerprint(pinned_fingerprint)))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![tls::alpn()];

    let config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("valid client config"),
    ));

    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(config);
    endpoint
}

/// Certificate verification = fingerprint check. The test knows the
/// server's fingerprint (same process), so this is real verification,
/// not "accept anything". 1b moves the pin into the client itself
/// (pin on first connect, warn on change).
#[derive(Debug)]
struct PinnedFingerprint(String);

impl ServerCertVerifier for PinnedFingerprint {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let seen = tls::fingerprint(end_entity);
        if seen == self.0 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(format!(
                "fingerprint mismatch: pinned {}, saw {seen}",
                self.0
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[tokio::test]
async fn voice_datagram_routed_between_members() {
    init_logs();

    let (config, server) = setup_server_with_user("voice-route");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    // testuser = id 1, seconduser = id 2 (creation order)
    let (conn_a, mut tx_a, mut rx_a) =
        connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    let (conn_b, mut tx_b, mut rx_b) =
        connect_and_auth(&endpoint, addr, "seconduser", "secondpass").await;

    // A joins voice channel 1
    send_client_msg(
        &mut tx_a,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 1, 1, true).await;
    expect_voice_state(&mut rx_b, 1, 1, true).await;

    // B requests the snapshot: A is in channel 1
    send_client_msg(
        &mut tx_b,
        ClientBody::VoiceStatesRequest(VoiceStatesRequest {}),
    )
    .await;
    let msg = read_server_msg(&mut rx_b).await;
    match msg.body {
        Some(ServerBody::VoiceStateList(list)) => {
            assert_eq!(list.states.len(), 1);
            assert_eq!(list.states[0].user_id, 1);
            assert_eq!(list.states[0].channel_id, 1);
        }
        other => panic!("expected voice state list, got {other:?}"),
    }

    // B joins voice channel 1
    send_client_msg(
        &mut tx_b,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 2, 1, true).await;
    expect_voice_state(&mut rx_b, 2, 1, true).await;

    // A sends a media datagram (synthetic payload, sender_id 0 — server stamps)
    let payload: Vec<u8> = (0..160u32).map(|i| (i % 251) as u8).collect();
    let dg = parley_proto::media::encode(0, 42, 48_000, 0, &payload);
    conn_a.send_datagram(dg.into()).unwrap();

    // B receives it, stamped with A's authenticated user id (1)
    let received = timeout(Duration::from_secs(2), conn_b.read_datagram())
        .await
        .expect("datagram should arrive")
        .unwrap();
    let decoded = parley_proto::media::decode(&received).unwrap();
    assert_eq!(decoded.sender_id, 1);
    assert_eq!(decoded.seq, 42);
    assert_eq!(decoded.timestamp, 48_000);
    assert_eq!(decoded.payload, &payload[..]);

    // A must NOT receive its own datagram back (no loopback in the SFU)
    let loopback = timeout(Duration::from_millis(200), conn_a.read_datagram()).await;
    assert!(loopback.is_err(), "no loopback expected");

    // A leaves; both see the departure
    send_client_msg(&mut tx_a, ClientBody::LeaveVoice(LeaveVoice {})).await;
    expect_voice_state(&mut rx_a, 1, 0, false).await;
    expect_voice_state(&mut rx_b, 1, 0, false).await;

    conn_a.close(0u32.into(), b"done");
    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn voice_roundtrip_synthetic_audio() {
    init_logs();

    let (config, server) = setup_server_with_user("opus");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let (conn_a, mut tx_a, mut rx_a) =
        connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    let (conn_b, mut tx_b, mut rx_b) =
        connect_and_auth(&endpoint, addr, "seconduser", "secondpass").await;

    // Both join voice channel 1
    send_client_msg(
        &mut tx_a,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 1, 1, true).await;
    expect_voice_state(&mut rx_b, 1, 1, true).await;
    send_client_msg(
        &mut tx_b,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 2, 1, true).await;
    expect_voice_state(&mut rx_b, 2, 1, true).await;

    // ---- Part 1: end-to-end through the server ----
    // A generates 10 frames of 440 Hz sine, encodes, sends as datagrams.
    let frames = sine_frames(10);
    let mut encoder = parley_core::audio::OpusEncoder::new(64_000).unwrap();
    let mut seq: u16 = 0;
    let mut ts: u32 = 0;
    for frame in &frames {
        let packet = encoder.encode(frame).unwrap().to_vec();
        let dg = parley_proto::media::encode(0, seq, ts, 0, &packet);
        conn_a.send_datagram(dg.into()).unwrap();
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(parley_core::audio::SAMPLES_PER_FRAME as u32);
    }

    // B receives all 10 through the SFU, decodes, concatenates.
    let mut decoder = parley_core::audio::OpusDecoder::new().unwrap();
    let mut decoded: Vec<i16> = Vec::new();
    for _ in 0..10 {
        let d = timeout(Duration::from_secs(2), conn_b.read_datagram())
            .await
            .expect("datagram should arrive")
            .unwrap();
        let m = parley_proto::media::decode(&d).unwrap();
        assert_eq!(m.sender_id, 1, "server must stamp authenticated sender id");
        decoded.extend(decoder.decode(m.payload).unwrap());
    }

    // Signal must survive the trip: encode → QUIC → SFU → decode.
    let original: Vec<i16> = frames.concat();
    assert_eq!(decoded.len(), original.len());
    let corr = best_correlation(&original, &decoded);
    assert!(corr > 0.95, "correlation too low: {corr:.3}");

    // ---- Part 2: jitter buffer under loss + reorder + duplicate ----
    // Local-only (no network): feed encoded packets into the buffer in a
    // hostile order and verify what comes out.
    let frames2 = sine_frames(10);
    let mut encoder2 = parley_core::audio::OpusEncoder::new(64_000).unwrap();
    let packets: Vec<Vec<u8>> = frames2
        .iter()
        .map(|f| encoder2.encode(f).unwrap().to_vec())
        .collect();

    let mut jb = parley_core::audio::JitterBuffer::new();

    // Delivery order: 0-4, skip 5 (lost), 6, 7, then late 5, 8 twice, 9.
    for &idx in &[0usize, 1, 2, 3, 4, 6, 7, 5, 8, 8, 9] {
        let seq = 10u16.wrapping_add(idx as u16);
        jb.push(seq, &packets[idx]);
    }

    // Drain everything. All 9 surviving frames must emerge in order 0-9
    // minus 5; the late-arriving 5 must fill its hole; the duplicate 8
    // must not produce a second output.
    let mut received: Vec<Vec<u8>> = Vec::new();
    let mut gaps = 0;
    for _ in 0..50 {
        match jb.pop() {
            (PopOutcome::Packet, Some(packet)) => received.push(packet),
            (PopOutcome::Packet, None) => unreachable!(),
            (PopOutcome::Gap, _) => gaps += 1,
            (PopOutcome::Empty, _) => break,
        }
    }

    assert_eq!(
        received.len(),
        10,
        "duplicate dropped, all 10 frames survive"
    );
    assert_eq!(gaps, 0, "late frame 5 must prevent gap declaration");

    // Frames must emerge in order 0-9; the duplicate 8 produced one copy.
    let expected = [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9];
    for (i, packet) in received.iter().enumerate() {
        assert_eq!(packet, &packets[expected[i]], "frame at slot {i} wrong");
    }

    conn_a.close(0u32.into(), b"done");
    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn video_fragments_routed_between_members() {
    init_logs();

    let (config, server) = setup_server_with_user("video-route");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let (conn_a, mut tx_a, mut rx_a) =
        connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    let (conn_b, mut tx_b, mut rx_b) =
        connect_and_auth(&endpoint, addr, "seconduser", "secondpass").await;

    // Both join voice channel 1 (video rides the same channel membership).
    send_client_msg(
        &mut tx_a,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 1, 1, true).await;
    expect_voice_state(&mut rx_b, 1, 1, true).await;
    send_client_msg(
        &mut tx_b,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 2, 1, true).await;
    expect_voice_state(&mut rx_b, 2, 1, true).await;

    // A sends one fragmented keyframe + one delta frame; B reassembles.
    let keyframe: Vec<u8> = (0..5000u32).map(|i| (i % 253) as u8).collect();
    let delta: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
    let dgs = parley_proto::video::fragment_frame(0, true, &keyframe, 0, 0, 0);
    let dgs2 = parley_proto::video::fragment_frame(1, false, &delta, 100, 960, 0);

    for dg in dgs.iter().chain(dgs2.iter()) {
        conn_a.send_datagram(dg.clone().into()).unwrap();
    }

    // B receives all datagrams, reassembles both frames.
    let mut asm = parley_proto::video::VideoAssembler::new();
    let expected_total = dgs.len() + dgs2.len();
    for _ in 0..expected_total {
        let d = timeout(Duration::from_secs(2), conn_b.read_datagram())
            .await
            .expect("datagram should arrive")
            .unwrap();
        let m = parley_proto::media::decode(&d).unwrap();
        assert!(m.flags & parley_proto::media::FLAG_VIDEO != 0);
        assert_eq!(m.sender_id, 1, "server stamps sender id");
        let frag = parley_proto::video::decode_fragment(m.payload).unwrap();
        asm.push_fragment(frag);
    }

    let f1 = asm.pop_frame().expect("keyframe assembled");
    assert_eq!(f1.data, keyframe);
    assert!(f1.keyframe);
    let f2 = asm.pop_frame().expect("delta assembled");
    assert_eq!(f2.data, delta);
    assert!(!f2.keyframe);
    assert_eq!(asm.frames_completed, 2);

    // A does not receive its own video back.
    let loopback = timeout(Duration::from_millis(200), conn_a.read_datagram()).await;
    assert!(loopback.is_err(), "no video loopback to sender");

    conn_a.close(0u32.into(), b"done");
    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn unknown_body_ignored_session_survives() {
    init_logs();

    let (config, server) = setup_server_with_user("unknown-body");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let (conn, mut tx, mut rx) = connect_and_auth(&endpoint, addr, "testuser", "testpass").await;

    // body: None — exactly what an older prost decodes a NEWER client's
    // unknown oneof variant to. Must be ignored, not fatal: the protocol
    // is additive, an old server survives a new client.
    session::write_message(&mut tx, &ClientMessage { body: None })
        .await
        .unwrap();

    // Session still alive: heartbeat still answers.
    send_client_msg(
        &mut tx,
        ClientBody::Heartbeat(Heartbeat { client_time_ms: 1 }),
    )
    .await;
    let msg = read_server_msg(&mut rx).await;
    match msg.body {
        Some(ServerBody::HeartbeatAck(ack)) => assert_eq!(ack.client_time_ms, 1),
        other => panic!("expected heartbeat ack, got {other:?}"),
    }

    conn.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

// ---- Phase 6b: multi-channel correctness ----

/// A + B in channel 1, C in channel 2. Returns (conn_a.., conn_b.., conn_c..).
/// All join-broadcasts are consumed on each stream.
async fn setup_two_channels(
    tag: &str,
) -> (
    (quinn::Connection, quinn::SendStream, quinn::RecvStream),
    (quinn::Connection, quinn::SendStream, quinn::RecvStream),
    (quinn::Connection, quinn::SendStream, quinn::RecvStream),
    u64,
    Config,
) {
    init_logs();
    let (config, server) = setup_server_with_user(tag);
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());
    let ch2 = add_channel(&config, "stage", false);

    let endpoint = client_endpoint(pinned);
    let mut a = connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    let mut b = connect_and_auth(&endpoint, addr, "seconduser", "secondpass").await;
    let mut c = connect_and_auth(&endpoint, addr, "thirduser", "thirdpass").await;

    // A and B join channel 1; C joins channel 2. Consume every
    // join broadcast on every stream (order: A, B, C).
    send_client_msg(&mut a.1, ClientBody::JoinVoice(JoinVoice { channel_id: 1 })).await;
    expect_voice_state(&mut a.2, 1, 1, true).await;
    expect_voice_state(&mut b.2, 1, 1, true).await;
    expect_voice_state(&mut c.2, 1, 1, true).await;

    send_client_msg(&mut b.1, ClientBody::JoinVoice(JoinVoice { channel_id: 1 })).await;
    expect_voice_state(&mut a.2, 2, 1, true).await;
    expect_voice_state(&mut b.2, 2, 1, true).await;
    expect_voice_state(&mut c.2, 2, 1, true).await;

    send_client_msg(
        &mut c.1,
        ClientBody::JoinVoice(JoinVoice { channel_id: ch2 }),
    )
    .await;
    expect_voice_state(&mut a.2, 3, ch2, true).await;
    expect_voice_state(&mut b.2, 3, ch2, true).await;
    expect_voice_state(&mut c.2, 3, ch2, true).await;

    (a, b, c, ch2, config)
}

#[tokio::test]
async fn media_datagrams_no_leak_across_channels() {
    let (a, b, c, _ch2, config) = setup_two_channels("leak").await;

    // A (ch1) sends an audio datagram. B (ch1) receives; C (ch2) must not.
    let payload: Vec<u8> = (0..160u32).map(|i| (i % 251) as u8).collect();
    let dg = parley_proto::media::encode(0, 42, 48_000, 0, &payload);
    a.0.send_datagram(dg.into()).unwrap();

    let received = timeout(Duration::from_secs(2), b.0.read_datagram())
        .await
        .expect("same-channel member must receive")
        .unwrap();
    let decoded = parley_proto::media::decode(&received).unwrap();
    assert_eq!(decoded.sender_id, 1);
    assert_eq!(decoded.payload, &payload[..]);

    // C is in another channel: nothing may arrive.
    let leak = timeout(Duration::from_millis(300), c.0.read_datagram()).await;
    assert!(leak.is_err(), "media leaked across channels");

    // A gets no loopback.
    let loopback = timeout(Duration::from_millis(300), a.0.read_datagram()).await;
    assert!(loopback.is_err(), "no loopback expected");

    a.0.close(0u32.into(), b"done");
    b.0.close(0u32.into(), b"done");
    c.0.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn receiver_report_routed_to_channel_members_only() {
    let (mut a, mut b, mut c, _ch2, config) = setup_two_channels("report").await;

    // A (ch1) reports loss. B (ch1) receives it; C (ch2) and A must not.
    send_client_msg(
        &mut a.1,
        ClientBody::ReceiverReport(ReceiverReport {
            gap_rate_permille: 42,
            reporter_id: 0, // server stamps
        }),
    )
    .await;

    let msg = read_server_msg(&mut b.2).await;
    match msg.body {
        Some(ServerBody::ReceiverReport(rep)) => {
            assert_eq!(rep.reporter_id, 1, "stamped with authenticated id");
            assert_eq!(rep.gap_rate_permille, 42);
        }
        other => panic!("expected receiver report, got {other:?}"),
    }

    // No self-echo to the reporter.
    let echo = timeout(
        Duration::from_millis(300),
        session::read_message::<ServerMessage>(&mut a.2),
    )
    .await;
    assert!(echo.is_err(), "reporter must not receive own echo");

    // No cross-channel delivery.
    let leak = timeout(
        Duration::from_millis(300),
        session::read_message::<ServerMessage>(&mut c.2),
    )
    .await;
    assert!(leak.is_err(), "report leaked across channels");

    a.0.close(0u32.into(), b"done");
    b.0.close(0u32.into(), b"done");
    c.0.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn stop_share_scoped_to_channel_members() {
    let (mut a, mut b, mut c, _ch2, config) = setup_two_channels("stop-scope").await;

    // A (ch1, sharing) stops. B (ch1) is told; C (ch2) and A must not be.
    send_client_msg(&mut a.1, ClientBody::StopShare(StopShare {})).await;

    let msg = read_server_msg(&mut b.2).await;
    match msg.body {
        Some(ServerBody::ShareStopped(s)) => {
            assert_eq!(s.user_id, 1, "stamped with the authenticated sharer id");
        }
        other => panic!("expected share stopped, got {other:?}"),
    }

    let echo = timeout(
        Duration::from_millis(300),
        session::read_message::<ServerMessage>(&mut a.2),
    )
    .await;
    assert!(echo.is_err(), "sharer must not receive own echo");

    let leak = timeout(
        Duration::from_millis(300),
        session::read_message::<ServerMessage>(&mut c.2),
    )
    .await;
    assert!(leak.is_err(), "stop notice leaked across channels");

    a.0.close(0u32.into(), b"done");
    b.0.close(0u32.into(), b"done");
    c.0.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

/// 7k: a share cannot outlive channel membership. Leaving voice while
/// sharing used to notify nobody — viewers kept the card (and its Watch
/// button showed the last decoded frame) indefinitely.
#[tokio::test]
async fn share_stop_notice_on_voice_leave() {
    let (mut a, mut b, mut c, _ch2, config) = setup_two_channels("stop-leave").await;

    // A is sharing: one video datagram is what marks that server-side
    // (the teardown paths only announce a stop for a live share).
    let dg = parley_proto::media::encode(parley_proto::media::FLAG_VIDEO, 0, 0, 0, &[0u8; 16]);
    a.0.send_datagram(dg.into()).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;

    send_client_msg(&mut a.1, ClientBody::LeaveVoice(LeaveVoice {})).await;

    let mut saw_stop = false;
    let mut saw_leave = false;
    while !(saw_stop && saw_leave) {
        let msg = timeout(Duration::from_secs(2), read_server_msg(&mut b.2))
            .await
            .expect("viewer must hear about the leave");
        match msg.body {
            Some(ServerBody::ShareStopped(s)) => {
                assert_eq!(s.user_id, 1, "stamped with the leaver's id");
                saw_stop = true;
            }
            Some(ServerBody::VoiceState(v)) if v.user_id == 1 => {
                assert!(!v.joined);
                saw_leave = true;
            }
            _ => {}
        }
    }

    // Neither the leaver nor the other channel sees the stop notice.
    for stream in [&mut a.2, &mut c.2] {
        for _ in 0..4 {
            let Ok(read) = timeout(
                Duration::from_millis(300),
                session::read_message::<ServerMessage>(stream),
            )
            .await
            else {
                break;
            };
            let Ok(msg) = read else { break };
            assert!(
                !matches!(msg.body, Some(ServerBody::ShareStopped(_))),
                "stop notice must stay in the sharer's channel and exclude the sharer"
            );
        }
    }

    a.0.close(0u32.into(), b"done");
    b.0.close(0u32.into(), b"done");
    c.0.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

/// 7k: a sharer whose app dies (crash, closed window, network drop)
/// never sends StopShare — the server's own teardown has to tell the
/// room, or its viewers hold a dead card until they reconnect.
#[tokio::test]
async fn share_stop_notice_on_session_death() {
    let (a, mut b, c, _ch2, config) = setup_two_channels("stop-death").await;

    // Mark the share, give the server's datagram task a beat to see it,
    // then kill the session the way a crash does: no StopShare ever
    // rides the control stream.
    let dg = parley_proto::media::encode(parley_proto::media::FLAG_VIDEO, 0, 0, 0, &[0u8; 16]);
    a.0.send_datagram(dg.into()).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    a.0.close(0u32.into(), b"gone");

    let mut saw_stop = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !saw_stop && tokio::time::Instant::now() < deadline {
        let Ok(msg) = timeout(Duration::from_secs(1), read_server_msg(&mut b.2)).await else {
            continue;
        };
        if let Some(ServerBody::ShareStopped(s)) = msg.body {
            assert_eq!(s.user_id, 1, "stamped with the dead sharer's id");
            saw_stop = true;
        }
    }
    assert!(
        saw_stop,
        "viewers must hear that a dead sharer's share ended"
    );

    b.0.close(0u32.into(), b"done");
    c.0.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn voice_states_snapshot_two_channels() {
    let (mut a, _b, _c, ch2, config) = setup_two_channels("snapshot").await;

    // A requests the snapshot: three members, two channels.
    send_client_msg(
        &mut a.1,
        ClientBody::VoiceStatesRequest(VoiceStatesRequest {}),
    )
    .await;
    let msg = read_server_msg(&mut a.2).await;
    match msg.body {
        Some(ServerBody::VoiceStateList(list)) => {
            assert_eq!(list.states.len(), 3);
            let by_user: std::collections::HashMap<u32, u64> = list
                .states
                .iter()
                .map(|s| (s.user_id, s.channel_id))
                .collect();
            assert_eq!(by_user[&1], 1, "testuser in channel 1");
            assert_eq!(by_user[&2], 1, "seconduser in channel 1");
            assert_eq!(by_user[&3], ch2, "thirduser in the created channel")
        }
        other => panic!("expected voice state list, got {other:?}"),
    }

    a.0.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn channel_switch_broadcasts_leave_then_join() {
    init_logs();

    let (config, server) = setup_server_with_user("switch");
    let ch2 = add_channel(&config, "stage", false);
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let (conn_a, mut tx_a, mut rx_a) =
        connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    let (conn_b, _tx_b, mut rx_b) =
        connect_and_auth(&endpoint, addr, "seconduser", "secondpass").await;

    // A joins ch1; both see it.
    send_client_msg(
        &mut tx_a,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 1, 1, true).await;
    expect_voice_state(&mut rx_b, 1, 1, true).await;

    // A switches to ch2: peers must see leave(ch1) BEFORE join(ch2) —
    // a missing leave freezes their viewers/mixers.
    send_client_msg(
        &mut tx_a,
        ClientBody::JoinVoice(JoinVoice { channel_id: ch2 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 1, 1, false).await;
    expect_voice_state(&mut rx_b, 1, 1, false).await;
    expect_voice_state(&mut rx_a, 1, ch2, true).await;
    expect_voice_state(&mut rx_b, 1, ch2, true).await;

    // And the datagram routing followed: A in ch2, B alone in ch1.
    let payload: Vec<u8> = vec![7u8; 160];
    let dg = parley_proto::media::encode(0, 1, 48_000, 0, &payload);
    conn_a.send_datagram(dg.into()).unwrap();
    let leak = timeout(Duration::from_millis(300), conn_b.read_datagram()).await;
    assert!(leak.is_err(), "switcher's media must not reach old channel");

    conn_a.close(0u32.into(), b"done");
    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

// ---- Phase 6c: relogin, session replacement, dead-client reaping ----

#[tokio::test]
async fn token_relogin_end_to_end() {
    init_logs();

    let (config, server) = setup_server_with_user("token-relogin");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);

    // Password login — grab the issued token.
    let (conn_a, _tx_a, _rx_a, ok_a) =
        connect_and_auth_full(&endpoint, addr, "testuser", "testpass").await;
    assert!(!ok_a.session_token.is_empty());

    // Fresh connection, token-only auth: username/password empty —
    // the server must not need them on the token path.
    let conn_b = endpoint.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut tx_b, mut rx_b) = conn_b.open_bi().await.unwrap();
    session::write_message(
        &mut tx_b,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await
    .unwrap();
    let _ack: HelloAck = session::read_message(&mut rx_b).await.unwrap();
    session::write_message(
        &mut tx_b,
        &AuthRequest {
            username: String::new(),
            password: String::new(),
            session_token: ok_a.session_token.clone(),
        },
    )
    .await
    .unwrap();
    let ok_b: AuthOk = session::read_message(&mut rx_b).await.unwrap();
    assert_eq!(
        ok_b.user_id, ok_a.user_id,
        "token must resolve to the same user"
    );

    // The token session is fully usable: heartbeat answers.
    send_client_msg(
        &mut tx_b,
        ClientBody::Heartbeat(Heartbeat { client_time_ms: 5 }),
    )
    .await;
    let msg = read_server_msg(&mut rx_b).await;
    match msg.body {
        Some(ServerBody::HeartbeatAck(ack)) => assert_eq!(ack.client_time_ms, 5),
        other => panic!("expected heartbeat ack, got {other:?}"),
    }

    // A garbage token is denied.
    let conn_c = endpoint.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut tx_c, mut rx_c) = conn_c.open_bi().await.unwrap();
    session::write_message(
        &mut tx_c,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await
    .unwrap();
    let _ack: HelloAck = session::read_message(&mut rx_c).await.unwrap();
    session::write_message(
        &mut tx_c,
        &AuthRequest {
            username: String::new(),
            password: String::new(),
            session_token: "not-a-real-token".to_string(),
        },
    )
    .await
    .unwrap();
    let denied: AuthDenied = session::read_message(&mut rx_c).await.unwrap();
    assert!(!denied.reason.is_empty());

    conn_a.close(0u32.into(), b"done");
    conn_b.close(0u32.into(), b"done");
    conn_c.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn second_login_replaces_session() {
    init_logs();

    let (config, server) = setup_server_with_user("replace");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    // Observer
    let (conn_b, mut tx_b, mut rx_b) =
        connect_and_auth(&endpoint, addr, "seconduser", "secondpass").await;
    // First session of user 1, in voice.
    let (conn_a, mut tx_a, mut rx_a) =
        connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    send_client_msg(
        &mut tx_a,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 1, 1, true).await;
    expect_voice_state(&mut rx_b, 1, 1, true).await;

    // Same user logs in again — replaces the registry entry.
    let (conn_a2, mut tx_a2, mut rx_a2) =
        connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    // First connection dies. Its cleanup is generation-guarded: it
    // must NOT unregister the successor, NOT leave voice, NOT
    // broadcast a departure.
    conn_a.close(0u32.into(), b"replaced");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // B saw no leave for user 1. expect_silence, not a bare timeout:
    // a2's registration legitimately pushed a PresenceList to B —
    // ambient, tolerated — but nothing else may arrive (no departure,
    // no voice state: the replaced session's cleanup is
    // generation-guarded).
    expect_silence(&mut rx_b, Duration::from_millis(500)).await;

    // User 1 is still in voice — membership outlived the swap.
    send_client_msg(
        &mut tx_b,
        ClientBody::VoiceStatesRequest(VoiceStatesRequest {}),
    )
    .await;
    let msg = read_server_msg(&mut rx_b).await;
    match msg.body {
        Some(ServerBody::VoiceStateList(list)) => {
            assert_eq!(list.states.len(), 1);
            assert_eq!(list.states[0].user_id, 1);
            assert_eq!(list.states[0].channel_id, 1);
        }
        other => panic!("expected voice state list, got {other:?}"),
    }

    // The successor is fully served.
    send_client_msg(
        &mut tx_a2,
        ClientBody::Heartbeat(Heartbeat { client_time_ms: 9 }),
    )
    .await;
    let msg = read_server_msg(&mut rx_a2).await;
    match msg.body {
        Some(ServerBody::HeartbeatAck(ack)) => assert_eq!(ack.client_time_ms, 9),
        other => panic!("expected heartbeat ack, got {other:?}"),
    }

    conn_b.close(0u32.into(), b"done");
    conn_a2.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn dead_client_reaped_by_idle_timeout() {
    init_logs();

    let (config, server) = setup_server_with_user("idle-timeout");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint_b = client_endpoint(pinned.clone());
    let (conn_b, mut tx_b, mut rx_b) =
        connect_and_auth(&endpoint_b, addr, "seconduser", "secondpass").await;

    // A gets its own endpoint so its death can be SILENT: forgetting
    // every handle leaks the socket — no close frame, exactly like
    // kill -9. (Dropping would send CONNECTION_CLOSE.)
    let endpoint_a = client_endpoint(pinned);
    let (conn_a, mut tx_a, mut rx_a) =
        connect_and_auth(&endpoint_a, addr, "testuser", "testpass").await;
    send_client_msg(
        &mut tx_a,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 1, 1, true).await;
    expect_voice_state(&mut rx_b, 1, 1, true).await;

    std::mem::forget((endpoint_a, conn_a, tx_a, rx_a));

    // B must heartbeat while waiting: the server's idle timeout applies
    // to EVERY connection, observer included (QUIC negotiates the min
    // of both endpoints — a client can't opt out), and a silent B gets
    // reaped by the same 10 s deadline it's waiting on. Not a test
    // artifact: any client silent for 10 s is gone by design
    // (heartbeat_ms is 5 s; the reap window is ~2 missed beats).
    let started = std::time::Instant::now();
    let mut hb = tokio::time::interval(Duration::from_secs(3));
    hb.tick().await; // intervals fire once immediately: skip that tick
    let mut reaped = false;
    while !reaped {
        tokio::select! {
            _ = hb.tick() => {
                send_client_msg(
                    &mut tx_b,
                    ClientBody::Heartbeat(Heartbeat { client_time_ms: 0 }),
                )
                .await;
            }
            msg = session::read_message::<ServerMessage>(&mut rx_b) => {
                let msg = msg.expect("B stays alive while heartbeating");
                if let Some(ServerBody::VoiceState(v)) = &msg.body
                    && v.user_id == 1
                    && !v.joined
                {
                    reaped = true;
                }
                // HeartbeatAcks and anything else: keep waiting.
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "server never reaped the dead client"
        );
    }
    assert!(
        started.elapsed() >= Duration::from_secs(8),
        "cleanup must come from the idle timeout, not an immediate close"
    );

    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

// ---- Phase 6d: music channels ----

#[tokio::test]
async fn music_channel_stereo_survives_the_chain() {
    init_logs();

    let (config, server) = setup_server_with_user("music");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());
    let music_ch = add_channel(&config, "music-hall", true);

    let endpoint = client_endpoint(pinned);
    let (conn_a, mut tx_a, mut rx_a) =
        connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    let (conn_b, mut tx_b, mut rx_b) =
        connect_and_auth(&endpoint, addr, "seconduser", "secondpass").await;

    send_client_msg(
        &mut tx_a,
        ClientBody::JoinVoice(JoinVoice {
            channel_id: music_ch,
        }),
    )
    .await;
    expect_voice_state(&mut rx_a, 1, music_ch, true).await;
    expect_voice_state(&mut rx_b, 1, music_ch, true).await;
    send_client_msg(
        &mut tx_b,
        ClientBody::JoinVoice(JoinVoice {
            channel_id: music_ch,
        }),
    )
    .await;
    expect_voice_state(&mut rx_a, 2, music_ch, true).await;
    expect_voice_state(&mut rx_b, 2, music_ch, true).await;

    // A: interleaved 440 Hz L / 880 Hz R through the music pipeline
    // (256 kbps, Application::Audio, stereo), 10 frames.
    let n_frames = 10;
    let spf = parley_core::audio::SAMPLES_PER_FRAME;
    let mut stereo_pcm: Vec<i16> = Vec::with_capacity(n_frames * spf * 2);
    for i in 0..(n_frames * spf) {
        let t = i as f64 / parley_core::audio::SAMPLE_RATE as f64;
        let l = (0.3 * f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * i16::MAX as f64) as i16;
        let r = (0.3 * f64::sin(2.0 * std::f64::consts::PI * 880.0 * t) * i16::MAX as f64) as i16;
        stereo_pcm.push(l);
        stereo_pcm.push(r);
    }
    let mut send = parley_core::audio::pipeline::SendPipeline::new_music(256_000).unwrap();
    let mut packets = Vec::new();
    send.push_pcm(&stereo_pcm, &mut packets).unwrap();
    assert_eq!(packets.len(), n_frames);
    for (seq, ts, pkt) in &packets {
        let dg = parley_proto::media::encode(
            parley_proto::media::FLAG_SPEAKING | parley_proto::media::FLAG_STEREO,
            *seq,
            *ts,
            0, // server stamps
            pkt,
        );
        conn_a.send_datagram(dg.into()).unwrap();
    }

    // B: through the SFU, into the stereo receive path.
    let mut recv = parley_core::audio::pipeline::RecvPipeline::new(1).unwrap();
    recv.set_stereo(true).unwrap();
    for _ in 0..n_frames {
        let d = timeout(Duration::from_secs(2), conn_b.read_datagram())
            .await
            .expect("datagram should arrive")
            .unwrap();
        let m = parley_proto::media::decode(&d).unwrap();
        assert_eq!(m.sender_id, 1, "server stamps sender id");
        assert!(
            m.flags & parley_proto::media::FLAG_STEREO != 0,
            "stereo flag must survive the SFU"
        );
        recv.push_packet(m.seq, m.payload);
    }
    let mut out = Vec::new();
    recv.pull_pcm(&mut out, n_frames + 2).unwrap();
    let decoded: Vec<i16> = out.iter().map(|&s| (s * 32767.0) as i16).collect();
    assert_eq!(decoded.len(), n_frames * spf * 2);

    let l_orig: Vec<i16> = stereo_pcm.iter().step_by(2).copied().collect();
    let r_orig: Vec<i16> = stereo_pcm.iter().skip(1).step_by(2).copied().collect();
    let l: Vec<i16> = decoded.iter().step_by(2).copied().collect();
    let r: Vec<i16> = decoded.iter().skip(1).step_by(2).copied().collect();

    assert!(
        best_correlation(&l_orig, &l) > 0.9,
        "left must survive as left: {:.3}",
        best_correlation(&l_orig, &l)
    );
    assert!(
        best_correlation(&r_orig, &r) > 0.9,
        "right must survive as right: {:.3}",
        best_correlation(&r_orig, &r)
    );
    assert!(
        best_correlation(&l, &r_orig) < 0.5,
        "channels must stay separate: {:.3}",
        best_correlation(&l, &r_orig)
    );

    conn_a.close(0u32.into(), b"done");
    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

// ---- Phase 6e: video loss feedback routing ----

#[tokio::test]
async fn video_report_routed_to_channel_members_only() {
    let (mut a, mut b, mut c, _ch2, config) = setup_two_channels("vreport").await;

    send_client_msg(
        &mut a.1,
        ClientBody::VideoReport(VideoReport {
            gap_rate_permille: 77,
            reporter_id: 0, // server stamps
        }),
    )
    .await;

    let msg = read_server_msg(&mut b.2).await;
    match msg.body {
        Some(ServerBody::VideoReport(rep)) => {
            assert_eq!(rep.reporter_id, 1, "stamped with authenticated id");
            assert_eq!(rep.gap_rate_permille, 77);
        }
        other => panic!("expected video report, got {other:?}"),
    }

    let echo = timeout(
        Duration::from_millis(300),
        session::read_message::<ServerMessage>(&mut a.2),
    )
    .await;
    assert!(echo.is_err(), "reporter must not receive own echo");

    let leak = timeout(
        Duration::from_millis(300),
        session::read_message::<ServerMessage>(&mut c.2),
    )
    .await;
    assert!(leak.is_err(), "video report leaked across channels");

    a.0.close(0u32.into(), b"done");
    b.0.close(0u32.into(), b"done");
    c.0.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

// ---- Phase 6h: robustness ----

#[tokio::test]
async fn restart_persistence_end_to_end() {
    init_logs();

    let (config, server) = setup_server_with_user("restart-persist");
    // A music channel must survive the restart with its mode.
    {
        let db = parley_server::db::Db::open(&config.data_dir.join("parley.db")).unwrap();
        let _music_id = db.create_channel("stage", true).unwrap();
        drop(db);
    }
    let addr = server.local_addr();
    let fp1 = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(fp1.clone());
    let (conn_a, _tx_a, _rx_a) = connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    conn_a.close(0u32.into(), b"done"); // silence unused in this scope
    conn_a.close(0u32.into(), b"done");

    // --- restart: drop the endpoint (closes conns), rebind on same data dir ---
    drop(endpoint);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let server2 = Server::bind(&config).unwrap();
    let fp2 = server2.fingerprint().to_string();
    assert_eq!(fp1, fp2, "fingerprint must survive restart");
    let addr2 = server2.local_addr();
    tokio::spawn(server2.run());

    // Accounts survive: same password works.
    let endpoint2 = client_endpoint(fp2);
    let (conn_b, mut tx_b, mut rx_b) =
        connect_and_auth(&endpoint2, addr2, "testuser", "testpass").await;

    // Channels survive, including music mode.
    send_client_msg(&mut tx_b, ClientBody::ListChannels(ListChannels {})).await;
    let msg = read_server_msg(&mut rx_b).await;
    match msg.body {
        Some(ServerBody::ChannelList(list)) => {
            let general = list.channels.iter().find(|c| c.id == 1).expect("general");
            assert_eq!(general.mode, 0, "general stays speech");
            let stage = list
                .channels
                .iter()
                .find(|c| c.name == "stage")
                .expect("music channel survived restart");
            assert_eq!(stage.mode, 1, "music mode survived restart");
        }
        other => panic!("expected channel list, got {other:?}"),
    }

    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn malformed_input_never_kills_the_server() {
    init_logs();

    let (config, server) = setup_server_with_user("malformed");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let (conn, mut tx, mut rx) = connect_and_auth(&endpoint, addr, "testuser", "testpass").await;

    // Heartbeat helper: the session must still work after each attack.
    async fn alive(tx: &mut quinn::SendStream, rx: &mut quinn::RecvStream, n: u64) {
        send_client_msg(tx, ClientBody::Heartbeat(Heartbeat { client_time_ms: n })).await;
        let msg = read_server_msg(rx).await;
        match msg.body {
            Some(ServerBody::HeartbeatAck(ack)) => assert_eq!(ack.client_time_ms, n),
            other => panic!("session died: expected heartbeat ack, got {other:?}"),
        }
    }

    // --- hostile datagrams: empty, garbage, bad version, truncated
    // video header. Server must drop them and keep serving. (An
    // oversized-media case is unreachable: client endpoints cap
    // datagrams at ~1200 B, so >MTU media can't enter the network.)
    conn.send_datagram(Vec::new().into()).unwrap();
    conn.send_datagram(vec![0u8; 5].into()).unwrap();
    // Valid framing, version bits = 7 (unsupported).
    let mut bad_ver = parley_proto::media::encode(0, 1, 0, 1, &[0xAA; 20]);
    bad_ver[0] = 0xFF;
    conn.send_datagram(bad_ver.into()).unwrap();
    // FLAG_VIDEO with a payload shorter than the video sub-header.
    conn.send_datagram(
        parley_proto::media::encode(
            parley_proto::media::FLAG_VIDEO,
            2,
            0,
            1,
            &[0xBB; 4], // < 9-byte video header
        )
        .into(),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    alive(&mut tx, &mut rx, 1).await;

    // --- hostile stream bytes: random garbage as a "protobuf frame". ---
    tx.write_all(&frame::encode(&[0xDE, 0xAD, 0xBE, 0xEF, 0x99]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // A decode failure ends THIS session by design (the stream is
    // desynchronized — recovery is impossible); the assert is that it
    // ends cleanly, and the SERVER lives on for the next client.
    let ended = session::read_message::<ServerMessage>(&mut rx).await;
    assert!(
        ended.is_err(),
        "garbage protobuf must end the session (cleanly)"
    );

    // --- the server must still accept a FRESH session after all that. ---
    let (conn2, mut tx2, mut rx2) = connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    alive(&mut tx2, &mut rx2, 2).await;

    // --- oversized length prefix: rejected before allocation (64 KiB
    // cap), ends the session cleanly, no panic. ---
    tx2.write_all(&0x7FFF_FFFFu32.to_be_bytes()).await.unwrap();
    let ended2 = session::read_message::<ServerMessage>(&mut rx2).await;
    assert!(
        ended2.is_err(),
        "oversized frame must end the session (cleanly)"
    );

    conn.close(0u32.into(), b"done");
    conn2.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}

#[tokio::test]
async fn video_routing_is_order_robust() {
    // The .16-era investigation promised this hardening: collect ALL
    // fragments before feeding the assembler, then sort by
    // (frame_id, frag_idx). What's under test is ROUTING (every
    // fragment arrives exactly once, correctly stamped) — not the
    // assembler's tolerance for cross-frame reordering, which is a
    // documented limitation (drop-until-IDR recovers within ~2 s).
    init_logs();

    let (config, server) = setup_server_with_user("video-order");
    let addr = server.local_addr();
    let pinned = server.fingerprint().to_string();
    tokio::spawn(server.run());

    let endpoint = client_endpoint(pinned);
    let (conn_a, mut tx_a, mut rx_a) =
        connect_and_auth(&endpoint, addr, "testuser", "testpass").await;
    let (conn_b, mut tx_b, mut rx_b) =
        connect_and_auth(&endpoint, addr, "seconduser", "secondpass").await;

    send_client_msg(
        &mut tx_a,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 1, 1, true).await;
    expect_voice_state(&mut rx_b, 1, 1, true).await;
    send_client_msg(
        &mut tx_b,
        ClientBody::JoinVoice(JoinVoice { channel_id: 1 }),
    )
    .await;
    expect_voice_state(&mut rx_a, 2, 1, true).await;
    expect_voice_state(&mut rx_b, 2, 1, true).await;

    // Three frames with distinctive content: key + 2 deltas.
    let key: Vec<u8> = (0..5000u32).map(|i| (i % 253) as u8).collect();
    let d1: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
    let d2: Vec<u8> = (0..2400u32).map(|i| (i % 247) as u8).collect();
    let dgs = parley_proto::video::fragment_frame(0, true, &key, 0, 0, 0);
    let dgs1 = parley_proto::video::fragment_frame(1, false, &d1, 100, 960, 0);
    let dgs2 = parley_proto::video::fragment_frame(2, false, &d2, 200, 1920, 0);
    let total = dgs.len() + dgs1.len() + dgs2.len();
    for dg in dgs.iter().chain(dgs1.iter()).chain(dgs2.iter()) {
        conn_a.send_datagram(dg.clone().into()).unwrap();
    }

    // Collect every fragment (owned data — lifetime-free until feed
    // time), verify identity + stamp, THEN sort and feed — arrival
    // order must not matter.
    #[derive(Clone)]
    struct OwnedFrag {
        frame_id: u32,
        frag_idx: u16,
        frag_count: u16,
        keyframe: bool,
        data: Vec<u8>,
    }
    let mut frags: Vec<OwnedFrag> = Vec::new();
    for _ in 0..total {
        let d = timeout(Duration::from_secs(2), conn_b.read_datagram())
            .await
            .expect("datagram should arrive")
            .unwrap();
        let m = parley_proto::media::decode(&d).unwrap();
        assert!(m.flags & parley_proto::media::FLAG_VIDEO != 0);
        assert_eq!(m.sender_id, 1, "server stamps sender id");
        let frag = parley_proto::video::decode_fragment(m.payload).unwrap();
        frags.push(OwnedFrag {
            frame_id: frag.frame_id,
            frag_idx: frag.frag_idx,
            frag_count: frag.frag_count,
            keyframe: frag.keyframe,
            data: m.payload[parley_proto::video::VIDEO_HEADER_LEN..].to_vec(),
        });
    }
    frags.sort_by_key(|f| (f.frame_id, f.frag_idx));

    let mut asm = parley_proto::video::VideoAssembler::new();
    for f in &frags {
        asm.push_fragment(parley_proto::video::VideoFragment {
            frame_id: f.frame_id,
            frag_idx: f.frag_idx,
            frag_count: f.frag_count,
            keyframe: f.keyframe,
            // 7h: this fixture is a 30 fps stream; the rate hint is
            // advisory and the assembler must not require it.
            high_rate: false,
            data: &f.data,
        });
    }
    let f0 = asm.pop_frame().expect("keyframe");
    assert_eq!(f0.data, key);
    assert!(f0.keyframe);
    let f1 = asm.pop_frame().expect("delta 1");
    assert_eq!(f1.data, &d1[..]);
    let f2 = asm.pop_frame().expect("delta 2");
    assert_eq!(f2.data, &d2[..]);
    assert_eq!(asm.frames_completed, 3);

    conn_a.close(0u32.into(), b"done");
    conn_b.close(0u32.into(), b"done");
    let _ = std::fs::remove_dir_all(&config.data_dir);
}
