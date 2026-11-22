//! File logging for the native side.
//!
//! The Flutter side has its own session log (`app/lib/src/app_log.dart`);
//! this covers the voice/device thread, stream reopens and panics, which
//! `debugPrint` never sees. The file is rotated at startup exactly like the
//! Dart log, so a report after a hard crash still has the previous session.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static INIT: Once = Once::new();

fn config_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("PARLEY_DATA_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("APPDATA").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

fn log_dir() -> PathBuf {
    config_root().join("parley").join("logs")
}

fn ensure_open() {
    INIT.call_once(|| {
        let dir = log_dir();
        if fs::create_dir_all(&dir).is_err() {
            return;
        }
        let current = dir.join("rust-current.log");
        let previous = dir.join("rust-previous.log");
        if current.exists() {
            // A locked log (another copy still running) must not stop this
            // session: keep the old file and append instead.
            let _ = fs::remove_file(&previous);
            let _ = fs::rename(&current, &previous);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current)
            .ok();
        let _ = LOG.set(Mutex::new(file));
    });
}

/// Append one line. Safe from callbacks and the panic hook; a poisoned or
/// busy lock drops the line rather than blocking audio.
pub(crate) fn write(level: &str, message: &str) {
    ensure_open();
    let Some(lock) = LOG.get() else {
        return;
    };
    let Ok(mut guard) = lock.try_lock() else {
        return;
    };
    let Some(file) = guard.as_mut() else {
        return;
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(file, "{stamp} [{level}] {message}");
    let _ = file.flush();
}

/// Install the native panic hook and create the log file. Idempotent.
pub(crate) fn install_panic_hook() {
    ensure_open();
    use std::sync::Once as HookOnce;
    static HOOK: HookOnce = HookOnce::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            write("PANIC", &format!("{panic}"));
            previous(panic);
        }));
    });
}
