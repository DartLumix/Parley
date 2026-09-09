//! User accounts: SQLite storage, Argon2id password hashing.

use std::path::Path;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rusqlite::{Connection, OptionalExtension};
use tracing::{info, warn};

use crate::error::Error;

pub struct Db {
    conn: Connection,
}

pub struct UserInfo {
    pub id: u32,
    pub is_admin: bool,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                id INTEGER PRIMARY KEY,
                username TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL,
                is_admin INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )?;
        Ok(Self { conn })
    }

    /// On first boot (empty users table), create an admin account
    /// from PARLEY_ADMIN_PASSWORD. If unset, log a warning.
    pub fn bootstrap_admin(&self) -> Result<(), Error> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        if count > 0 {
            return Ok(());
        }
        match std::env::var("PARLEY_ADMIN_PASSWORD") {
            Ok(password) => {
                self.create_user("admin", &password, true)?;
                info!("admin account created (username: admin)");
            }
            Err(_) => {
                warn!(
                    "no users and PARLEY_ADMIN_PASSWORD not set — \
                       set it on first boot to create the admin account"
                );
            }
        }
        Ok(())
    }

    pub fn create_user(&self, username: &str, password: &str, is_admin: bool) -> Result<(), Error> {
        let hash = hash_password(password)?;
        self.conn.execute(
            "INSERT INTO users (username, password_hash, is_admin) VALUES (?1, ?2, ?3)",
            rusqlite::params![username, hash, is_admin as i32],
        )?;
        Ok(())
    }

    /// Returns Some(UserInfo) on success, None on bad credentials.
    pub fn verify_user(&self, username: &str, password: &str) -> Result<Option<UserInfo>, Error> {
        let row: Option<(i64, String, i32)> = self
            .conn
            .query_row(
                "SELECT id, password_hash, is_admin FROM users WHERE username = ?1",
                rusqlite::params![username],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        match row {
            Some((id, hash, is_admin)) => {
                if verify_password(password, &hash)? {
                    Ok(Some(UserInfo {
                        id: id as u32,
                        is_admin: is_admin != 0,
                    }))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }
}

fn hash_password(password: &str) -> Result<String, Error> {
    use rand_core::OsRng;
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| Error::Hash(e.to_string()))?
        .to_string();
    Ok(hash)
}

fn verify_password(password: &str, hash: &str) -> Result<bool, Error> {
    let parsed = PasswordHash::new(hash).map_err(|e| Error::Hash(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}
