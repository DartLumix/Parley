//! Thin launcher: logging, bind, run. All logic lives in the library.
//!
//! One subcommand exists for operations, because that is how accounts are
//! created in the released container (no user-management RPC yet):
//!
//!     parley-server adduser <username> <password>
//!
//! It loads the same config as the server, opens the same database and
//! writes one account. The server may be running: SQLite WAL tolerates
//! the second connection. A BUSY error means retry with the server
//! stopped.

use parley_server::{Config, Error, Server};

fn config() -> Result<Config, Error> {
    let path = std::env::var("PARLEY_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    Config::load_or_default(std::path::Path::new(&path))
}

fn add_user(args: &[String]) -> Result<(), Error> {
    if args.len() != 2 {
        eprintln!("usage: parley-server adduser <username> <password>");
        std::process::exit(2);
    }
    let config = config()?;
    let db_path = config.data_dir.join("parley.db");
    let db = parley_server::db::Db::open(&db_path)?;
    db.create_user(&args[0], &args[1], false)?;
    println!("user \"{}\" created ({})", args[0], db_path.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("adduser") {
        return add_user(&args[1..]);
    }
    if matches!(
        args.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        println!(
            "usage:\n  \
             parley-server                 run the server (config: $PARLEY_CONFIG or config.toml)\n  \
             parley-server adduser USER PASS\n                                create an account in the configured data_dir"
        );
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let server = Server::bind(&config()?)?;
    server.run().await
}
