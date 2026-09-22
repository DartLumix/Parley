//! parley-loadtest (6g): N synthetic talkers against a RUNNING server.
//!
//! Creates its users (WAL allows the brief concurrent writes — same
//! pattern as the transport tests), connects N senders + one observer
//! to voice channel 1, and pushes 160-byte audio datagrams at 50 Hz
//! per sender — exactly the real client's packet rate and size.
//! The observer counts what the SFU delivers; server CPU is measured
//! on the server process from the shell (`top -b`) while this runs.
//!
//! Certs: accepts any presented cert (signature verification still
//! delegated to ring) — loopback load tool against our own server;
//! TOFU pinning is for remote servers.
//!
//! Senders stream datagrams continuously (QUIC idle timeout resets on
//! any packet), so no heartbeats are needed inside the run window.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

use parley_core::session;
use parley_proto::{
    AuthOk, AuthRequest, ClientBody, ClientMessage, Hello, HelloAck, JoinVoice, PROTOCOL_VERSION,
    ServerBody, ServerMessage,
};

struct Args {
    server: String,
    senders: usize,
    duration: u64,
    data_dir: String,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        server: "127.0.0.1:4443".to_string(),
        senders: 10,
        duration: 60,
        data_dir: "data".to_string(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server" => args.server = it.next().ok_or("--server needs a value")?,
            "--senders" => {
                args.senders = it
                    .next()
                    .ok_or("--senders needs a value")?
                    .parse()
                    .map_err(|_| "--senders must be a number")?
            }
            "--duration" => {
                args.duration = it
                    .next()
                    .ok_or("--duration needs a value")?
                    .parse()
                    .map_err(|_| "--duration must be seconds")?
            }
            "--data-dir" => args.data_dir = it.next().ok_or("--data-dir needs a value")?,
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

/// Loopback load tool: accept any presented cert; key-exchange
/// signature checks still run via ring.
#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
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

fn client_endpoint() -> quinn::Endpoint {
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![parley_server::tls::alpn()];
    let config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("config"),
    ));
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("bind")).expect("client endpoint");
    endpoint.set_default_client_config(config);
    endpoint
}

/// Connect + password-auth; returns (conn, control tx, control rx, user_id).
async fn connect_and_auth(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    username: &str,
    password: &str,
) -> Result<(quinn::Connection, quinn::SendStream, quinn::RecvStream, u32), String> {
    let conn = endpoint
        .connect(addr, "localhost")
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| e.to_string())?;
    let (mut tx, mut rx) = conn.open_bi().await.map_err(|e| e.to_string())?;
    session::write_message(
        &mut tx,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            caps: 0,
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    let _ack: HelloAck = session::read_message(&mut rx)
        .await
        .map_err(|e| e.to_string())?;
    session::write_message(
        &mut tx,
        &AuthRequest {
            username: username.to_string(),
            password: password.to_string(),
            session_token: String::new(),
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    let ok: AuthOk = session::read_message(&mut rx)
        .await
        .map_err(|e| e.to_string())?;
    Ok((conn, tx, rx, ok.user_id))
}

/// Join voice channel 1 and wait for our own confirmation (skipping
/// other users' interleaved join broadcasts).
async fn join_voice(
    tx: &mut quinn::SendStream,
    rx: &mut quinn::RecvStream,
    user_id: u32,
) -> Result<(), String> {
    session::write_message(
        tx,
        &ClientMessage {
            body: Some(ClientBody::JoinVoice(JoinVoice { channel_id: 1 })),
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    loop {
        let msg: ServerMessage = session::read_message(rx).await.map_err(|e| e.to_string())?;
        if let Some(ServerBody::VoiceState(v)) = &msg.body
            && v.user_id == user_id
            && v.joined
            && v.channel_id == 1
        {
            return Ok(());
        }
    }
}

/// One synthetic talker: 160 B audio datagrams at 50 Hz — the real
/// client's exact rate and size at 64 kbps/20 ms.
///
/// Holds `_tx` (the control SendStream) for the whole run: dropping a
/// SendStream FINishes it, and the server treats control-stream EOF as
/// session end — unregister + leave voice. (First version dropped it:
/// every sender died the instant it joined; observer saw 0 datagrams.)
async fn sender_task(conn: quinn::Connection, _tx: quinn::SendStream, duration: Duration) {
    let payload = vec![0x55u8; 160];
    let mut seq: u16 = 0;
    let mut ts: u32 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let end = Instant::now() + duration;
    while Instant::now() < end {
        tick.tick().await;
        let dg =
            parley_proto::media::encode(parley_proto::media::FLAG_SPEAKING, seq, ts, 0, &payload);
        if conn.send_datagram(dg.into()).is_err() {
            return; // connection gone
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(960);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let addr: SocketAddr = args.server.parse().map_err(|_| "bad --server addr")?;
    let duration = Duration::from_secs(args.duration);

    // Create users (ignore errors: they persist across runs).
    let db = parley_server::db::Db::open(&Path::new(&args.data_dir).join("parley.db"))?;
    let _ = db.create_user("loadobs", "loadpass", false);
    for i in 0..args.senders {
        let _ = db.create_user(&format!("load{i}"), "loadpass", false);
    }
    drop(db);

    let endpoint = client_endpoint();

    // Observer: joins, then counts delivered datagrams.
    let (obs_conn, mut obs_tx, mut obs_rx, obs_id) =
        connect_and_auth(&endpoint, addr, "loadobs", "loadpass").await?;
    join_voice(&mut obs_tx, &mut obs_rx, obs_id).await?;
    // Drain the control stream: broadcasts would otherwise fill the
    // stream window and backpressure the server's session loop.
    tokio::spawn(async move {
        while session::read_message::<ServerMessage>(&mut obs_rx)
            .await
            .is_ok()
        {}
    });
    println!("loadtest: observer ready");

    // Senders.
    let mut tasks = Vec::new();
    for i in 0..args.senders {
        let (conn, mut tx, mut rx, id) =
            connect_and_auth(&endpoint, addr, &format!("load{i}"), "loadpass").await?;
        join_voice(&mut tx, &mut rx, id).await?;
        tokio::spawn(async move {
            while session::read_message::<ServerMessage>(&mut rx)
                .await
                .is_ok()
            {}
        });
        tasks.push(tokio::spawn(sender_task(conn, tx, duration)));
    }
    println!(
        "loadtest: {} senders streaming 50 pps each for {} s (expected observer rate: {} pps)",
        args.senders,
        args.duration,
        args.senders * 50
    );
    println!(
        "loadtest: measure the SERVER process now:  top -b -n 4 -d 5 -p (pgrep -x parley-server)"
    );

    let received = Arc::new(AtomicU64::new(0));
    let counter = received.clone();
    let obs_reader = tokio::spawn(async move {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            match tokio::time::timeout(Duration::from_secs(1), obs_conn.read_datagram()).await {
                Ok(Ok(_)) => {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                _ => continue,
            }
        }
    });

    for t in tasks {
        let _ = t.await;
    }
    let _ = obs_reader.await;

    let total = received.load(Ordering::Relaxed);
    let expected = args.senders as u64 * 50 * args.duration;
    println!(
        "loadtest: observer received {total} datagrams (expected ~{expected}, {:.0}%)",
        100.0 * total as f64 / expected as f64
    );
    Ok(())
}
