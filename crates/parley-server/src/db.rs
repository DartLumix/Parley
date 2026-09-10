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
    pub username: String,
    pub is_admin: bool,
}

/// (message_id, user_id, username, text, timestamp_ms)
pub type HistoryRow = (u64, u32, String, String, u64);

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
            );
            CREATE TABLE IF NOT EXISTS sessions (
                token TEXT PRIMARY KEY,
                user_id INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS channels (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                channel_id INTEGER NOT NULL,
                user_id INTEGER NOT NULL,
                text TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO channels (id, name) VALUES (1, 'general');
            ",
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
        let row: Option<(i64, String, String, i64)> = self
            .conn
            .query_row(
                "SELECT id, username, password_hash, is_admin FROM users WHERE username = ?1",
                rusqlite::params![username],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;

        match row {
            Some((id, name, hash, is_admin)) => {
                if verify_password(password, &hash)? {
                    Ok(Some(UserInfo {
                        id: id as u32,
                        username: name,
                        is_admin: is_admin != 0,
                    }))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    pub fn create_session(&self, user_id: u32) -> Result<String, Error> {
        use rand_core::{OsRng, RngCore};
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let token = hex::encode(bytes);
        self.conn.execute(
            "INSERT INTO sessions (token, user_id) VALUES (?1, ?2)",
            rusqlite::params![token, user_id as i64],
        )?;
        Ok(token)
    }

    pub fn verify_session_token(&self, token: &str) -> Result<Option<UserInfo>, Error> {
        let row: Option<(i64, String, i64)> = self
            .conn
            .query_row(
                "SELECT u.id, u.username, u.is_admin FROM sessions s
             JOIN users u ON u.id = s.user_id WHERE s.token = ?1",
                rusqlite::params![token],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match row {
            Some((id, name, is_admin)) => Ok(Some(UserInfo {
                id: id as u32,
                username: name,
                is_admin: is_admin != 0,
            })),
            None => Ok(None),
        }
    }

    pub fn list_channels(&self) -> Result<Vec<(u64, String)>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM channels ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn insert_message(
        &self,
        channel_id: u64,
        user_id: u32,
        text: &str,
    ) -> Result<(u64, u64, String), Error> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        self.conn.execute(
            "INSERT INTO messages (channel_id, user_id, text, timestamp_ms) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![channel_id as i64, user_id, text, ts],
        )?;
        let id = self.conn.last_insert_rowid() as u64;
        Ok((id, ts as u64, String::new()))
    }

    /// Fetch up to `limit` messages in `channel_id`, older than
    /// `before_message_id` (0 = newest). Returns oldest-first.
    pub fn fetch_history(
        &self,
        channel_id: u64,
        limit: u32,
        before_message_id: u64,
    ) -> Result<Vec<HistoryRow>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.user_id, u.username, m.text, m.timestamp_ms
             FROM messages m JOIN users u ON u.id = m.user_id
             WHERE m.channel_id = ?1 AND (?2 = 0 OR m.id < ?2)
             ORDER BY m.id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![channel_id as i64, before_message_id as i64, limit],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i32>(1)? as u32,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)? as u64,
                ))
            },
        )?;
        let mut out: Vec<_> = rows.filter_map(|r| r.ok()).collect();
        out.reverse(); // oldest first
        Ok(out)
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
