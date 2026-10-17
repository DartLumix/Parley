//! Voice channel membership state.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Default)]
pub struct VoiceManager {
    /// user_id -> channel they're currently in
    members: Mutex<HashMap<u32, u64>>,
    /// 7k: users with a live screen share, learned from their video
    /// datagrams and cleared on an explicit stop or when their
    /// membership dies. The teardown paths use it to decide whether a
    /// channel needs a ShareStopped notice — telling everyone that a
    /// user who never shared stopped sharing is protocol noise.
    sharing: Mutex<HashSet<u32>>,
}

impl VoiceManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Join (or switch) a channel. Returns the previous channel, if any.
    pub fn join(&self, user_id: u32, channel_id: u64) -> Option<u64> {
        self.members.lock().unwrap().insert(user_id, channel_id)
    }

    /// Leave voice entirely. Returns the channel they left, if any.
    pub fn leave(&self, user_id: u32) -> Option<u64> {
        self.members.lock().unwrap().remove(&user_id)
    }

    /// 7k: first video datagram of a share (the client's supervisor
    /// clears the flag again on stop/leave, so a re-share re-marks).
    pub fn mark_sharing(&self, user_id: u32) {
        self.sharing.lock().unwrap().insert(user_id);
    }

    /// 7k: forget the share. True when there was one to forget — the
    /// caller only notifies the room in that case.
    pub fn stop_sharing(&self, user_id: u32) -> bool {
        self.sharing.lock().unwrap().remove(&user_id)
    }

    pub fn channel_of(&self, user_id: u32) -> Option<u64> {
        self.members.lock().unwrap().get(&user_id).copied()
    }

    /// Users currently in `channel_id`.
    pub fn members_of(&self, channel_id: u64) -> Vec<u32> {
        self.members
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, ch)| **ch == channel_id)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Full state for snapshots.
    pub fn snapshot(&self) -> Vec<(u32, u64)> {
        self.members
            .lock()
            .unwrap()
            .iter()
            .map(|(&id, &ch)| (id, ch))
            .collect()
    }
}
