//! Server engine. The binary (src/main.rs) is a thin launcher;
//! tests and future embedders consume this library.

pub mod config;
pub mod db;
pub mod error;
pub mod presence;
pub mod server;
pub mod tls;

pub use config::Config;
pub use error::Error;
pub use presence::PresenceManager;
pub use server::Server;
