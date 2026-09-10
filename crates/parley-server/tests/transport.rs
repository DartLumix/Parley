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
    ClientBody, ClientMessage, Heartbeat, PresenceRequest, ServerBody, ServerMessage,
};

use parley_proto::{FetchHistory, ListChannels, SendChat};

use parley_proto::{JoinVoice, LeaveVoice, VoiceStatesRequest};
use std::time::Duration;
use tokio::time::timeout;

use parley_core::audio::jitter::PopOutcome;

async fn send_client_msg(tx: &mut quinn::SendStream, body: parley_proto::ClientBody) {
    session::write_message(tx, &parley_proto::ClientMessage { body: Some(body) })
        .await
        .unwrap();
}

async fn read_server_msg(rx: &mut quinn::RecvStream) -> parley_proto::ServerMessage {
    session::read_message(rx).await.unwrap()
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
    drop(db);

    let server = Server::bind(&config).unwrap();
    (config, server)
}

async fn connect_and_auth(
    endpoint: &quinn::Endpoint,
    addr: std::net::SocketAddr,
    username: &str,
    password: &str,
) -> (quinn::Connection, quinn::SendStream, quinn::RecvStream) {
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

    let response: ServerMessage = session::read_message(&mut rx).await.unwrap();
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

    // Alice requests presence
    session::write_message(
        &mut tx_a,
        &ClientMessage {
            body: Some(ClientBody::PresenceRequest(PresenceRequest {})),
        },
    )
    .await
    .unwrap();

    let response: ServerMessage = session::read_message(&mut rx_a).await.unwrap();
    match response.body {
        Some(ServerBody::PresenceList(list)) => {
            assert_eq!(list.users.len(), 2, "both alice and bob should be online");
            let names: Vec<&str> = list.users.iter().map(|u| u.username.as_str()).collect();
            assert!(names.contains(&"alice"));
            assert!(names.contains(&"bob"));
        }
        _ => panic!("expected presence list"),
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
        decoded.extend_from_slice(decoder.decode(m.payload).unwrap());
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
