//! parley-server entry point. Phase 0: identify and exit cleanly.
//! Phase 1 replaces this body with the QUIC listener.

use parley_core::Error;
use parley_proto::handshake::PROTOCOL_VERSION;

fn main() -> Result<(), Error> {
    println!(
        "parley-server {} (protocol v{})",
        env!("CARGO_PKG_VERSION"),
        PROTOCOL_VERSION
    );

    Ok(()) // nothing fallible yet; Phase 1 propagates real errors with `?`
}
