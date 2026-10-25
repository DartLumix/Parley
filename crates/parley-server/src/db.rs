//! User accounts: SQLite storage, Argon2id password hashing.
//!
//! 6c: all server access goes through `DbHandle` — a dedicated OS
//! thread owning the Connection (rusqlite is Send + !Sync: exactly one
//! owner, no Mutex, no poisoning, and blocking work — Argon2 verifies,
//! WAL checkpoints — never touches a tokio worker).

use std::path::Path;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::error::Error;

pub struct Db {
    conn: Connection,
}

/// Three permission tiers. `User` is the ordinary client; `Mod` can
/// manage users and channels but not roles; `Admin` can do everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserRole {
    User,
    Mod,
    Admin,
}

impl UserRole {
    pub fn from_i64(value: i64) -> Self {
        match value {
            2 => Self::Admin,
            1 => Self::Mod,
            _ => Self::User,
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            Self::User => 0,
            Self::Mod => 1,
            Self::Admin => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Mod => "mod",
            Self::Admin => "admin",
        }
    }

    /// Admin or mod: access to the management page and its non-role
    /// actions.
    pub fn can_manage(self) -> bool {
        !matches!(self, Self::User)
    }

    pub fn is_admin(self) -> bool {
        matches!(self, Self::Admin)
    }
}

pub struct UserInfo {
    pub id: u32,
    pub username: String,
    pub role: UserRole,
}

impl UserInfo {
    pub fn is_admin(&self) -> bool {
        self.role.is_admin()
    }
}

/// One row of the management users table (the DB half; online and voice
/// membership are live session state and are added by the server).
#[derive(Debug, Clone)]
pub struct ManagedUserRow {
    pub id: u32,
    pub username: String,
    pub role: UserRole,
}

/// (message_id, user_id, username, text, timestamp_ms)
pub type HistoryRow = (u64, u32, String, String, u64);
/// (id, name, music) per channel (6d).
pub type ChannelRow = (u64, String, bool);

impl Db {
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Seed the default channel only when the schema is created. The
        // management page can delete `general`; re-running the seed on
        // every open would resurrect it at the next restart.
        let first_boot = {
            let mut stmt = conn
                .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'users'")?;
            !stmt.exists([])?
        };
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                id INTEGER PRIMARY KEY,
                username TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL,
                is_admin INTEGER NOT NULL DEFAULT 0,
                role INTEGER NOT NULL DEFAULT 0,
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
            ",
        )?;
        if first_boot {
            conn.execute(
                "INSERT OR IGNORE INTO channels (id, name) VALUES (1, 'general')",
                [],
            )?;
        }
        // 6d migration: channels.mode (0 = speech, 1 = music). CREATE
        // IF NOT EXISTS won't touch an existing table, so ALTER is the
        // path — guarded by a pragma_table_info check so re-running is
        // a no-op.
        let has_mode: bool = {
            let mut stmt =
                conn.prepare("SELECT 1 FROM pragma_table_info('channels') WHERE name = 'mode'")?;
            stmt.exists([])?
        };
        if !has_mode {
            conn.execute_batch("ALTER TABLE channels ADD COLUMN mode INTEGER NOT NULL DEFAULT 0;")?;
        }
        // Role migration: older databases only have the is_admin boolean.
        // Admins keep their powers; everyone else starts as a user.
        let has_role: bool = {
            let mut stmt =
                conn.prepare("SELECT 1 FROM pragma_table_info('users') WHERE name = 'role'")?;
            stmt.exists([])?
        };
        if !has_role {
            conn.execute_batch(
                "ALTER TABLE users ADD COLUMN role INTEGER NOT NULL DEFAULT 0;
                 UPDATE users SET role = 2 WHERE is_admin <> 0;",
            )?;
        }
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

    /// GC old session tokens (B-bucket): the sessions table grows one
    /// row per login forever otherwise. 30 days is generous — tokens
    /// are re-issued on every password login, so losing an old one
    /// costs a password prompt at worst.
    pub fn purge_stale_sessions(&self) -> Result<(), Error> {
        let n = self.conn.execute(
            "DELETE FROM sessions WHERE created_at < datetime('now', '-30 days')",
            [],
        )?;
        if n > 0 {
            info!("purged {n} stale session tokens");
        }
        Ok(())
    }

    /// Compatibility entry point for the CLI and bootstrap: a boolean
    /// admin is still a valid way to create an account.
    pub fn create_user(&self, username: &str, password: &str, is_admin: bool) -> Result<(), Error> {
        let role = if is_admin {
            UserRole::Admin
        } else {
            UserRole::User
        };
        self.create_user_with_role(username, password, role)
    }

    pub fn create_user_with_role(
        &self,
        username: &str,
        password: &str,
        role: UserRole,
    ) -> Result<(), Error> {
        let hash = hash_password(password)?;
        self.conn.execute(
            "INSERT INTO users (username, password_hash, is_admin, role) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![username, hash, role.is_admin() as i32, role.as_i64()],
        )?;
        Ok(())
    }

    /// Create a channel, returning its id (6b/6d). `music` selects the
    /// stereo/256 kbps profile. No control-plane caller yet — tests
    /// and future tooling use this.
    pub fn create_channel(&self, name: &str, music: bool) -> Result<u64, Error> {
        self.conn.execute(
            "INSERT INTO channels (name, mode) VALUES (?1, ?2)",
            rusqlite::params![name, music as i32],
        )?;
        Ok(self.conn.last_insert_rowid() as u64)
    }

    pub fn update_channel(
        &self,
        channel_id: u64,
        name: Option<&str>,
        music: Option<bool>,
    ) -> Result<(), Error> {
        let current: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT name, mode FROM channels WHERE id = ?1",
                rusqlite::params![channel_id as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((old_name, old_mode)) = current else {
            return Err(Error::Internal("channel not found".into()));
        };
        let name = name.unwrap_or(&old_name);
        let music = music.unwrap_or(old_mode != 0);
        self.conn.execute(
            "UPDATE channels SET name = ?1, mode = ?2 WHERE id = ?3",
            rusqlite::params![name, music as i32, channel_id as i64],
        )?;
        Ok(())
    }

    /// Remove a channel and its chat history. Voice membership is live
    /// server state, not a DB row — the caller must empty the channel
    /// first.
    pub fn delete_channel(&self, channel_id: u64) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "DELETE FROM channels WHERE id = ?1",
            rusqlite::params![channel_id as i64],
        )?;
        if changed == 0 {
            return Err(Error::Internal("channel not found".into()));
        }
        tx.execute(
            "DELETE FROM messages WHERE channel_id = ?1",
            rusqlite::params![channel_id as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Returns Some(UserInfo) on success, None on bad credentials.
    pub fn verify_user(&self, username: &str, password: &str) -> Result<Option<UserInfo>, Error> {
        let row: Option<(i64, String, String, i64)> = self
            .conn
            .query_row(
                "SELECT id, username, password_hash, role FROM users WHERE username = ?1",
                rusqlite::params![username],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;

        match row {
            Some((id, name, hash, role)) => {
                if verify_password(password, &hash)? {
                    Ok(Some(UserInfo {
                        id: id as u32,
                        username: name,
                        role: UserRole::from_i64(role),
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
                "SELECT u.id, u.username, u.role FROM sessions s
             JOIN users u ON u.id = s.user_id WHERE s.token = ?1",
                rusqlite::params![token],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match row {
            Some((id, name, role)) => Ok(Some(UserInfo {
                id: id as u32,
                username: name,
                role: UserRole::from_i64(role),
            })),
            None => Ok(None),
        }
    }

    /// Every account, for the management page.
    pub fn list_users(&self) -> Result<Vec<ManagedUserRow>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, username, role FROM users ORDER BY username COLLATE NOCASE, id")?;
        let rows = stmt.query_map([], |row| {
            Ok(ManagedUserRow {
                id: row.get::<_, i64>(0)? as u32,
                username: row.get(1)?,
                role: UserRole::from_i64(row.get(2)?),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn get_user(&self, user_id: u32) -> Result<Option<ManagedUserRow>, Error> {
        let row = self
            .conn
            .query_row(
                "SELECT id, username, role FROM users WHERE id = ?1",
                rusqlite::params![user_id as i64],
                |row| {
                    Ok(ManagedUserRow {
                        id: row.get::<_, i64>(0)? as u32,
                        username: row.get(1)?,
                        role: UserRole::from_i64(row.get(2)?),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Apply only the fields that were provided. The role guard runs in
    /// the same statement as the change, so two admins demoting each other
    /// cannot both see "there is another admin" and leave the server with
    /// none.
    pub fn update_user(
        &self,
        user_id: u32,
        username: Option<&str>,
        password: Option<&str>,
        role: Option<UserRole>,
    ) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction()?;
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM users WHERE id = ?1",
                rusqlite::params![user_id as i64],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(Error::Internal("user not found".into()));
        }
        if let Some(name) = username {
            tx.execute(
                "UPDATE users SET username = ?1 WHERE id = ?2",
                rusqlite::params![name, user_id as i64],
            )?;
        }
        if let Some(password) = password {
            let hash = hash_password(password)?;
            tx.execute(
                "UPDATE users SET password_hash = ?1 WHERE id = ?2",
                rusqlite::params![hash, user_id as i64],
            )?;
        }
        if let Some(role) = role {
            let changed = tx.execute(
                "UPDATE users SET role = ?1, is_admin = ?2 WHERE id = ?3
                 AND (?1 = 2 OR role <> 2
                      OR (SELECT COUNT(*) FROM users WHERE role = 2) > 1)",
                rusqlite::params![role.as_i64(), role.is_admin() as i32, user_id as i64],
            )?;
            if changed == 0 {
                return Err(Error::Internal("cannot demote the last admin".into()));
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete an account and its session tokens. The last admin cannot be
    /// deleted (same guard shape as the role update). Chat rows stay; the
    /// history join simply stops naming the account.
    pub fn delete_user(&self, user_id: u32) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "DELETE FROM users WHERE id = ?1
             AND (role <> 2 OR (SELECT COUNT(*) FROM users WHERE role = 2) > 1)",
            rusqlite::params![user_id as i64],
        )?;
        if changed == 0 {
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM users WHERE id = ?1",
                    rusqlite::params![user_id as i64],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            return Err(Error::Internal(if exists {
                "cannot remove the last admin".into()
            } else {
                "user not found".into()
            }));
        }
        tx.execute(
            "DELETE FROM sessions WHERE user_id = ?1",
            rusqlite::params![user_id as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_channels(&self) -> Result<Vec<ChannelRow>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, mode FROM channels ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
            ))
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
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
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

    pub fn channel_exists(&self, channel_id: u64) -> Result<bool, Error> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM channels WHERE id = ?1",
                rusqlite::params![channel_id as i64],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Actor dispatch: run one request against the owned Connection
    /// and send the reply. Private — only the actor thread calls this.
    fn dispatch(&mut self, cmd: DbCmd) {
        match cmd {
            DbCmd::VerifyUser {
                username,
                password,
                reply,
            } => {
                let _ = reply.send(self.verify_user(&username, &password));
            }
            DbCmd::VerifyToken { token, reply } => {
                let _ = reply.send(self.verify_session_token(&token));
            }
            DbCmd::CreateSession { user_id, reply } => {
                let _ = reply.send(self.create_session(user_id));
            }
            DbCmd::ListChannels { reply } => {
                let _ = reply.send(self.list_channels());
            }
            DbCmd::ChannelExists { channel_id, reply } => {
                let _ = reply.send(self.channel_exists(channel_id));
            }
            DbCmd::InsertMessage {
                channel_id,
                user_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.insert_message(channel_id, user_id, &text));
            }
            DbCmd::FetchHistory {
                channel_id,
                limit,
                before_message_id,
                reply,
            } => {
                let _ = reply.send(self.fetch_history(channel_id, limit, before_message_id));
            }
            DbCmd::ListUsers { reply } => {
                let _ = reply.send(self.list_users());
            }
            DbCmd::GetUser { user_id, reply } => {
                let _ = reply.send(self.get_user(user_id));
            }
            DbCmd::CreateUser {
                username,
                password,
                role,
                reply,
            } => {
                let _ = reply.send(self.create_user_with_role(&username, &password, role));
            }
            DbCmd::UpdateUser {
                user_id,
                username,
                password,
                role,
                reply,
            } => {
                let _ = reply.send(self.update_user(
                    user_id,
                    username.as_deref(),
                    password.as_deref(),
                    role,
                ));
            }
            DbCmd::DeleteUser { user_id, reply } => {
                let _ = reply.send(self.delete_user(user_id));
            }
            DbCmd::CreateChannel { name, music, reply } => {
                let _ = reply.send(self.create_channel(&name, music));
            }
            DbCmd::UpdateChannel {
                channel_id,
                name,
                music,
                reply,
            } => {
                let _ = reply.send(self.update_channel(channel_id, name.as_deref(), music));
            }
            DbCmd::DeleteChannel { channel_id, reply } => {
                let _ = reply.send(self.delete_channel(channel_id));
            }
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

// ---- db actor (6c) ----

/// One request to the actor thread. Every variant carries its reply
/// oneshot.
pub enum DbCmd {
    VerifyUser {
        username: String,
        password: String,
        reply: oneshot::Sender<Result<Option<UserInfo>, Error>>,
    },
    VerifyToken {
        token: String,
        reply: oneshot::Sender<Result<Option<UserInfo>, Error>>,
    },
    CreateSession {
        user_id: u32,
        reply: oneshot::Sender<Result<String, Error>>,
    },
    ListChannels {
        reply: oneshot::Sender<Result<Vec<ChannelRow>, Error>>,
    },
    ChannelExists {
        channel_id: u64,
        reply: oneshot::Sender<Result<bool, Error>>,
    },
    InsertMessage {
        channel_id: u64,
        user_id: u32,
        text: String,
        reply: oneshot::Sender<Result<(u64, u64, String), Error>>,
    },
    FetchHistory {
        channel_id: u64,
        limit: u32,
        before_message_id: u64,
        reply: oneshot::Sender<Result<Vec<HistoryRow>, Error>>,
    },
    ListUsers {
        reply: oneshot::Sender<Result<Vec<ManagedUserRow>, Error>>,
    },
    GetUser {
        user_id: u32,
        reply: oneshot::Sender<Result<Option<ManagedUserRow>, Error>>,
    },
    CreateUser {
        username: String,
        password: String,
        role: UserRole,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    UpdateUser {
        user_id: u32,
        username: Option<String>,
        password: Option<String>,
        role: Option<UserRole>,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    DeleteUser {
        user_id: u32,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    CreateChannel {
        name: String,
        music: bool,
        reply: oneshot::Sender<Result<u64, Error>>,
    },
    UpdateChannel {
        channel_id: u64,
        name: Option<String>,
        music: Option<bool>,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    DeleteChannel {
        channel_id: u64,
        reply: oneshot::Sender<Result<(), Error>>,
    },
}

fn gone() -> Error {
    Error::Internal("db actor is gone".into())
}

/// Cloneable handle to the db actor thread.
#[derive(Clone)]
pub struct DbHandle {
    tx: mpsc::UnboundedSender<DbCmd>,
}

impl DbHandle {
    /// Start the actor on its own OS thread; the Connection moves in.
    /// The thread exits when the last handle drops.
    pub fn spawn(db: Db) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<DbCmd>();
        std::thread::Builder::new()
            .name("parley-db".to_string())
            .spawn(move || {
                let mut db = db;
                while let Some(cmd) = rx.blocking_recv() {
                    db.dispatch(cmd);
                }
            })
            .expect("failed to spawn db actor thread");
        Self { tx }
    }

    pub async fn verify_user(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<UserInfo>, Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::VerifyUser {
                username: username.to_string(),
                password: password.to_string(),
                reply,
            })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn verify_token(&self, token: &str) -> Result<Option<UserInfo>, Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::VerifyToken {
                token: token.to_string(),
                reply,
            })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn create_session(&self, user_id: u32) -> Result<String, Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::CreateSession { user_id, reply })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn list_channels(&self) -> Result<Vec<ChannelRow>, Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::ListChannels { reply })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn channel_exists(&self, channel_id: u64) -> Result<bool, Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::ChannelExists { channel_id, reply })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn insert_message(
        &self,
        channel_id: u64,
        user_id: u32,
        text: &str,
    ) -> Result<(u64, u64, String), Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::InsertMessage {
                channel_id,
                user_id,
                text: text.to_string(),
                reply,
            })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn fetch_history(
        &self,
        channel_id: u64,
        limit: u32,
        before_message_id: u64,
    ) -> Result<Vec<HistoryRow>, Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::FetchHistory {
                channel_id,
                limit,
                before_message_id,
                reply,
            })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn list_users(&self) -> Result<Vec<ManagedUserRow>, Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::ListUsers { reply })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn get_user(&self, user_id: u32) -> Result<Option<ManagedUserRow>, Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::GetUser { user_id, reply })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn create_user_with_role(
        &self,
        username: &str,
        password: &str,
        role: UserRole,
    ) -> Result<(), Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::CreateUser {
                username: username.to_string(),
                password: password.to_string(),
                role,
                reply,
            })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn update_user(
        &self,
        user_id: u32,
        username: Option<&str>,
        password: Option<&str>,
        role: Option<UserRole>,
    ) -> Result<(), Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::UpdateUser {
                user_id,
                username: username.map(str::to_string),
                password: password.map(str::to_string),
                role,
                reply,
            })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn delete_user(&self, user_id: u32) -> Result<(), Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::DeleteUser { user_id, reply })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn create_channel(&self, name: &str, music: bool) -> Result<u64, Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::CreateChannel {
                name: name.to_string(),
                music,
                reply,
            })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn update_channel(
        &self,
        channel_id: u64,
        name: Option<&str>,
        music: Option<bool>,
    ) -> Result<(), Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::UpdateChannel {
                channel_id,
                name: name.map(str::to_string),
                music,
                reply,
            })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn delete_channel(&self, channel_id: u64) -> Result<(), Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCmd::DeleteChannel { channel_id, reply })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_is_seeded_once_not_resurrected() {
        let dir = std::env::temp_dir().join(format!("parley-db-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("parley.db");

        {
            let db = Db::open(&path).unwrap();
            assert!(
                db.list_channels().unwrap().iter().any(|c| c.0 == 1),
                "a new database seeds general"
            );
            db.delete_channel(1).unwrap();
        }
        {
            let db = Db::open(&path).unwrap();
            assert!(
                !db.list_channels().unwrap().iter().any(|c| c.0 == 1),
                "reopening must not resurrect a deleted channel"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
