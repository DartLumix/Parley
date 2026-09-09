//! Thin launcher: logging, bind, run. All logic lives in the library.

use parley_server::{Config, Error, Server};

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::var("PARLEY_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let config = Config::load_or_default(std::path::Path::new(&config_path))?;

    let server = Server::bind(&config)?;
    server.run().await
}
