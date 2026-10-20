//! Remembered login: server + account + the session token the server
//! issued at auth.
//!
//! Closing the window is not signing out — the app comes back up already
//! connected to the last server. What is stored is the TOKEN from
//! `AuthOk`, never the password: it is revocable server-side, it expires
//! with the other session tokens, and a copied file cannot be replayed
//! against a password prompt.
//!
//! Path: `$XDG_CONFIG_HOME/parley/session.toml`, mode 0600 (the same
//! directory as the TOFU pins). `Disconnect` deletes it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The remembered session, as the login screen needs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedLogin {
    pub server: String,
    pub user: String,
    pub token: String,
}

fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

fn login_path() -> PathBuf {
    config_dir().join("parley").join("session.toml")
}

/// Persist the session. Rewrites the file atomically (tmp + rename) so a
/// crash mid-write can never leave a half token behind, and locks it down
/// to the owner — it is a bearer credential.
pub fn save(server: &str, user: &str, token: &str) -> Result<(), String> {
    save_to(
        &login_path(),
        &SavedLogin {
            server: server.trim().to_string(),
            user: user.trim().to_string(),
            token: token.to_string(),
        },
    )
}

/// The remembered login, or None when this machine has none. A file that
/// does not parse is treated as none (an older/edited file must not block
/// startup) and is left alone until the next successful login rewrites it.
pub fn load() -> Result<Option<SavedLogin>, String> {
    load_from(&login_path())
}

/// Forget the session — the explicit Disconnect/sign-out path.
pub fn forget() -> Result<(), String> {
    forget_at(&login_path())
}

fn save_to(path: &Path, saved: &SavedLogin) -> Result<(), String> {
    let text = toml::to_string(saved).map_err(|e| format!("encode login: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("install {}: {e}", path.display()))
}

fn load_from(path: &Path) -> Result<Option<SavedLogin>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    match toml::from_str::<SavedLogin>(&text) {
        Ok(saved) if !saved.server.is_empty() && !saved.token.is_empty() => Ok(Some(saved)),
        _ => Ok(None),
    }
}

fn forget_at(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("parley-login-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("session.toml")
    }

    #[test]
    fn save_load_and_forget_roundtrip() {
        let path = temp_path("roundtrip");
        assert!(load_from(&path).unwrap().is_none());

        save_to(
            &path,
            &SavedLogin {
                server: "parley.example.com:4443".into(),
                user: "alice".into(),
                token: "tok-123".into(),
            },
        )
        .unwrap();
        let saved = load_from(&path).unwrap().expect("saved login");
        assert_eq!(saved.server, "parley.example.com:4443");
        assert_eq!(saved.user, "alice");
        assert_eq!(saved.token, "tok-123");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the token file is owner-only");
        }

        forget_at(&path).unwrap();
        assert!(load_from(&path).unwrap().is_none());
        forget_at(&path).unwrap(); // idempotent: signing out twice is fine
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_file_reads_as_no_login() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not toml at all").unwrap();
        assert!(load_from(&path).unwrap().is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
