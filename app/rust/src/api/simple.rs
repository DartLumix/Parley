//! Smoke-test API — the first function across the bridge.

/// Bridge smoke test: Flutter calls this, Rust answers.
pub fn ping() -> String {
    "pong".to_string()
}
