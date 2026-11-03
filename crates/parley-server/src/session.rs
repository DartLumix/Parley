//! Connected-session registry: tracks who is online, broadcasts control
//! messages, and forwards media datagrams to specific connections.
//!
//! One account may have several live devices. `session` identifies a
//! connection; `media_id` is the compact identity stamped into audio and
//! video datagrams, because a 32-bit account id cannot distinguish a phone
//! from a desktop. Presence remains one row per account, with a list of
//! device kinds.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use tokio::sync::mpsc;

/// (join, leave, afk) revisions for one account's personal sounds.
pub type SoundVersions = (u64, u64, u64);

pub struct ClientHandle {
    pub user_id: u32,
    pub username: String,
    /// Avatar revision as of registration, and after a profile update.
    pub avatar_version: u64,
    /// (join, leave, afk) personal-sound revisions.
    pub sound_versions: SoundVersions,
    /// Permission tier as of registration.
    pub role: crate::db::UserRole,
    /// For media datagram forwarding.
    pub conn: quinn::Connection,
    /// Control-plane messages for this client (broadcasts).
    pub tx: mpsc::UnboundedSender<parley_proto::ServerMessage>,
    /// Stable connection generation.
    pub session: u64,
    /// Compact identity used in media datagrams and voice reports.
    pub media_id: u32,
    /// DeviceKind wire value (Desktop / Mobile).
    pub device: i32,
    pub device_name: String,
}

/// One account's live facts, without any one connection's handles.
pub struct OnlineUser {
    pub id: u32,
    pub username: String,
    pub role: crate::db::UserRole,
    pub sound_versions: (u64, u64, u64),
    pub avatar_version: u64,
    pub devices: Vec<i32>,
}

struct RegistryState {
    clients: HashMap<u64, ClientHandle>,
    by_user: HashMap<u32, HashSet<u64>>,
    by_media: HashMap<u32, u64>,
    next_session: u64,
    next_media: u32,
}

impl Default for RegistryState {
    fn default() -> Self {
        Self {
            clients: HashMap::new(),
            by_user: HashMap::new(),
            by_media: HashMap::new(),
            // Zero is reserved as "unknown" in media headers.
            next_session: 1,
            next_media: 1,
        }
    }
}

#[derive(Default)]
pub struct SessionRegistry {
    state: Mutex<RegistryState>,
}

#[derive(Debug, Clone, Copy)]
pub struct Registered {
    pub session: u64,
    pub media: u32,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a client. Existing devices on the same account stay
    /// connected; this is the whole point of the multi-device model.
    pub fn register(&self, mut handle: ClientHandle) -> Registered {
        let mut state = self.state.lock().unwrap();
        // Both counters wrap. Skip ids that are still live rather than
        // hand two connections the same identity (the old media loop
        // compared the counter with the wrong variable and could spin
        // for the whole u32 range on a collision).
        let mut session = state.next_session.max(1);
        while state.clients.contains_key(&session) {
            session = session.wrapping_add(1).max(1);
        }
        state.next_session = session.wrapping_add(1).max(1);
        let mut media = state.next_media.max(1);
        while state.by_media.contains_key(&media) {
            media = media.wrapping_add(1).max(1);
        }
        state.next_media = media.wrapping_add(1).max(1);
        handle.session = session;
        handle.media_id = media;
        state
            .by_user
            .entry(handle.user_id)
            .or_default()
            .insert(session);
        state.by_media.insert(media, session);
        state.clients.insert(session, handle);
        Registered { session, media }
    }

    /// Remove one connection. Returns true when it was still registered.
    pub fn unregister_if(&self, user_id: u32, session: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(handle) = state.clients.remove(&session) else {
            return false;
        };
        if handle.user_id != user_id {
            // Put it back rather than corrupting the registry.
            state.clients.insert(session, handle);
            return false;
        }
        state.by_media.remove(&handle.media_id);
        if let Some(sessions) = state.by_user.get_mut(&user_id) {
            sessions.remove(&session);
            if sessions.is_empty() {
                state.by_user.remove(&user_id);
            }
        }
        true
    }

    pub fn is_registered(&self, user_id: u32, session: u64) -> bool {
        self.state
            .lock()
            .unwrap()
            .clients
            .get(&session)
            .is_some_and(|h| h.user_id == user_id)
    }

    /// Send a control-plane message to every connected device.
    pub fn broadcast(&self, msg: parley_proto::ServerMessage) {
        for handle in self.state.lock().unwrap().clients.values() {
            let _ = handle.tx.send(msg.clone());
        }
    }

    /// Voice state is device-local: send it to the originating device and
    /// to every OTHER account, but not to the same account's other
    /// devices. A phone joining must not make the desktop think its own
    /// voice session moved, and a phone leaving must not tear down the
    /// desktop's microphone. The account still has one presence row and
    /// one roster row, with both device badges.
    pub fn broadcast_voice(
        &self,
        user_id: u32,
        from_session: u64,
        msg: parley_proto::ServerMessage,
    ) {
        for handle in self.state.lock().unwrap().clients.values() {
            if handle.user_id != user_id || handle.session == from_session {
                let _ = handle.tx.send(msg.clone());
            }
        }
    }

    /// Management state goes to managers, excluding the request's own
    /// connection (its reply is written on its own control stream).
    pub fn broadcast_managers_except(&self, except_session: u64, msg: parley_proto::ServerMessage) {
        for handle in self.state.lock().unwrap().clients.values() {
            if handle.session != except_session && handle.role.can_manage() {
                let _ = handle.tx.send(msg.clone());
            }
        }
    }

    /// Close every live connection for an account (account removed or
    /// role changed).
    pub fn disconnect_user(&self, user_id: u32) {
        let conns = self.connections_for_user(user_id, None);
        for conn in conns {
            conn.close(3u32.into(), b"account changed");
        }
    }

    /// Disconnect every other device on this account, leaving the calling
    /// session alone. Tokens are deliberately untouched: those devices
    /// can reconnect without re-entering credentials.
    pub fn log_out_other_sessions(&self, user_id: u32, keep_session: u64) -> usize {
        let conns = self.connections_for_user(user_id, Some(keep_session));
        let count = conns.len();
        for conn in conns {
            conn.close(4u32.into(), b"logged out from another device");
        }
        count
    }

    fn connections_for_user(&self, user_id: u32, except: Option<u64>) -> Vec<quinn::Connection> {
        let state = self.state.lock().unwrap();
        let Some(sessions) = state.by_user.get(&user_id) else {
            return Vec::new();
        };
        sessions
            .iter()
            .filter(|session| Some(**session) != except)
            .filter_map(|session| state.clients.get(session))
            .map(|handle| handle.conn.clone())
            .collect()
    }

    /// Send a control-plane message to every device of one account.
    /// Returns false if they are offline.
    pub fn send_to_user(&self, user_id: u32, msg: parley_proto::ServerMessage) -> bool {
        let state = self.state.lock().unwrap();
        let Some(sessions) = state.by_user.get(&user_id) else {
            return false;
        };
        let mut sent = false;
        for session in sessions {
            if let Some(handle) = state.clients.get(session) {
                sent |= handle.tx.send(msg.clone()).is_ok();
            }
        }
        sent
    }

    /// Send a control-plane message to one device by its media id.
    /// Voice and video control describes one device, so routing it to
    /// every device on the account makes the other devices act on a
    /// report that was never about them. Returns false when the device
    /// is gone (it left between the roster read and this send).
    pub fn send_to_media(&self, media_id: u32, msg: parley_proto::ServerMessage) -> bool {
        let state = self.state.lock().unwrap();
        state
            .by_media
            .get(&media_id)
            .and_then(|session| state.clients.get(session))
            .map(|handle| handle.tx.send(msg).is_ok())
            .unwrap_or(false)
    }

    /// Send a media datagram to one device by its media id.
    pub fn send_datagram(&self, media_id: u32, data: bytes::Bytes) -> bool {
        let state = self.state.lock().unwrap();
        state
            .by_media
            .get(&media_id)
            .and_then(|session| state.clients.get(session))
            .map(|handle| handle.conn.send_datagram(data).is_ok())
            .unwrap_or(false)
    }

    /// Send one datagram to several media ids with one registry lock.
    pub fn send_datagram_many(&self, media_ids: &[u32], data: &bytes::Bytes) -> usize {
        let state = self.state.lock().unwrap();
        let conns: Vec<quinn::Connection> = media_ids
            .iter()
            .filter_map(|media| state.by_media.get(media))
            .filter_map(|session| state.clients.get(session))
            .map(|handle| handle.conn.clone())
            .collect();
        drop(state);
        let mut sent = 0;
        for conn in conns {
            if conn.send_datagram(data.clone()).is_ok() {
                sent += 1;
            }
        }
        sent
    }

    /// One row per account, with one entry per live device kind.
    pub fn online_users(&self) -> Vec<OnlineUser> {
        let state = self.state.lock().unwrap();
        let mut rows: HashMap<u32, OnlineUser> = HashMap::new();
        for handle in state.clients.values() {
            let row = rows.entry(handle.user_id).or_insert_with(|| OnlineUser {
                id: handle.user_id,
                username: handle.username.clone(),
                role: handle.role,
                sound_versions: handle.sound_versions,
                avatar_version: handle.avatar_version,
                devices: Vec::new(),
            });
            if !row.devices.contains(&handle.device) {
                row.devices.push(handle.device);
            }
        }
        let mut rows: Vec<_> = rows.into_values().collect();
        // Device badges are a set, but their order must not change
        // between polls just because HashMap iteration did.
        for row in &mut rows {
            row.devices.sort_unstable();
        }
        rows.sort_by_key(|row| row.id);
        rows
    }

    /// The live name for one account (chat stamps, blobs).
    pub fn username_of(&self, user_id: u32) -> Option<String> {
        let state = self.state.lock().unwrap();
        state.by_user.get(&user_id).and_then(|sessions| {
            sessions
                .iter()
                .find_map(|session| state.clients.get(session))
                .map(|handle| handle.username.clone())
        })
    }

    pub fn session_of_media(&self, media_id: u32) -> Option<u64> {
        self.state.lock().unwrap().by_media.get(&media_id).copied()
    }

    pub fn user_of_media(&self, media_id: u32) -> Option<u32> {
        let state = self.state.lock().unwrap();
        state
            .by_media
            .get(&media_id)
            .and_then(|session| state.clients.get(session))
            .map(|handle| handle.user_id)
    }

    pub fn session_count(&self, user_id: u32) -> usize {
        self.state
            .lock()
            .unwrap()
            .by_user
            .get(&user_id)
            .map(HashSet::len)
            .unwrap_or(0)
    }

    pub fn other_sessions(
        &self,
        user_id: u32,
        keep_session: u64,
    ) -> Vec<parley_proto::DeviceSession> {
        let state = self.state.lock().unwrap();
        let Some(sessions) = state.by_user.get(&user_id) else {
            return Vec::new();
        };
        sessions
            .iter()
            .filter(|session| **session != keep_session)
            .filter_map(|session| state.clients.get(session))
            .map(|handle| parley_proto::DeviceSession {
                session_id: handle.session,
                kind: handle.device,
                name: handle.device_name.clone(),
            })
            .collect()
    }

    /// Device kinds for a user, used by the presence row.
    pub fn device_kinds(&self, user_id: u32) -> Vec<i32> {
        let state = self.state.lock().unwrap();
        let Some(sessions) = state.by_user.get(&user_id) else {
            return Vec::new();
        };
        let mut kinds = Vec::new();
        for session in sessions {
            if let Some(handle) = state.clients.get(session)
                && !kinds.contains(&handle.device)
            {
                kinds.push(handle.device);
            }
        }
        kinds
    }

    /// Every media id belonging to an account, for account-wide status
    /// changes.
    pub fn media_ids_for_user(&self, user_id: u32) -> Vec<u32> {
        let state = self.state.lock().unwrap();
        state
            .by_user
            .get(&user_id)
            .map(|sessions| {
                sessions
                    .iter()
                    .filter_map(|session| state.clients.get(session))
                    .map(|handle| handle.media_id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One personal sound was replaced or removed: keep every live entry
    /// in step before the presence broadcast.
    pub fn update_sound_versions(&self, user_id: u32, versions: SoundVersions) {
        let mut state = self.state.lock().unwrap();
        let sessions: Vec<u64> = state
            .by_user
            .get(&user_id)
            .map(|sessions| sessions.iter().copied().collect())
            .unwrap_or_default();
        for session in sessions {
            if let Some(handle) = state.clients.get_mut(&session) {
                handle.sound_versions = versions;
            }
        }
    }

    /// A profile change landed; keep every live entry in step.
    pub fn update_identity(&self, user_id: u32, username: String, avatar_version: u64) {
        let mut state = self.state.lock().unwrap();
        let sessions: Vec<u64> = state
            .by_user
            .get(&user_id)
            .map(|sessions| sessions.iter().copied().collect())
            .unwrap_or_default();
        for session in sessions {
            if let Some(handle) = state.clients.get_mut(&session) {
                handle.username = username.clone();
                handle.avatar_version = avatar_version;
            }
        }
    }

    /// Connected users with their permission tier, for the HTTP presence
    /// endpoint. One row per account.
    pub fn roster(&self) -> Vec<OnlineUser> {
        self.online_users()
    }
}
