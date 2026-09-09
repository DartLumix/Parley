//! The wire-protocol vocabulary shared by the server and every client.

pub mod frame;
pub mod handshake;

/// Generated protobuf types (from proto/*.proto via build.rs).
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/parley.rs"));
}

pub use handshake::PROTOCOL_VERSION;
pub use pb::{Hello, HelloAck};
