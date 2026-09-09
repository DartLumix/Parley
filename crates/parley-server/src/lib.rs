//! Server engine. The binary (src/main.rs) is a thin launcher;
//! tests and future embedders consume this library.

pub mod error;
pub mod server;
pub mod tls;

pub use error::Error;
pub use server::Server;

pub mod config;
pub use config::Config;

pub mod db;
