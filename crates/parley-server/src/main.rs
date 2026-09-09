//! Thin launcher: logging, bind, run. All logic lives in the library.

use parley_server::{Error, Server};

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let server = Server::bind_default()?;
    server.run().await
}
