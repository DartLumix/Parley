//! Voice channel membership state.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Away texts are roster decoration, not a message: cap them so one
/// client cannot push unbounded data to every session with every
/// keystroke.
pub const MAX_STATUS_CHARS: usize = 120;

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
    /// Mic-muted users, announced by SetMute. A preference like the
    /// client's own flag: it survives leave/join (a muted user stays
    /// muted when switching channels) and only ever describes a client
    /// that told us — silence is never inferred from an empty mixer.
    muted: Mutex<HashSet<u32>>,
    /// Away texts (SetStatus), same lifetime rules as `muted`: kept per
    /// user across channel switches, dropped when the session ends.
    statuses: Mutex<HashMap<u32, String>>,
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

    /// Record a client's mic-mute announcement. Returns true when the
    /// value actually changed (the caller only broadcasts real news).
    pub fn set_muted(&self, user_id: u32, muted: bool) -> bool {
        if muted {
            self.muted.lock().unwrap().insert(user_id)
        } else {
            self.muted.lock().unwrap().remove(&user_id)
        }
    }

    pub fn is_muted(&self, user_id: u32) -> bool {
        self.muted.lock().unwrap().contains(&user_id)
    }

    /// Drop a departed session's mute preference. Mute survives
    /// leave/join WITHIN a session (the client keeps its flag), but not
    /// the session itself: a fresh app start is unmuted, so the server
    /// must not remember an old one and disagree with it.
    pub fn clear_muted(&self, user_id: u32) {
        self.muted.lock().unwrap().remove(&user_id);
    }

    /// Record an away status. Returns true when the text actually
    /// changed — the caller only broadcasts real news. Text is trimmed
    /// and capped; empty clears the status.
    pub fn set_status(&self, user_id: u32, text: &str) -> bool {
        let text = text.trim();
        let mut statuses = self.statuses.lock().unwrap();
        if text.is_empty() {
            return statuses.remove(&user_id).is_some();
        }
        let capped: String = text.chars().take(MAX_STATUS_CHARS).collect();
        if statuses.get(&user_id) == Some(&capped) {
            return false;
        }
        statuses.insert(user_id, capped);
        true
    }

    /// The user's away text, or empty when there is none.
    pub fn status_of(&self, user_id: u32) -> String {
        self.statuses
            .lock()
            .unwrap()
            .get(&user_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Session end: a new login starts with no status.
    pub fn clear_status(&self, user_id: u32) {
        self.statuses.lock().unwrap().remove(&user_id);
    }

    /// Full state for snapshots: (user_id, channel_id, muted, status).
    pub fn snapshot(&self) -> Vec<(u32, u64, bool, String)> {
        let muted = self.muted.lock().unwrap();
        let statuses = self.statuses.lock().unwrap();
        self.members
            .lock()
            .unwrap()
            .iter()
            .map(|(&id, &ch)| {
                (
                    id,
                    ch,
                    muted.contains(&id),
                    statuses.get(&id).cloned().unwrap_or_default(),
                )
            })
            .collect()
    }
}
