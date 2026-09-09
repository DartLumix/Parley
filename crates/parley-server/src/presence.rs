//! Online user tracking.

use std::collections::HashMap;
use std::sync::Mutex;

pub struct PresenceManager {
    users: Mutex<HashMap<u32, String>>,
}

impl PresenceManager {
    pub fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
        }
    }

    pub fn add_user(&self, id: u32, username: String) {
        self.users.lock().unwrap().insert(id, username);
    }

    pub fn remove_user(&self, id: u32) {
        self.users.lock().unwrap().remove(&id);
    }

    pub fn online_users(&self) -> Vec<(u32, String)> {
        self.users
            .lock()
            .unwrap()
            .iter()
            .map(|(id, name)| (*id, name.clone()))
            .collect()
    }
}

impl Default for PresenceManager {
    fn default() -> Self {
        Self::new()
    }
}
