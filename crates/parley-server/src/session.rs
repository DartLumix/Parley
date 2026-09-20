//! Connected-session registry: tracks who is online, broadcasts control
//! messages, and forwards media datagrams to specific connections.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

pub struct ClientHandle {
    pub user_id: u32,
    pub username: String,
    /// For media datagram forwarding.
    pub conn: quinn::Connection,
    /// Control-plane messages for this client (broadcasts).
    pub tx: mpsc::UnboundedSender<parley_proto::ServerMessage>,
    /// Registration generation (6c): unique per register() call. Guards
    /// a dying session's cleanup — if a same-user successor has
    /// registered, the entry (and the user's voice slot) belongs to it.
    /// Overwritten by register(); the 0 is a placeholder.
    pub session: u64,
}

#[derive(Default)]
pub struct SessionRegistry {
    clients: Mutex<HashMap<u32, ClientHandle>>,
    next_session: AtomicU64,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a client; returns its connection generation.
    pub fn register(&self, mut handle: ClientHandle) -> u64 {
        let session = self.next_session.fetch_add(1, Ordering::Relaxed);
        handle.session = session;
        self.clients.lock().unwrap().insert(handle.user_id, handle);
        session
    }

    /// Remove the user's entry ONLY if it still belongs to `session`.
    /// Returns true when it did (and was removed) — only then may the
    /// caller run session-scoped cleanup such as leaving voice.
    pub fn unregister_if(&self, user_id: u32, session: u64) -> bool {
        let mut clients = self.clients.lock().unwrap();
        let ours = clients.get(&user_id).is_some_and(|h| h.session == session);
        if ours {
            clients.remove(&user_id);
        }
        ours
    }

    /// Is `session` still the live registration for `user_id`?
    pub fn is_current(&self, user_id: u32, session: u64) -> bool {
        self.clients
            .lock()
            .unwrap()
            .get(&user_id)
            .is_some_and(|h| h.session == session)
    }

    /// Send a control-plane message to every connected client.
    pub fn broadcast(&self, msg: parley_proto::ServerMessage) {
        for handle in self.clients.lock().unwrap().values() {
            let _ = handle.tx.send(msg.clone());
        }
    }

    /// Send a control-plane message to one user (6b: member-targeted
    /// routing for channel-scoped notices). Returns false if they're
    /// offline (the send itself can't fail on an unbounded channel).
    pub fn send_to_user(&self, user_id: u32, msg: parley_proto::ServerMessage) -> bool {
        match self.clients.lock().unwrap().get(&user_id) {
            Some(h) => h.tx.send(msg).is_ok(),
            None => false,
        }
    }

    /// Send a media datagram to one user's connection.
    /// Returns false if they're offline or the send failed.
    pub fn send_datagram(&self, user_id: u32, data: bytes::Bytes) -> bool {
        match self.clients.lock().unwrap().get(&user_id) {
            Some(h) => h.conn.send_datagram(data).is_ok(),
            None => false,
        }
    }

    pub fn online_users(&self) -> Vec<(u32, String)> {
        self.clients
            .lock()
            .unwrap()
            .iter()
            .map(|(id, h)| (*id, h.username.clone()))
            .collect()
    }
}
