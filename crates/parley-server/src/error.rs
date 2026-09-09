/// Startup and per-connection failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid listen address: {0}")]
    Addr(#[from] std::net::AddrParseError),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Cert(#[from] rcgen::Error),
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("stream read: {0}")]
    Read(#[from] quinn::ReadError),
    #[error("read to end: {0}")]
    ReadToEnd(#[from] quinn::ReadToEndError),
    #[error("stream write: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("stream finish: {0}")]
    Finished(#[from] quinn::ClosedStream),
    #[error("quinn tls setup: {0}")]
    QuinnTls(String),
    #[error("config: {0}")]
    Config(String),
    #[error("session: {0}")]
    Session(#[from] parley_core::Error),
}
