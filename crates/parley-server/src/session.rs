//! Connected-session registry: tracks who is online, broadcasts control
//! messages, and forwards media datagrams to specific connections.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;

pub struct ClientHandle {
    pub user_id: u32,
    pub username: String,
    /// For media datagram forwarding.
    pub conn: quinn::Connection,
    /// Control-plane messages for this client (broadcasts).
    pub tx: mpsc::UnboundedSender<parley_proto::ServerMessage>,
}

#[derive(Default)]
pub struct SessionRegistry {
    clients: Mutex<HashMap<u32, ClientHandle>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, handle: ClientHandle) {
        self.clients.lock().unwrap().insert(handle.user_id, handle);
    }

    pub fn unregister(&self, user_id: u32) {
        self.clients.lock().unwrap().remove(&user_id);
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
