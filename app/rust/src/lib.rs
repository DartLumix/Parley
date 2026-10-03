//! Phase 7 bridge crate: parley-core surfaced to Flutter via
//! flutter_rust_bridge. API surface lives in `api/`; this file stays
//! minimal (the generator may append its wiring here — verify with cat
//! after generate, before building).

mod api;
mod frb_generated;
