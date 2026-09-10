//! Connected-session registry: tracks who is online and lets the
//! server broadcast chat messages to every connected client.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;

pub struct ClientHandle {
    pub user_id: u32,
    pub username: String,
    /// Channel for sending messages to this client's session loop.
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

    /// Send a message to every connected client. Dead sends are
    /// dropped silently (session loop cleans up on its own).
    pub fn broadcast(&self, msg: parley_proto::ServerMessage) {
        for handle in self.clients.lock().unwrap().values() {
            let _ = handle.tx.send(msg.clone());
        }
    }

    /// usernames of online users.
    pub fn online_users(&self) -> Vec<(u32, String)> {
        self.clients
            .lock()
            .unwrap()
            .iter()
            .map(|(id, h)| (*id, h.username.clone()))
            .collect()
    }
}
