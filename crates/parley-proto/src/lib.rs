//! The wire-protocol vocabulary shared by the server and every client.
//!
//! Rules for this crate:
//! * no I/O, no async, no OS calls — data descriptions only
//! * zero external dependencies, so it compiles everywhere
//! * server and all clients link this exact crate: the protocol is
//!   defined in exactly one place and can never drift

pub mod handshake;
