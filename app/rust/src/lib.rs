//! Phase 7 bridge crate: parley-core surfaced to Flutter via
//! flutter_rust_bridge. API surface lives in `api/`; this file stays
//! minimal (the generator may append its wiring here — verify with cat
//! after generate, before building).

mod api;
mod frb_generated;

// The desktop client ships echo cancellation: voice on open speakers
// without it is a feedback loop. A Linux build that forgot the feature
// compiles cleanly otherwise, so fail loudly here instead of shipping
// a quieter regression. Gated on `screen-capture` — the marker of the
// shipping desktop feature set — so the reduced voice-only shape can
// still be built to check it. (Windows takes the same guard through the
// release workflow: `aec-rust`, the pure-Rust filter; see aec.rs.)
#[cfg(all(
    target_os = "linux",
    feature = "screen-capture",
    not(any(feature = "aec", feature = "aec-rust"))
))]
compile_error!("the Linux desktop build must enable the `aec` feature");
